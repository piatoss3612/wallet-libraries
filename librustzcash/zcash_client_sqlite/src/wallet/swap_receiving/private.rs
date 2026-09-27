//! Durable opt-in for private historical discovery. No targeted replay fallback.
use super::{Error, KeyId, account_key, corrupt, payments::key_ref};
use crate::{AccountUuid, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
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
