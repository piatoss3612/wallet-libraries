//! Restore sweeps: one receiver-directory pass for each key recovered from the seed.
//! Preparing metadata does not derive viewing keys.
use super::{
    Error, KeyId, PendingPayment, Purpose, account_key, activate, corrupt, payments::key_ref,
    reservations::used, stored_key_id,
};
use crate::{AccountUuid, SqlTransaction, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zcash_client_backend::data_api::transparent_ledger::ChainPoint;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

/// One receiver's sweep target. Public metadata is not note ownership.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveryWork {
    /// Derivation identity. Derive only when authenticating returned notes.
    pub key: KeyId,
    /// Canonical address bytes already stored at registration.
    pub receiver: [u8; 43],
    /// Persisted, independently accepted target that retries must reach.
    pub target: ChainPoint,
    /// A completed lookup whose candidates are durably queued. Resume those first.
    pub lookup: Option<ChainPoint>,
}
/// Bounded work plus the entire job's due, uncached lookup count for transport choice.
pub struct DiscoveryBatch {
    /// At most the requested number of records. Taking a batch does not lease its tail.
    pub work: Vec<DiscoveryWork>,
    /// Remaining due receivers needing network discovery, not lifetime registrations.
    pub remaining_lookups: usize,
}

/// Returns `anchor` only while its block is still on the wallet's chain.
fn canonical(conn: &Connection, anchor: Option<ChainPoint>) -> Result<Option<ChainPoint>, Error> {
    Ok(match anchor {
        Some(a) if wallet::get_block_hash(conn, a.height)? == Some(a.hash) => Some(a),
        _ => None,
    })
}

