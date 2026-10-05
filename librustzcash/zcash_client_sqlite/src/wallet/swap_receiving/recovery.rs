//! Recovery from authenticated wallet data, independent of the discovery transport.

use std::borrow::BorrowMut;

use crate::{util::Clock, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use zakura_swap_receiving::RefundMemo;
use zcash_keys::encoding::AddressCodec as _;
use zcash_protocol::consensus::{BlockHeight, Parameters};

use super::{Discovery, Error, KeyId, Purpose, account_key, decode_index, register, unix_now};
use crate::{AccountUuid, SqlTransaction, WalletDb};

/// A confirmed funding record recovered from an ordinary internal Ironwood note.
/// The deposit address restores the application's provider-status lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredRefund {
    /// Refund sequence index.
    pub index: u64,
    /// Address of the funding transaction's only transparent output (P2PKH or P2SH).
    pub deposit_address: String,
    /// Earliest block to scan with the recovered refund key.
    pub funding_height: BlockHeight,
}

impl<C: BorrowMut<Connection>, P: Parameters, CL: Clock, R> WalletDb<C, P, CL, R> {
    /// Registers refund keys from confirmed, authenticated internal funding memos.
    ///
    /// Run after scanning and memo enhancement, including during restore. A key this
    /// wallet was already scanning when the funding transaction was mined needs
    /// nothing more. Any other key is restored: it is queued for a receiver-directory
    /// sweep and a provider-status watch. Own-send evidence must include an input
    /// belonging to the same account. Records whose inputs or memos are not yet
    /// available remain eligible on subsequent calls.
    /// Spent and zero-value marker notes are included. Unsupported records, and
    /// funding transactions without a single transparent deposit output, return
    /// an error and remain stored, rather than silently completing recovery.
    /// Returns only records processed by this call. Completed records are skipped
    /// across restarts unless their memo or funding height changes. Progress is
    /// committed atomically with the key registration and provider watch.
    pub fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
        self.transactionally(|db| db.recover_swap_refund_memos(account))
    }

    /// Claims due provider lookups for authenticated refund records. Call at sync
    /// time, including when no new blocks arrive. The persisted retry time also
    /// bounds failed requests across restarts. Unknown responses and inconclusive
    /// terminal statuses, such as `FAILED`, leave watches active.
    /// Applications pass a Unix timestamp and perform network I/O after this returns.
    /// Unchecked records are returned first, up to `limit`, so old failed requests
    /// cannot starve newly restored records.
    pub fn take_swap_refund_status_checks(
        &mut self,
        account: AccountUuid,
        now: i64,
        limit: std::num::NonZeroU32,
    ) -> Result<Vec<(KeyId, String)>, Error> {
        if now < 0 {
            return Err(super::corrupt("invalid provider-check time"));
        }
        self.transactionally(|db| {
            let (id, _) = account_key(db.conn.0, &db.params, account)?;
            let mut stmt = db.conn.0.prepare_cached(
                "SELECT k.key_index,w.operation_id FROM ironwood_swap_refund_watches w
                 JOIN ironwood_receiving_keys k ON k.id=w.receiving_key_id
                 JOIN ironwood_swap_operations s ON s.receiving_key_id=w.receiving_key_id
                    AND s.operation_id=w.operation_id
                 WHERE k.account_id=?1 AND k.closed_at IS NULL
                   AND (s.terminal_at IS NULL OR s.expectation=0) AND w.next_check_at<=?2
                 ORDER BY w.next_check_at,w.receiving_key_id,w.operation_id LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![id.0, now, limit.get()], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let next = now
                .checked_add(60)
                .ok_or_else(|| super::corrupt("provider-check time overflow"))?;
            let mut result = Vec::new();
            for (index, operation) in rows {
                let key = KeyId::new(Purpose::Refund, decode_index(index)?);
                let key_ref = super::payments::key_ref(db.conn.0, account, key)?;
                db.conn.0.execute(
                    "UPDATE ironwood_swap_refund_watches SET next_check_at=?3
                    WHERE receiving_key_id=?1 AND operation_id=?2",
                    params![key_ref, operation, next],
                )?;
                result.push((key, operation));
            }
            Ok(result)
        })
    }

    /// Ensures `count` incoming keys beyond the highest reserved or paid index, queued
    /// for a receiver-directory sweep.
    ///
    /// Call after scanning and after storing payments. The first window, and any
    /// window above a paid key found by restore and never reserved, extends restore
    /// recovery. Indices above a key this wallet issued were never handed out by it,
    /// so they need no sweep. This is bounded-gap recovery, not a completeness proof.
    pub fn maintain_swap_receive_lookahead(
        &mut self,
        account: AccountUuid,
        count: u32,
        scan_from: BlockHeight,
    ) -> Result<(), Error> {
        self.transactionally(|db| db.maintain_swap_receive_lookahead(account, count, scan_from))
    }
}

impl<P: Parameters, CL: Clock, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// See [`WalletDb::recover_swap_refund_memos`] on a connection-backed handle.
    pub fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
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
        for (bytes, height, note_id, raw) in records {
            // SQLite omits trailing zero padding when storing MemoBytes.
            let bytes = zcash_protocol::memo::MemoBytes::from_bytes(&bytes)
                .map_err(|_| super::corrupt("invalid stored swap memo length"))?;
            let memo = RefundMemo::decode(bytes.as_array())
                .map_err(|e| super::corrupt(&e.to_string()))?
                .ok_or_else(|| super::corrupt("missing swap memo discriminator"))?;
            let raw = raw.ok_or_else(|| super::corrupt("missing swap funding transaction"))?;
            let (_, tx) = wallet::parse_tx(&self.params, &raw, Some(height.into()), None)?;
            let deposit_address = match tx.transparent_bundle().map(|b| &b.vout[..]) {
                Some([output]) => output.recipient_address(),
                _ => None,
            }
            .ok_or_else(|| super::corrupt("invalid swap funding deposit output"))?
            .encode(&self.params);
            let key_id = KeyId::new(Purpose::Refund, memo.index());
            let scanned_locally = self
                .conn
                .0
                .query_row(
                    "SELECT active_from <= ?3 FROM ironwood_receiving_keys
                     WHERE account_id = ?1 AND purpose = 0 AND derivation_version = 1 AND key_index = ?2",
                    params![account_ref.0, key_id.index().to_be_bytes(), height],
                    |row| row.get::<_, Option<bool>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(false);
            if !scanned_locally {
                let key = register(
                    self.conn.0,
                    &self.params,
                    account,
                    key_id,
                    height.into(),
                    true,
                    Discovery::Sweep,
                    now,
                )?;
                let id = super::payments::key_ref(self.conn.0, account, key.key_id())?;
                // Restoring a seed has no local activity record. The provider
                // identity schedules status polling for the restored refund.
                self.conn.0.execute(
                    "INSERT OR IGNORE INTO ironwood_swap_refund_watches (receiving_key_id, operation_id)
                     VALUES (?1, ?2)",
                    params![id, deposit_address],
                )?;
                self.conn.0.execute(
                    "INSERT OR IGNORE INTO ironwood_swap_operations (receiving_key_id, operation_id)
                     VALUES (?1, ?2)",
                    params![id, deposit_address],
                )?;
            }
            let id = super::payments::key_ref(self.conn.0, account, key_id)?;
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
                funding_height: height.into(),
            });
        }
        Ok(recovered)
    }

    /// See [`WalletDb::maintain_swap_receive_lookahead`] on a connection-backed handle.
    pub fn maintain_swap_receive_lookahead(
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
                    AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_reservations r WHERE r.receiving_key_id = k.id)
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
