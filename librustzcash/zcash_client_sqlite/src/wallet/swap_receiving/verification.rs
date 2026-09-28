//! Address-exposure evidence is independent of recovery closeout checkpoints.
use super::{Error, corrupt, private::has_gap, reservations::reservation_key};
use crate::{AccountUuid, WalletDb, wallet};
use orchard::{
    keys::Scope,
    note_encryption::{CompactAction, IronwoodDomain},
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    borrow::{Borrow, BorrowMut},
    ops::Range,
};
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_note_encryption::try_compact_note_decryption;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

/// Maximum missing recent history that address preparation may check locally.
/// Restore and final closeout still require their full, fixed coverage targets.
pub const RECEIVE_VERIFICATION_MAX_LAG: u32 = 5;

fn coverage_error() -> Error {
    Error::ReservationPolicy(
        "SWAP_RECEIVE_COVERAGE: Receive-address verification is waiting for complete coverage. Try again shortly.",
    )
}

fn target<P: Parameters>(conn: &Connection, params: &P) -> Result<ChainAnchor, Error> {
    let block = wallet::block_fully_scanned(conn, params)?.ok_or_else(coverage_error)?;
    Ok(ChainAnchor {
        height: block.block_height(),
        hash: block.block_hash().0,
    })
}

fn available(conn: &Connection, key: i64) -> Result<bool, Error> {
    Ok(!conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_swap_receive_used WHERE receiving_key_id=?1)
        OR EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery WHERE receiving_key_id=?1)",
        [key],
        |r| r.get::<_, bool>(0),
    )?)
}

fn covered(
    conn: &Connection,
    key: i64,
    from: BlockHeight,
    through: BlockHeight,
) -> Result<bool, Error> {
    let mut stmt = conn.prepare(
        "SELECT range_start,range_end FROM ironwood_receiving_key_scan_ranges
        WHERE receiving_key_id=?1 ORDER BY range_start",
    )?;
    let ranges = stmt
        .query_map([key], |r| Ok(r.get::<_, u32>(0)?..r.get::<_, u32>(1)?))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(!has_gap(0, through.into(), Some(from.into()), ranges))
}

