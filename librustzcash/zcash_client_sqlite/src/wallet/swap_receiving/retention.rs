//! Temporary spend evidence for notes that a restore sweep finds after ordinary scanning.
use super::{Error, account_key, corrupt, reservations::RECEIVE_LOOKAHEAD, restore_start};
use crate::{AccountUuid, SqlTransaction, WalletDb, util::Clock, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::BorrowMut;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::scanning::{ScanPriority, ScanRange};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Retains `account`'s Ironwood spend evidence from its birthday until
    /// [`WalletDb::finish_swap_nullifier_recovery`] releases it. Call before the
    /// first scan; evidence already pruned cannot be recovered without a rescan.
    pub fn retain_swap_spend_history(&mut self, account: AccountUuid) -> Result<(), Error> {
        self.transactionally(|db| retain_spend_history(db.conn.0, &db.params, account))
    }
}

/// See [`WalletDb::retain_swap_spend_history`].
pub(super) fn retain_spend_history<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
) -> Result<(), Error> {
    let (id, _) = account_key(conn, params, account)?;
    conn.execute(
        "INSERT OR IGNORE INTO ironwood_swap_spend_retention(account_id, nullifier_retention_height)
         SELECT id, MAX(birthday_height, ?2) FROM accounts WHERE id = ?1",
        params![
            id.0,
            params
                .activation_height(NetworkUpgrade::Nu6_3)
                .map(u32::from)
                .unwrap_or(0)
        ],
    )?;
    Ok(())
}

impl<C: BorrowMut<Connection>, P: Parameters, CL: Clock, R> WalletDb<C, P, CL, R> {
    /// Releases retained Ironwood nullifiers at a canonical scanned tip once no
    /// restore sweep needs them. Missing memos, pending sweeps and queued candidates
    /// protect their evidence. Returns true when release reaches the tip.
    ///
    /// Restore discovery (see [`WalletDb::maintain_swap_receiving`]) runs inside the
    /// transaction first, so an edge payment cannot race with pruning.
    pub fn finish_swap_nullifier_recovery(
        &mut self,
        account: AccountUuid,
        through: ChainAnchor,
    ) -> Result<bool, Error> {
        self.finish_swap_nullifier_recovery_with(account, through, RECEIVE_LOOKAHEAD)
    }

    /// [`WalletDb::finish_swap_nullifier_recovery`] keeping `lookahead` incoming keys.
    pub(crate) fn finish_swap_nullifier_recovery_with(
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
                "SELECT EXISTS(SELECT 1 FROM ironwood_swap_spend_retention WHERE account_id=?1)",
                [id.0], |r| r.get(0),
            )?;
            if !enabled { return Ok(false); }
            if wallet::fully_scanned_height(db.conn.0)? != Some(through.height)
                || wallet::chain_tip_height(db.conn.0)? != Some(through.height)
                || wallet::get_block_hash(db.conn.0, through.height)? != Some(BlockHash(through.hash))
            {
                return Ok(false);
            }
            db.maintain_restore_discovery(account, lookahead)?;
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
                ",

                params![id.0], |r| r.get(0),
            )?;
            if pending { return Ok(false); }
            // Pending sweeps keep evidence from their earliest possible payment, and a
            // queued candidate from its height. Scanned keys never need it.
            let pending: Option<u32> = db.conn.0.query_row(
                "SELECT MIN(h) FROM (
                    SELECT k.scan_from AS h FROM ironwood_swap_sweeps s
                        JOIN ironwood_receiving_keys k ON k.id = s.receiving_key_id
                        WHERE k.account_id = ?1 AND s.done_height IS NULL
                    UNION ALL SELECT p.height FROM ironwood_swap_payment_recovery p
                        JOIN ironwood_receiving_keys k ON k.id = p.receiving_key_id
                        WHERE k.account_id = ?1)",
                [id.0],
                |r| r.get(0),
            )?;
            let next = pending.unwrap_or(u32::from(through.height).saturating_add(1));
            let old:u32=db.conn.0.query_row("SELECT nullifier_retention_height FROM ironwood_swap_spend_retention WHERE account_id=?1",[id.0],|r|r.get(0))?;
            if next>old {db.conn.0.execute("DELETE FROM ironwood_swap_spend_replay WHERE account_id=?1",[id.0])?;}

            db.conn.0.execute(
                "UPDATE ironwood_swap_spend_retention SET nullifier_retention_height=?2
                 WHERE account_id=?1", params![id.0, next],
            )?;
            wallet::prune_nullifier_map(db.conn.0, through.height.saturating_sub(crate::PRUNING_DEPTH))?;
            Ok(next>u32::from(through.height))
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
        let start = restore_start(self.conn.0, &self.params, id)?;
        let end = u32::from(through)
            .checked_add(1)
            .ok_or_else(|| corrupt("swap recovery height overflow"))?;
        if start >= BlockHeight::from(end) {
            return Err(corrupt("empty spend recovery interval"));
        }
        self.conn.0.execute(
            "INSERT INTO ironwood_swap_spend_retention(account_id,nullifier_retention_height)
             VALUES(?1,?2) ON CONFLICT(account_id) DO UPDATE SET
             nullifier_retention_height=MIN(nullifier_retention_height,excluded.nullifier_retention_height)",
            params![id.0,u32::from(start)],
        )?;
        let previous: Option<u32> = self
            .conn
            .0
            .query_row(
                "SELECT through_height FROM ironwood_swap_spend_replay WHERE account_id=?1",
                [id.0],
                |r| r.get(0),
            )
            .optional()?;
        if previous.is_some_and(|h| h >= u32::from(through)) {
            return Ok(());
        }
        let from = previous
            .map(|h| BlockHeight::from(h.saturating_add(1)))
            .unwrap_or(start)
            .max(start);
        self.conn.0.execute(
            "INSERT INTO ironwood_swap_spend_replay(account_id,through_height) VALUES(?1,?2)
            ON CONFLICT(account_id) DO UPDATE SET through_height=excluded.through_height",
            params![id.0, u32::from(through)],
        )?;
        let range = from..BlockHeight::from(end);
        wallet::scanning::replace_queue_entries::<crate::error::SqliteClientError>(
            self.conn.0,
            &range,
            std::iter::once(ScanRange::from_parts(range.clone(), ScanPriority::Historic)),
            true,
        )?;
        Ok(())
    }
}
