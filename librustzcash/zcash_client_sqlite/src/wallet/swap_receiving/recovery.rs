//! Recovery from authenticated wallet data, independent of the discovery transport.

use std::borrow::BorrowMut;

use crate::wallet;
use rusqlite::{Connection, params};
use zakura_swap_receiving::RefundMemo;
use zcash_protocol::consensus::{BlockHeight, Parameters};

use super::{Error, KeyId, Purpose, account_key, decode_index, register};
use crate::{AccountUuid, SqlTransaction, WalletDb};

/// A confirmed funding record recovered from an ordinary internal Ironwood note.
/// The deposit address restores the application's provider-status lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredRefund {
    /// Refund sequence index.
    pub index: u64,
    /// Exact deposit address authenticated by the memo.
    pub deposit_address: String,
    /// Earliest block to scan with the recovered refund key.
    pub funding_height: BlockHeight,
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Registers refund keys from confirmed, authenticated internal funding memos.
    ///
    /// Run after scanning and memo enhancement, including during restore. Own-send
    /// evidence must include an input belonging to the same account. Records whose
    /// inputs or memos are not yet available remain eligible on subsequent calls.
    /// Spent and zero-value marker notes are included. Unsupported records return
    /// an error and remain stored, rather than silently completing recovery.
    pub fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
        self.transactionally(|db| db.recover_swap_refund_memos(account))
    }

    /// Claims due provider lookups for authenticated refund records. Call at sync
    /// time, including when no new blocks arrive. The persisted retry time also
    /// bounds failed requests across restarts. Unknown responses leave watches active.
    /// Applications pass a Unix timestamp and perform network I/O after this returns.
    pub fn take_swap_refund_status_checks(
        &mut self,
        account: AccountUuid,
        now: i64,
    ) -> Result<Vec<(KeyId, String)>, Error> {
        if now < 0 {
            return Err(super::corrupt("invalid provider-check time"));
        }
        self.transactionally(|db| {
            let (id, _) = account_key(db.conn.0, &db.params, account)?;
            let mut stmt = db.conn.0.prepare_cached(
                "SELECT k.key_index,w.operation_id FROM ironwood_swap_refund_watches w
                 JOIN ironwood_receiving_keys k ON k.id=w.receiving_key_id
                 JOIN ironwood_swap_scan_uses s ON s.receiving_key_id=w.receiving_key_id
                    AND s.operation_id=w.operation_id
                 WHERE k.account_id=?1 AND s.scan_through IS NULL AND w.next_check_at<=?2",
            )?;
            let rows = stmt
                .query_map(params![id.0, now], |r| {
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

    /// Ensures `count` incoming keys beyond the highest reserved or paid index.
    ///
    /// Call before scanning and after storing payments. New keys queue replay
    /// from `scan_from`, so a payment found at the edge can reveal earlier payments
    /// to the next window. This is bounded-gap recovery, not a completeness proof.
    pub fn maintain_swap_receive_lookahead(
        &mut self,
        account: AccountUuid,
        count: u32,
        scan_from: BlockHeight,
    ) -> Result<(), Error> {
        self.transactionally(|db| db.maintain_swap_receive_lookahead(account, count, scan_from))
    }
}

impl<P: Parameters, CL, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// See [`WalletDb::recover_swap_refund_memos`] on a connection-backed handle.
    pub fn recover_swap_refund_memos(
        &mut self,
        account: AccountUuid,
    ) -> Result<Vec<RecoveredRefund>, Error> {
        let (account_ref, _) = account_key(self.conn.0, &self.params, account)?;
        let records = {
            let mut stmt = self.conn.0.prepare_cached(
                "SELECT n.memo, t.mined_height
                 FROM ironwood_received_notes n
                 JOIN transactions t ON t.id_tx = n.transaction_id
                 WHERE n.account_id = ?1 AND n.recipient_key_scope = 1
                   AND n.receiving_key_id IS NULL AND t.mined_height IS NOT NULL
                   AND substr(n.memo, 1, 5) = X'FF5A535750'
                   AND EXISTS (SELECT 1 FROM v_received_output_spends s
                               WHERE s.transaction_id = n.transaction_id
                                 AND s.account_id = n.account_id)",
            )?;
            stmt.query_map([account_ref.0], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u32>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let restored_through = wallet::fully_scanned_height(self.conn.0)?;
        let mut recovered = Vec::new();
        for (bytes, height) in records {
            // SQLite omits trailing zero padding when storing MemoBytes.
            let bytes = zcash_protocol::memo::MemoBytes::from_bytes(&bytes)
                .map_err(|_| super::corrupt("invalid stored swap memo length"))?;
            let memo = RefundMemo::decode(self.params.network_type(), bytes.as_array())
                .map_err(|e| super::corrupt(&e.to_string()))?
                .ok_or_else(|| super::corrupt("missing swap memo discriminator"))?;
            let key_id = KeyId::new(Purpose::Refund, memo.index());
            let needs_registration: bool = self.conn.0.query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM ironwood_receiving_keys
                 WHERE account_id = ?1 AND purpose = 0 AND derivation_version = 1
                   AND key_index = ?2 AND scan_from <= ?3 AND advances_allocation = 1)",
                params![account_ref.0, key_id.index().to_be_bytes(), height],
                |row| row.get(0),
            )?;
            if needs_registration {
                register(
                    self.conn.0,
                    &self.params,
                    account,
                    key_id,
                    height.into(),
                    true,
                )?;
            }
            // Seed restoration has no local activity record. Persist its provider
            // identity and activate future scanning before returning the new key.
            if let Some(scanned) = restored_through {
                let id = super::payments::key_ref(self.conn.0, account, key_id)?;
                let local: bool = self.conn.0.query_row(
                    "SELECT EXISTS(SELECT 1 FROM
                    ironwood_swap_scan_uses WHERE receiving_key_id=?1 AND operation_id=?2)",
                    params![id, memo.deposit_address()],
                    |r| r.get(0),
                )?;
                let initial = (!local).then_some(u32::from(scanned));
                self.conn.0.execute(
                    "INSERT OR IGNORE INTO ironwood_swap_refund_watches
                    (receiving_key_id,operation_id,initial_height) VALUES(?1,?2,?3)",
                    params![id, memo.deposit_address(), initial],
                )?;
                // Never reset a locally observed terminal deadline on another scan.
                self.conn.0.execute(
                    "INSERT OR IGNORE INTO ironwood_swap_scan_uses
                    (receiving_key_id,operation_id,scan_from,scan_through) VALUES(?1,?2,?3,NULL)",
                    params![id, memo.deposit_address(), height],
                )?;
            }
            recovered.push(RecoveredRefund {
                index: memo.index(),
                deposit_address: memo.deposit_address().to_owned(),
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
        let last: Option<Vec<u8>> = self.conn.0.query_row(
            "SELECT MAX(key_index) FROM ironwood_receiving_keys
             WHERE account_id = ?1 AND purpose = 1 AND derivation_version = 1
               AND advances_allocation = 1",
            [account_ref.0],
            |row| row.get(0),
        )?;
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