/// Must be rechecked in the same transaction that authorizes exposing the address.
pub(super) fn verified<P: Parameters>(
    conn: &Connection,
    params: &P,
    key: i64,
) -> Result<Option<ChainAnchor>, Error> {
    if !available(conn, key)? {
        return Ok(None);
    }
    let saved: Option<ChainAnchor> = conn
        .query_row(
            "SELECT height,block_hash FROM ironwood_swap_receive_checks WHERE receiving_key_id=?1",
            [key],
            |r| {
                Ok(ChainAnchor {
                    height: BlockHeight::from(r.get::<_, u32>(0)?),
                    hash: r.get(1)?,
                })
            },
        )
        .optional()?;
    let Some(saved) = saved else {
        return Ok(None);
    };
    let through = target(conn, params)?;
    if saved.height > through.height
        || wallet::get_block_hash(conn, saved.height)? != Some(BlockHash(saved.hash))
    {
        return Ok(None);
    }
    Ok(covered(conn, key, saved.height, through.height)?.then_some(through))
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Reuses an empty historical check only with continuous per-key coverage to the
    /// current scan frontier. A globally synced wallet does not establish this.
    pub fn verified_swap_receive_reservation(
        &self,
        account: AccountUuid,
        reservation: i64,
    ) -> Result<Option<ChainAnchor>, Error> {
        let key = reservation_key(self.conn.borrow(), account, reservation)?;
        verified(self.conn.borrow(), &self.params, key)
    }

    /// Plans a bounded compact-block check after a complete empty PIR response.
    /// The publication must be canonical and no more than five blocks behind the
    /// current scan frontier, even when some local coverage is already available.
    pub fn swap_receive_verification_tail(
        &self,
        account: AccountUuid,
        reservation: i64,
        history: ChainAnchor,
    ) -> Result<Option<Range<BlockHeight>>, Error> {
        let conn = self.conn.borrow();
        let key = reservation_key(conn, account, reservation)?;
        let through = target(conn, &self.params)?;
        if !available(conn, key)? {
            return Err(Error::ReservationPolicy(
                "SWAP_RECEIVE_STALE: This address already has a payment. Request a new quote.",
            ));
        }
        if history.height > through.height
            || through.height - history.height > RECEIVE_VERIFICATION_MAX_LAG
            || wallet::get_block_hash(conn, history.height)? != Some(BlockHash(history.hash))
        {
            return Err(coverage_error());
        }
        if covered(conn, key, history.height, through.height)? {
            return Ok(None);
        }
        let end = u32::from(through.height)
            .checked_add(1)
            .ok_or_else(coverage_error)?;
        Ok(Some(history.height + 1..BlockHeight::from(end)))
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Persists complete empty-address verification after a caller-validated empty
    /// PIR response. `blocks` must contain the entire planned tail, not a sampled
    /// subset. This uses the same trusted compact-block source as ordinary scanning.
    /// A discovered receipt returns false, permanently excludes the address, and
    /// requests private recovery without crediting an unauthenticated balance.
    pub fn verify_swap_receive_history(
        &mut self,
        account: AccountUuid,
        reservation: i64,
        history: ChainAnchor,
        blocks: &[CompactBlock],
    ) -> Result<bool, Error> {
        self.transactionally(|db| {
            let key = reservation_key(db.conn.0, account, reservation)?;
            let missing = db.swap_receive_verification_tail(account, reservation, history)?;
            let through = target(db.conn.0, &db.params)?;
            let paid = if let Some(range) = missing {
                let reserved = db.swap_receive_reservation(account, reservation)?;
                check_tail(db.conn.0, &reserved.key, history, range, blocks)?
            } else {
                // A concurrent scan may have covered the downloaded range. Retry with fresh state.
                if !blocks.is_empty() {
                    return Err(coverage_error());
                }
                false
            };
            if paid {
                db.conn.0.execute(
                    "INSERT OR IGNORE INTO ironwood_swap_receive_used(receiving_key_id) VALUES (?1)",
                    [key],
                )?;
                super::reservations::request_recheck(db.conn.0, key, through)?;
                return Ok(false);
            }
            db.conn.0.execute(
                "INSERT INTO ironwood_swap_receive_checks(receiving_key_id,height,block_hash)
                 VALUES (?1,?2,?3) ON CONFLICT(receiving_key_id)
                 DO UPDATE SET height=excluded.height,block_hash=excluded.block_hash",
                params![key, u32::from(through.height), through.hash],
            )?;
            Ok(true)
        })
    }
}

fn tree_size(conn: &Connection, height: BlockHeight) -> Result<u32, Error> {
    Ok(conn.query_row(
        "SELECT ironwood_commitment_tree_size FROM blocks WHERE height=?1",
        [u32::from(height)],
        |r| r.get(0),
    )?)
}

// A targeted check must not create normal scan coverage: it does not store notes or
// witnesses. Only an empty result can advance the separate address-verification record.
fn check_tail(
    conn: &Connection,
    key: &super::RegisteredKey,
    history: ChainAnchor,
    range: Range<BlockHeight>,
    blocks: &[CompactBlock],
) -> Result<bool, Error> {
    if blocks.len() != usize::try_from(range.end - range.start).map_err(|_| coverage_error())? {
        return Err(coverage_error());
    }
    let ivk = key.full_viewing_key().to_ivk(Scope::External).prepare();
    let mut previous = history.hash;
    let mut size = tree_size(conn, history.height)?;
    let mut paid = false;
    for (height, block) in (u32::from(range.start)..u32::from(range.end)).zip(blocks) {
        let canonical =
            wallet::get_block_hash(conn, BlockHeight::from(height))?.ok_or_else(coverage_error)?;
        let expected_size = tree_size(conn, BlockHeight::from(height))?;
        let count: usize = block.vtx.iter().map(|t| t.ironwood_actions.len()).sum();
        if block.height != u64::from(height)
            || block.hash.as_slice() != canonical.0
            || block.prev_hash.as_slice() != previous
            || size.checked_add(u32::try_from(count).map_err(|_| coverage_error())?)
                != Some(expected_size)
            || block
                .chain_metadata
                .map(|m| m.ironwood_commitment_tree_size)
                != Some(expected_size)
        {
            return Err(coverage_error());
        }
        for action in block.vtx.iter().flat_map(|t| &t.ironwood_actions) {
            let compact = CompactAction::try_from(action).map_err(|e| corrupt(&e.to_string()))?;
            if let Some((_, recipient)) = try_compact_note_decryption(
                &IronwoodDomain::for_compact_action(&compact),
                &ivk,
                &compact,
            ) {
                paid |= recipient == key.receiver();
            }
        }
        size = expected_size;
        previous = canonical.0;
    }
    Ok(paid)
}
