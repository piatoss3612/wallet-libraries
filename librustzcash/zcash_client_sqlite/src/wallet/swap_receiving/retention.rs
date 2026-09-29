//! Temporary spend evidence for notes discovered after ordinary scanning.
use super::{Error, account_key, corrupt};
use crate::{AccountUuid, SqlTransaction, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::BorrowMut;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::scanning::{ScanPriority, ScanRange};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Releases the covered prefix of retained Ironwood nullifiers at a canonical
    /// scanned tip. Missing memos and pending notes protect their required evidence.
    /// Returns true when release reaches the tip, independent of provider completion.
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
                ",

                params![id.0], |r| r.get(0),
            )?;
            if pending { return Ok(false); }
            // The earliest uncovered height, not provider completion, controls
            // retention. A pending candidate may only lower this safe frontier.
            let mut next = u32::from(through.height).saturating_add(1);
            let keys={
                let mut stmt=db.conn.0.prepare("SELECT purpose,derivation_version,key_index,scan_from FROM ironwood_receiving_keys WHERE account_id=?1")?;
                let mut rows=stmt.query([id.0])?; let mut keys=Vec::new();
                while let Some(r)=rows.next()? {keys.push((super::stored_key_id(r)?,r.get::<_,u32>(3)?));}
                keys
            };
            for (key,start) in keys {
                let closed:bool=db.conn.0.query_row("SELECT closed FROM ironwood_swap_discovery WHERE receiving_key_id=?1",[super::payments::key_ref(db.conn.0,account,key)?],|r|r.get(0))?;
                if closed && db.swap_directory_check(account,key)?.is_some() {continue;}
                let mut frontier=start;
                if let Some(check)=db.swap_directory_check(account,key)? {frontier=frontier.max(u32::from(check.height).saturating_add(1));}
                for range in db.get_swap_receiving_scan_ranges(account,key)?.unwrap_or_default() {
                    if u32::from(range.start)>frontier {break;}
                    frontier=frontier.max(u32::from(range.end));
                }
                next=next.min(frontier);
            }
            let pending_height:Option<u32>=db.conn.0.query_row("SELECT MIN(p.height) FROM ironwood_swap_payment_recovery p
                JOIN ironwood_receiving_keys k ON k.id=p.receiving_key_id WHERE k.account_id=?1",[id.0],|r|r.get(0))?;
            if let Some(height)=pending_height {next=next.min(height);}
            let old:u32=db.conn.0.query_row("SELECT nullifier_retention_height FROM ironwood_swap_private_recovery WHERE account_id=?1",[id.0],|r|r.get(0))?;
            if next>old {db.conn.0.execute("DELETE FROM ironwood_swap_spend_replay WHERE account_id=?1",[id.0])?;}

            db.conn.0.execute(
                "UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=?2
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