/// Builds an anchor from a stored height and hash pair.
fn anchor(height: Option<u32>, hash: Option<[u8; 32]>) -> Option<ChainPoint> {
    height.zip(hash).map(|(height, hash)| ChainPoint {
        height: BlockHeight::from(height),
        hash: BlockHash(hash),
    })
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Last atomically persisted lookup, independent of candidate application.
    pub(crate) fn swap_lookup_coverage(
        &self,
        account: AccountUuid,
        key: KeyId,
    ) -> Result<Option<ChainPoint>, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let lookup = conn
            .query_row(
                "SELECT lookup_height, lookup_hash FROM ironwood_swap_sweeps
                 WHERE receiving_key_id = ?1",
                [id],
                |r| Ok(anchor(r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .flatten();
        canonical(conn, lookup)
    }

    /// Whether `account` still has restore sweeps or queued candidates through `through`.
    /// Retry backoff never makes an unfinished restore appear complete.
    pub fn swap_history_pending(
        &self,
        account: AccountUuid,
        through: BlockHeight,
    ) -> Result<bool, Error> {
        let conn = self.conn.borrow();
        let (owner, _) = account_key(conn, &self.params, account)?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_swap_sweeps s
                JOIN ironwood_receiving_keys k ON k.id = s.receiving_key_id
                WHERE k.account_id = ?1 AND k.scan_from <= ?2 AND s.done_height IS NULL)
             OR EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery p
                JOIN ironwood_receiving_keys k ON k.id = p.receiving_key_id
                WHERE k.account_id = ?1)",
            params![owner.0, u32::from(through)],
            |r| r.get(0),
        )?)
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Selects due sweeps without reconstructing historical keys. Each key's target
    /// is fixed at its first selection and replaced only after a reorg removes it.
    /// Attempts are leased separately just before I/O, so a stopped batch cannot
    /// starve its tail.
    pub fn prepare_swap_discovery_batch(
        &mut self,
        account: AccountUuid,
        through: ChainPoint,
        now: i64,
        limit: std::num::NonZeroU32,
    ) -> Result<DiscoveryBatch, Error> {
        if now < 0 {
            return Err(corrupt("invalid recovery time"));
        }
        self.transactionally(|db| {
            let (owner, _) = account_key(db.conn.0, &db.params, account)?;
            if canonical(db.conn.0, Some(through))?.is_none() {
                return Err(Error::SweepDeferred(super::SweepDeferral::UnknownAnchor));
            }
            // A finished sweep is offered again while a late lookup left candidates queued.
            let eligible = "FROM ironwood_swap_sweeps s
                JOIN ironwood_receiving_keys k ON k.id = s.receiving_key_id
                WHERE k.account_id = ?1 AND k.scan_from <= ?2 AND s.next_attempt_at <= ?3
                  AND (s.done_height IS NULL OR EXISTS(
                      SELECT 1 FROM ironwood_swap_payment_recovery p
                      WHERE p.receiving_key_id = s.receiving_key_id))";
            let remaining_lookups = db.conn.0.query_row(
                &format!(
                    "SELECT COUNT(*) {eligible} AND (s.lookup_height IS NULL
                        OR s.target_height IS NULL OR s.lookup_height < s.target_height)"
                ),
                params![owner.0, u32::from(through.height), now],
                |r| r.get::<_, usize>(0),
            )?;
            let rows = {
                let mut stmt = db.conn.0.prepare(&format!(
                    "SELECT k.purpose, k.derivation_version, k.key_index, k.id, k.receiver,
                        s.target_height, s.target_hash, s.lookup_height, s.lookup_hash
                    {eligible} ORDER BY s.next_attempt_at, s.receiving_key_id LIMIT ?4"
                ))?;
                let through_height = u32::from(through.height);
                let mut rows = stmt.query(params![owner.0, through_height, now, limit.get()])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    out.push((
                        stored_key_id(r)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        anchor(r.get(5)?, r.get(6)?),
                        anchor(r.get(7)?, r.get(8)?),
                    ));
                }
                out
            };
            let mut work = Vec::new();
            for (key, id, receiver, target, lookup) in rows {
                let target = match canonical(db.conn.0, target)? {
                    Some(target) => target,
                    None => {
                        db.conn.0.execute(
                            "UPDATE ironwood_swap_sweeps SET target_height = ?2, target_hash = ?3
                             WHERE receiving_key_id = ?1",
                            params![id, u32::from(through.height), through.hash.0],
                        )?;
                        through
                    }
                };
                let lookup = canonical(db.conn.0, lookup)?.filter(|a| a.height >= target.height);
                work.push(DiscoveryWork {
                    key,
                    receiver: receiver
                        .try_into()
                        .map_err(|_| corrupt("invalid stored receiver"))?,
                    target,
                    lookup,
                });
            }
            Ok(DiscoveryBatch {
                work,
                remaining_lookups,
            })
        })
    }

    /// Leases `key`'s sweep for an attempt against a directory publication at
    /// `publication`, just before its network lookups. Every attempt, including one
    /// that fails or the process abandons, retains the target and backs off the next
    /// from one minute to twelve hours. A publication short of the sweep's target
    /// returns [`Error::SweepDeferred`], so no lookup is spent on it.
    pub fn begin_swap_discovery_attempt(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        publication: ChainPoint,
        now: i64,
    ) -> Result<(), Error> {
        let reached = self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            let (attempt, target): (u32, Option<u32>) = db.conn.0.query_row(
                "SELECT attempts, target_height FROM ironwood_swap_sweeps
                 WHERE receiving_key_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let delay = (60i64 << attempt.min(10)).min(43200);
            db.conn.0.execute(
                "UPDATE ironwood_swap_sweeps
                 SET attempts = MIN(attempts + 1, 30), next_attempt_at = ?2
                 WHERE receiving_key_id = ?1",
                params![id, now.saturating_add(delay)],
            )?;
            Ok::<_, Error>(target.is_none_or(|target| u32::from(publication.height) >= target))
        })?;
        if !reached {
            return Err(Error::SweepDeferred(super::SweepDeferral::TargetNotReached));
        }
        Ok(())
    }

    /// See [`WalletDb::queue_swap_lookup`] on a transaction-backed handle.
    #[cfg(test)]
    pub(crate) fn queue_swap_lookup(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainPoint,
        payments: &[PendingPayment],
    ) -> Result<(), Error> {
        self.transactionally(|db| db.queue_swap_lookup(account, key, anchor, payments))
    }

    /// Completes a key's sweep at `anchor` once all its candidates are applied.
    ///
    /// The key then scans from the next block until it closes: a refund key
    /// because the provider may still return funds, and an unpaid incoming key to
    /// catch a payout from a swap in flight at restore. Returns an error while
    /// candidates remain queued, unless a canonical lookup reached `anchor`, or if
    /// `anchor` is no longer canonical.
    pub(crate) fn finish_swap_discovery_attempt(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainPoint,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            let (pending, lookup): (bool, Option<ChainPoint>) = db.conn.0.query_row(
                "SELECT EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery
                        WHERE receiving_key_id = ?1),
                    lookup_height, lookup_hash
                 FROM ironwood_swap_sweeps WHERE receiving_key_id = ?1",
                [id],
                |r| Ok((r.get(0)?, self::anchor(r.get(1)?, r.get(2)?))),
            )?;
            if pending {
                return Err(corrupt("sweep has unapplied candidates"));
            }
            // A reorg between the lookup and this call leaves the sweep to run again.
            let covered = canonical(db.conn.0, lookup)?.is_some_and(|l| l.height >= anchor.height);
            if !covered || canonical(db.conn.0, Some(anchor))?.is_none() {
                return Err(Error::SweepDeferred(super::SweepDeferral::UnknownAnchor));
            }
            db.conn.0.execute(
                "UPDATE ironwood_swap_sweeps SET done_height = ?2 WHERE receiving_key_id = ?1",
                params![id, u32::from(anchor.height)],
            )?;
            if key.purpose() == Purpose::Refund || !used(db.conn.0, id)? {
                activate(db.conn.0, id, anchor.height + 1)?;
            }
            Ok(())
        })
    }
}

impl<P: Parameters, CL, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// Atomically persists an entire validated lookup and authenticated ciphertexts.
    /// No balance is credited here. The caller validates the publication's complete
    /// coverage and pagination before invoking this method, including for empty results.
    pub(crate) fn queue_swap_lookup(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainPoint,
        payments: &[PendingPayment],
    ) -> Result<(), Error> {
        let id = key_ref(self.conn.0, account, key)?;
        if canonical(self.conn.0, Some(anchor))?.is_none() {
            return Err(Error::SweepDeferred(super::SweepDeferral::UnknownAnchor));
        }
        for payment in payments {
            if payment.height > anchor.height {
                return Err(corrupt("payment exceeds lookup coverage"));
            }
            self.queue_swap_payment(account, key, payment)?;
        }
        self.conn.0.execute(
            "UPDATE ironwood_swap_sweeps SET lookup_height = ?2, lookup_hash = ?3
             WHERE receiving_key_id = ?1 AND (lookup_height IS NULL OR lookup_height <= ?2)",
            params![id, u32::from(anchor.height), anchor.hash.0],
        )?;
        Ok(())
    }
}
