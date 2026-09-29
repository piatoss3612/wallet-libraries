//! Temporary spend evidence for notes discovered after ordinary scanning.
use super::{Error, account_key, corrupt};
use crate::{AccountUuid, SqlTransaction, WalletDb, wallet};
use rusqlite::{Connection, params};
use std::borrow::BorrowMut;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::scanning::{ScanPriority, ScanRange};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Releases old Ironwood nullifiers only after memos, lookahead, directory checks
    /// and note imports are complete at a canonical scanned tip. Returns false while
    /// work remains. The next scan retains new evidence until this is called again.
    ///
    /// `lookahead` is the wallet's nonzero incoming-address gap limit. Maintenance
    /// runs inside the transaction so an edge payment cannot race with pruning.
    pub fn finish_swap_nullifier_recovery(
        &mut self,
        account: AccountUuid,
        through: ChainAnchor,
        lookahead: u32,
    ) -> Result<bool, Error> {
        if lookahead == 0 {
            return Err(corrupt("swap recovery requires a nonzero lookahead"));
        }
        self.transactionally(|db| {
            let (id, _) = account_key(db.conn.0, &db.params, account)?;
            let enabled: bool = db.conn.0.query_row(
                "SELECT EXISTS(SELECT 1 FROM ironwood_swap_private_recovery WHERE account_id=?1)",
                [id.0], |r| r.get(0),
            )?;
            if !enabled { return Ok(false); }
            if wallet::fully_scanned_height(db.conn.0)? != Some(through.height)
                || wallet::chain_tip_height(db.conn.0)? != Some(through.height)
                || wallet::get_block_hash(db.conn.0, through.height)? != Some(BlockHash(through.hash))
            {
                return Ok(false);
            }
            db.recover_swap_refund_memos(account)?;
            let birthday: u32 = db.conn.0.query_row(
                "SELECT birthday_height FROM accounts WHERE id=?1", [id.0], |r| r.get(0),
            )?;
            let start = BlockHeight::from(birthday).max(
                db.params.activation_height(NetworkUpgrade::Nu6_3)
                    .ok_or_else(|| corrupt("Ironwood inactive"))?,
            );
            db.maintain_swap_receive_lookahead(account, lookahead, start)?;
            // A decrypted marker can precede its own-send evidence. Keep it eligible
            // until its inputs are known, just as we keep a missing memo eligible.
            let pending: bool = db.conn.0.query_row(
                "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes n
                 JOIN transactions t ON t.id_tx=n.transaction_id
                 WHERE n.account_id=?1 AND n.recipient_key_scope=1
                 AND n.receiving_key_id IS NULL AND t.mined_height IS NOT NULL
                 AND (n.memo IS NULL OR (substr(n.memo,1,5)=X'FF5A535750' AND NOT EXISTS(
                    SELECT 1 FROM v_received_output_spends s
                    WHERE s.transaction_id=n.transaction_id AND s.account_id=n.account_id))))
                 OR EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery p
                    JOIN ironwood_receiving_keys k ON k.id=p.receiving_key_id WHERE k.account_id=?1)
                 OR EXISTS(SELECT 1 FROM ironwood_receiving_keys k
                    WHERE k.account_id=?1 AND k.scan_from<=?2 AND NOT EXISTS(
                        SELECT 1 FROM ironwood_swap_recovery_targets t
                        JOIN blocks b ON b.height=t.height AND b.hash=t.block_hash
                        JOIN ironwood_swap_directory_checks c ON c.receiving_key_id=t.receiving_key_id
                        JOIN blocks cb ON cb.height=c.height AND cb.hash=c.block_hash
                        WHERE t.receiving_key_id=k.id AND c.height>=t.height))",
                params![id.0, u32::from(through.height)], |r| r.get(0),
            )?;
            if pending { return Ok(false); }
            let next = u32::from(through.height).saturating_add(1);
            db.conn.0.execute(
                "UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=?2
                 WHERE account_id=?1", params![id.0, next],
            )?;
            wallet::prune_nullifier_map(db.conn.0, through.height.saturating_sub(crate::PRUNING_DEPTH))?;
            Ok(true)
        })
    }
}

impl<P: Parameters, CL, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// Repairs missing spend evidence using the account's public recovery interval.
    /// Discovery still uses PIR. Replaying ordinary blocks makes no nullifier query
    /// and cannot turn a service's missing response into a claim that a note is unspent.
    pub(super) fn queue_swap_spend_history(
        &mut self,
        account: AccountUuid,
        through: BlockHeight,
    ) -> Result<(), Error> {
        let (id, _) = account_key(self.conn.0, &self.params, account)?;
        let birthday: u32 = self.conn.0.query_row(
            "SELECT birthday_height FROM accounts WHERE id=?1",
            [id.0],
            |r| r.get(0),
        )?;
        let start = BlockHeight::from(birthday).max(
            self.params
                .activation_height(NetworkUpgrade::Nu6_3)
                .ok_or_else(|| corrupt("Ironwood inactive"))?,
        );
        let end = u32::from(through)
            .checked_add(1)
            .ok_or_else(|| corrupt("swap recovery height overflow"))?;
        if start >= BlockHeight::from(end) {
            return Err(corrupt("empty spend recovery interval"));
        }
        self.conn.0.execute(
            "INSERT INTO ironwood_swap_private_recovery(account_id,nullifier_retention_height)
             VALUES(?1,?2) ON CONFLICT(account_id) DO UPDATE SET
             nullifier_retention_height=MIN(nullifier_retention_height,excluded.nullifier_retention_height)",
            params![id.0,u32::from(start)],
        )?;
        let range = start..BlockHeight::from(end);
        wallet::scanning::replace_queue_entries::<crate::error::SqliteClientError>(
            self.conn.0,
            &range,
            std::iter::once(ScanRange::from_parts(range.clone(), ScanPriority::Historic)),
            true,
        )?;
        Ok(())
    }
}
