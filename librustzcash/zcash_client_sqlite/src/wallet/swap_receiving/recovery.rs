//! Recovery from authenticated wallet data, independent of the discovery transport.

use std::borrow::BorrowMut;

use crate::{util::Clock, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use zakura_swap_receiving::RefundMemo;
use zcash_keys::encoding::AddressCodec as _;
use zcash_protocol::consensus::{BlockHeight, Parameters};

use super::{
    Discovery, Error, KeyId, Purpose, account_key, decode_index, lifecycle::record_observation,
    payments::key_ref, register, reservations::RECEIVE_LOOKAHEAD, restore_start,
    retention::retain_spend_history, unix_now,
};
use crate::{AccountUuid, SqlTransaction, WalletDb};
use zakura_swap_receiving::lifecycle::{Observation, OperationStatus};
use zcash_protocol::consensus::NetworkUpgrade;

/// A confirmed funding record recovered from an ordinary internal Ironwood note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecoveredRefund {
    /// Refund sequence index.
    pub(crate) index: u64,
    /// Address of the funding transaction's only transparent output (P2PKH or P2SH),
    /// or `None` when the transaction did not have exactly one or is not stored.
    pub(crate) deposit_address: Option<String>,
}

/// The outcome of one pass over an account's funding records.
pub(super) struct MemoRecovery {
    /// Records processed by this pass.
    pub(super) recovered: Vec<RecoveredRefund>,
    /// Records this version cannot read. They stay unprocessed.
    pub(super) unreadable: usize,
}

impl<C: BorrowMut<Connection>, P: Parameters, CL: Clock, R> WalletDb<C, P, CL, R> {
    /// See [`WalletDb::recover_swap_refund_memos`] on a transaction-backed handle.
    #[cfg(test)]
    pub(crate) fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
        self.transactionally(|db| db.recover_swap_refund_memos(account))
    }

    /// Queues one receiver-directory sweep for each of `account`'s closed swap keys, as
    /// a seed restore does, and returns how many were queued.
    ///
    /// A key stops scanning once its swap settles, so a rare later payment, such as a
    /// second refund, is found only by a sweep. Call this when the user asks to recheck
    /// swap history. A finished sweep reopens its key from the sweep's anchor until it
    /// closes again, and new incoming reservations wait for these sweeps as they do
    /// after a restore.
    pub fn recheck_swap_history(&mut self, account: AccountUuid) -> Result<usize, Error> {
        self.transactionally(|db| {
            let (owner, _) = account_key(db.conn.0, &db.params, account)?;
            Ok(db.conn.0.execute(
                "INSERT INTO ironwood_swap_sweeps (receiving_key_id)
                 SELECT id FROM ironwood_receiving_keys
                 WHERE account_id = ?1 AND closed_at IS NOT NULL
                 ON CONFLICT (receiving_key_id) DO UPDATE SET
                     target_height = NULL, target_hash = NULL, lookup_height = NULL,
                     lookup_hash = NULL, attempts = 0, next_attempt_at = 0, done_height = NULL
                 WHERE done_height IS NOT NULL",
                [owner.0],
            )?)
        })
    }

    /// Keeps `account`'s swap recovery current. Call under the wallet write lock when
    /// each sync starts, before planning scan work, and again once it reaches the tip.
    ///
    /// Retains Ironwood spend evidence from the account's birthday until
    /// [`WalletDb::finish_swap_nullifier_recovery`] releases it; evidence pruned
    /// before the first call cannot be recovered without a rescan. Once scanning
    /// reaches the chain tip at or above Ironwood activation, it also registers refund
    /// keys from confirmed funding memos and keeps
    /// [`RECEIVE_GAP_LIMIT`](super::RECEIVE_GAP_LIMIT) incoming lookahead keys above
    /// the highest restored index, each queued for one receiver-directory sweep.
    pub fn maintain_swap_receiving(&mut self, account: AccountUuid) -> Result<(), Error> {
        self.transactionally(|db| {
            retain_spend_history(db.conn.0, &db.params, account)?;
            db.maintain_restore_discovery(account, RECEIVE_LOOKAHEAD)
        })
    }

    /// See [`WalletDb::maintain_swap_receive_lookahead`] on a transaction-backed handle.
    #[cfg(test)]
    pub(crate) fn maintain_swap_receive_lookahead(
        &mut self,
        account: AccountUuid,
        count: u32,
        scan_from: BlockHeight,
    ) -> Result<(), Error> {
        self.transactionally(|db| db.maintain_swap_receive_lookahead(account, count, scan_from))
    }
}

