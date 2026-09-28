//! Durable opt-in for private historical discovery. No targeted replay fallback.
use super::{Error, KeyId, account_key, corrupt, payments::key_ref};
use crate::{AccountUuid, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Whether historical discovery still has a gap through the requested height.
    /// Canonical directory coverage and ranges scanned with this key are combined;
    /// a newer directory publication alone does not invalidate completed recovery.
    pub fn swap_receiving_needs_discovery(
        &self,
        account: AccountUuid,
        key: KeyId,
        through: BlockHeight,
    ) -> Result<bool, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let start: u32 = conn.query_row(
            "SELECT scan_from FROM ironwood_receiving_keys WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if !self.pending_swap_payments(account, key)?.is_empty() {
            return Ok(true);
        }
        let checked = self.swap_directory_check(account, key)?;
        let ranges = self
            .get_swap_receiving_scan_ranges(account, key)?
            .unwrap_or_default();
        Ok(has_gap(
            start,
            u32::from(through),
            checked.map(|a| u32::from(a.height)),
            ranges
                .into_iter()
                .map(|r| u32::from(r.start)..u32::from(r.end)),
        ))
    }

    /// Last fully processed directory publication for this key, if still canonical.
    pub fn swap_directory_check(
        &self,
        account: AccountUuid,
        key: KeyId,
    ) -> Result<Option<ChainAnchor>, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let anchor = conn.query_row("SELECT height,block_hash FROM ironwood_swap_directory_checks WHERE receiving_key_id=?1", [id], |r| Ok(ChainAnchor {height:BlockHeight::from(r.get::<_,u32>(0)?),hash:r.get(1)?})).optional()?;
        match anchor {
            Some(a) if wallet::get_block_hash(conn, a.height)? == Some(BlockHash(a.hash)) => {
                Ok(Some(a))
            }
            _ => Ok(None),
        }
    }
}
impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Opt in before the first scan. Historical key gaps then use the directory,
    /// while ordinary scanning retains spend evidence for late note insertion.
    /// Enabling later cannot recreate already pruned evidence or remove queued scans.
    /// This POC policy deliberately retains the shared nullifier map without pruning.
    pub fn enable_private_swap_recovery(&mut self, account: AccountUuid) -> Result<(), Error> {
        self.transactionally(|db| {
            let (id, _) = account_key(db.conn.0, &db.params, account)?;
            db.conn.0.execute(
                "INSERT OR IGNORE INTO ironwood_swap_private_recovery(account_id) VALUES (?1)",
                [id.0],
            )?;
            Ok(())
        })
    }
    /// Records completed processing only with no pending candidates and a local anchor.
    pub fn mark_swap_directory_checked(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainAnchor,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id=key_ref(db.conn.0,account,key)?;
            if wallet::get_block_hash(db.conn.0,anchor.height)?!=Some(BlockHash(anchor.hash)) || !db.pending_swap_payments(account,key)?.is_empty() {
                return Err(corrupt("directory check is incomplete or its anchor changed"));
            }
            db.conn.0.execute("INSERT INTO ironwood_swap_directory_checks(receiving_key_id,height,block_hash) VALUES (?1,?2,?3) ON CONFLICT(receiving_key_id) DO UPDATE SET height=excluded.height,block_hash=excluded.block_hash",params![id,u32::from(anchor.height),anchor.hash])?;
            Ok(())
        })
    }
}

// Use u64 for the exclusive end so the maximum block height cannot overflow.
pub(super) fn has_gap(
    start: u32,
    through: u32,
    checked: Option<u32>,
    ranges: impl IntoIterator<Item = std::ops::Range<u32>>,
) -> bool {
    let end = u64::from(through) + 1;
    let mut cursor = u64::from(start).max(checked.map_or(0, |h| u64::from(h) + 1));
    for range in ranges {
        if cursor >= end {
            return false;
        }
        if u64::from(range.start) > cursor {
            return true;
        }
        cursor = cursor.max(u64::from(range.end));
    }
    cursor < end
}

#[cfg(test)]
mod coverage_tests {
    use super::has_gap;
    #[test]
    fn directory_hands_off_to_contiguous_local_scanning() {
        assert!(!has_gap(100, 220, Some(199), [200..221]));
        assert!(!has_gap(100, 230, Some(199), [200..231]));
        assert!(has_gap(100, 220, Some(198), [200..221]));
        assert!(has_gap(100, 220, Some(199), [200..210, 211..221]));
    }
    #[test]
    fn missing_or_rewound_directory_requires_history_again() {
        assert!(has_gap(100, 220, None, [200..221]));
        assert!(!has_gap(100, 220, None, [100..221]));
        assert!(has_gap(100, 220, Some(199), [200..215]));
        assert!(!has_gap(221, 220, None, []));
    }
}