impl<P: Parameters, CL: Clock, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// Registers refund keys from confirmed, authenticated internal funding memos.
    ///
    /// Run after scanning and memo enhancement, including during restore. A key this
    /// wallet was already scanning when the funding transaction was mined needs
    /// nothing more, except that its stored funding transaction marks the funded quote
    /// as waiting for its outcome. Any other key is restored: it is queued for a
    /// receiver-directory sweep and then scans until the completion limit after the
    /// funding block, with no provider lookups. Own-send evidence must include an
    /// input belonging to the same account. Spent and zero-value marker notes are
    /// included.
    ///
    /// A record whose inputs or memo are not available yet, or that this version cannot
    /// read, stays unprocessed without failing the call, and refund issuance waits for
    /// it (see [`WalletDb::swap_refund_memos_pending`]). Returns only records processed
    /// by this call. Completed records are skipped across restarts unless their memo or
    /// funding height changes. Progress is committed atomically with the key
    /// registration.
    pub(crate) fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
        Ok(self.recover_refund_memos(account)?.recovered)
    }

    /// See [`WalletDb::recover_swap_refund_memos`]. Also counts unreadable records.
    pub(super) fn recover_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<MemoRecovery, Error> {
        let (account_ref, _) = account_key(self.conn.0, &self.params, account)?;
        let records = {
            let mut stmt = self.conn.0.prepare_cached(
                "SELECT n.memo, t.mined_height, n.id, t.raw
                 FROM ironwood_received_notes n
                 JOIN transactions t ON t.id_tx = n.transaction_id
                 WHERE n.account_id = ?1 AND n.recipient_key_scope = 1
                   AND n.receiving_key_id IS NULL AND t.mined_height IS NOT NULL
                   AND substr(n.memo, 1, 5) = X'FF5A535750'
                   AND NOT EXISTS (SELECT 1 FROM ironwood_swap_refund_memo_progress p
                                   WHERE p.note_id=n.id AND p.funding_height=t.mined_height)
                   AND EXISTS (SELECT 1 FROM v_received_output_spends s
                               WHERE s.transaction_id = n.transaction_id
                                 AND s.account_id = n.account_id)",
            )?;
            stmt.query_map([account_ref.0], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let now = unix_now(&self.clock);
        let mut recovered = Vec::new();
        let mut unreadable = 0;
        for (bytes, height, note_id, raw) in records {
            // SQLite omits trailing zero padding when storing MemoBytes.
            let memo = zcash_protocol::memo::MemoBytes::from_bytes(&bytes)
                .ok()
                .and_then(|bytes| RefundMemo::decode(bytes.as_array()).ok().flatten());
            let Some(memo) = memo else {
                unreadable += 1;
                continue;
            };
            // Only the device that funded the swap stores the raw transaction, which
            // names the deposit; a restored key needs neither.
            let deposit_address = raw
                .and_then(|raw| {
                    wallet::parse_tx(&self.params, &raw, Some(height.into()), None).ok()
                })
                .and_then(
                    |(_, tx)| match tx.transparent_bundle().map(|b| &b.vout[..]) {
                        Some([output]) => output
                            .recipient_address()
                            .map(|address| address.encode(&self.params)),
                        _ => None,
                    },
                );
            let key_id = KeyId::new(Purpose::Refund, memo.index());
            let scanned_locally = self
                .conn
                .0
                .query_row(
                    "SELECT active_from <= ?3 FROM ironwood_receiving_keys
                     WHERE account_id = ?1 AND purpose = 0 AND derivation_version = 1
                       AND key_index = ?2",
                    params![account_ref.0, key_id.index().to_be_bytes(), height],
                    |row| row.get::<_, Option<bool>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(false);
            let id = if scanned_locally {
                let id = key_ref(self.conn.0, account, key_id)?;
                // The mined funding transaction proves a deposit, so the quote's record,
                // which expects nothing, must wait for the swap's outcome. Observation
                // time 0 changes only a record no provider status has updated.
                if let Some(deposit) = &deposit_address {
                    let funded = Observation {
                        status: OperationStatus::Active,
                        deadline: None,
                    };
                    record_observation(self.conn.0, id, deposit, funded, 0)?;
                }
                id
            } else {
                // Registering at the funding block's time makes the completion limit
                // count from the swap, so an old swap's key closes after its sweep.
                let funded_at = self
                    .conn
                    .0
                    .query_row(
                        "SELECT time FROM blocks WHERE height = ?1",
                        [height],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .unwrap_or(now);
                register(
                    self.conn.0,
                    &self.params,
                    account,
                    key_id,
                    height.into(),
                    true,
                    Discovery::Sweep,
                    funded_at,
                )?
                .0
            };
            self.conn.0.execute(
                "INSERT INTO ironwood_swap_refund_memo_progress
                (note_id,receiving_key_id,funding_height) VALUES(?1,?2,?3)
                ON CONFLICT(note_id) DO UPDATE SET
                    receiving_key_id=excluded.receiving_key_id,
                    funding_height=excluded.funding_height",
                params![note_id, id, height],
            )?;
            recovered.push(RecoveredRefund {
                index: memo.index(),
                deposit_address,
            });
        }
        Ok(MemoRecovery {
            recovered,
            unreadable,
        })
    }

    /// See [`WalletDb::maintain_swap_receiving`], keeping `lookahead` incoming keys.
    /// Does nothing until scanning reaches a chain tip at or above Ironwood activation.
    pub(crate) fn maintain_restore_discovery(
        &mut self,
        account: AccountUuid,
        lookahead: u32,
    ) -> Result<(), Error> {
        let tip = wallet::chain_tip_height(self.conn.0)?;
        let ready = match (tip, self.params.activation_height(NetworkUpgrade::Nu6_3)) {
            (Some(tip), Some(activation)) => {
                tip >= activation && wallet::fully_scanned_height(self.conn.0)? == Some(tip)
            }
            _ => false,
        };
        if !ready {
            return Ok(());
        }
        self.recover_swap_refund_memos(account)?;
        self.extend_receive_lookahead(account, lookahead)
    }

    /// Keeps `count` incoming lookahead keys, scanned from the account's restore start.
    pub(crate) fn extend_receive_lookahead(
        &mut self,
        account: AccountUuid,
        count: u32,
    ) -> Result<(), Error> {
        let (account_ref, _) = account_key(self.conn.0, &self.params, account)?;
        let start = restore_start(self.conn.0, &self.params, account_ref)?;
        self.maintain_swap_receive_lookahead(account, count, start)
    }

    /// Ensures `count` incoming keys beyond the highest reserved or paid index, queued
    /// for a receiver-directory sweep.
    ///
    /// Call after scanning and after storing payments. The first window, and any
    /// window above a paid key found by restore and never reserved, extends restore
    /// recovery. Indices above a key this wallet issued were never handed out by it,
    /// so they need no sweep. This is bounded-gap recovery, not a completeness proof.
    pub(crate) fn maintain_swap_receive_lookahead(
        &mut self,
        account: AccountUuid,
        count: u32,
        scan_from: BlockHeight,
    ) -> Result<(), Error> {
        let (account_ref, _) = account_key(self.conn.0, &self.params, account)?;
        let top: Option<(Vec<u8>, bool)> = self
            .conn
            .0
            .query_row(
                "SELECT k.key_index,
                    EXISTS(SELECT 1 FROM ironwood_swap_sweeps s WHERE s.receiving_key_id = k.id)
                    AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_reservations r
                        WHERE r.receiving_key_id = k.id)
                 FROM ironwood_receiving_keys k
                 WHERE k.account_id = ?1 AND k.purpose = 1 AND k.derivation_version = 1
                   AND k.advances_allocation = 1
                 ORDER BY k.key_index DESC LIMIT 1",
                [account_ref.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if top.as_ref().is_some_and(|(_, restored)| !restored) {
            return Ok(());
        }
        let last = top.map(|(index, _)| index);
        let start = last
            .map(decode_index)
            .transpose()?
            .map(|i| i.checked_add(1).ok_or(Error::IndexExhausted))
            .transpose()?
            .unwrap_or(0);
        for offset in 0..u64::from(count) {
            let index = start.checked_add(offset).ok_or(Error::IndexExhausted)?;
            let exists: bool = self.conn.0.query_row(
                "SELECT EXISTS (SELECT 1 FROM ironwood_receiving_keys
                 WHERE account_id = ?1 AND purpose = 1 AND derivation_version = 1
                   AND key_index = ?2 AND scan_from <= ?3)",
                params![account_ref.0, index.to_be_bytes(), u32::from(scan_from)],
                |row| row.get(0),
            )?;
            if !exists {
                self.watch_swap_receive_key(account, index, scan_from)?;
            }
        }
        Ok(())
    }
}
