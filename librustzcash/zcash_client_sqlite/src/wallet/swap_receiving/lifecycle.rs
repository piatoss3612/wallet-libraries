//! Only locally persisted operations enable temporary compact trial decryption.
//! Registry entries and note ownership remain available for PIR and spending.
use super::{Error, KeyId, corrupt, payments::key_ref};
use crate::{AccountUuid, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::lifecycle::{ChainAnchor, CompletionPolicy};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

/// Returns whether the key scans this height and the next height where that may change.
/// Non-private accounts retain their existing historical scanning behavior.
pub(crate) fn scan_window(
    conn: &Connection,
    id: i64,
    height: BlockHeight,
) -> Result<(bool, Option<BlockHeight>), Error> {
    let private: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM ironwood_swap_private_recovery p JOIN ironwood_receiving_keys k ON k.account_id=p.account_id WHERE k.id=?1)", [id], |r| r.get(0))?;
    if !private {
        return Ok((true, None));
    }
    let mut stmt = conn.prepare(
        "SELECT scan_from,scan_through FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1",
    )?;
    let rows = stmt.query_map([id], |r| {
        Ok((r.get::<_, u32>(0)?, r.get::<_, Option<u32>>(1)?))
    })?;
    let mut active = false;
    let mut next = None;
    let h = u32::from(height);
    for row in rows {
        let (start, end) = row?;
        active |= start <= h && end.is_none_or(|end| h <= end);
        for boundary in [Some(start), end.and_then(|end| end.checked_add(1))]
            .into_iter()
            .flatten()
        {
            if boundary > h {
                next = Some(next.map_or(boundary, |n: u32| n.min(boundary)));
            }
        }
    }
    Ok((active, next.map(BlockHeight::from)))
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Fixed, locally anchored recovery target. Tip movement does not extend it.
    pub fn swap_recovery_target(
        &self,
        account: AccountUuid,
        key: KeyId,
    ) -> Result<Option<ChainAnchor>, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let target = conn.query_row("SELECT height,block_hash FROM ironwood_swap_recovery_targets WHERE receiving_key_id=?1", [id], |r| Ok(ChainAnchor { height: BlockHeight::from(r.get::<_,u32>(0)?), hash: r.get(1)? })).optional()?;
        match target {
            Some(a) if wallet::get_block_hash(conn, a.height)? == Some(BlockHash(a.hash)) => {
                Ok(Some(a))
            }
            _ => Ok(None),
        }
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Records a known operation status before exposing a deposit instruction.
    /// Repeated terminal observations preserve the first deadline. Unknown statuses
    /// and network errors must not call this method. A supported pending status can
    /// reopen a terminal operation. Missing operations are never inferred as terminal.
    /// `observed_height` is the wallet's current chain height; the deadline is a height
    /// budget, not a claim that the block at that height remains canonical after a reorg.
    pub fn observe_swap_operation(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        operation: &str,
        terminal: bool,
        observed_height: BlockHeight,
    ) -> Result<(), Error> {
        if operation.is_empty() {
            return Err(corrupt("empty swap operation identifier"));
        }
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            let saved: Option<Option<u32>> = db.conn.0.query_row("SELECT scan_through FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1 AND operation_id=?2", params![id,operation], |r| r.get(0)).optional()?;
            if saved.is_some_and(|end| end.is_some() == terminal) { return Ok(()); }
            let end = if terminal { Some(u32::from(CompletionPolicy::default().scan_through(observed_height).map_err(|e| corrupt(&e.to_string()))?)) } else { None };
            let start: u32 = db.conn.0.query_row("SELECT scan_from FROM ironwood_receiving_keys WHERE id=?1", [id], |r| r.get(0))?;
            db.conn.0.execute("INSERT INTO ironwood_swap_scan_uses(receiving_key_id,operation_id,scan_from,scan_through) VALUES (?1,?2,?3,?4) ON CONFLICT(receiving_key_id,operation_id) DO UPDATE SET scan_through=excluded.scan_through", params![id,operation,start,end])?;
            db.conn.0.execute("DELETE FROM ironwood_swap_recovery_targets WHERE receiving_key_id=?1", [id])?;
            db.conn.0.execute("DELETE FROM ironwood_swap_directory_checks WHERE receiving_key_id=?1", [id])?;
            Ok(())
        })
    }

    /// Saves one closeout target once a local operation's grace height is scanned.
    /// Restored and lookahead keys have no local operation and target the first
    /// accepted restore tip. A pending operation uses local scanning until terminal.
    /// Reorgs remove invalid targets; retries and restarts preserve canonical targets.
    pub fn prepare_swap_recovery_target(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        through: ChainAnchor,
    ) -> Result<Option<ChainAnchor>, Error> {
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            if let Some(saved) = db.swap_recovery_target(account,key)? { return Ok(Some(saved)); }
            let (count,pending,end): (u32,u32,Option<u32>) = db.conn.0.query_row("SELECT COUNT(*),COUNT(*)-COUNT(scan_through),MAX(scan_through) FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            if pending > 0 { return Ok(None); }
            let height = if count == 0 { through.height } else { BlockHeight::from(end.ok_or_else(|| corrupt("missing terminal deadline"))?) };
            if height > through.height { return Ok(None); }
            let start: u32 = db.conn.0.query_row("SELECT scan_from FROM ironwood_receiving_keys WHERE id=?1", [id], |r| r.get(0))?;
            if u32::from(height) < start { return Ok(None); }
            let hash = wallet::get_block_hash(db.conn.0,height)?.ok_or_else(|| corrupt("recovery target has not been scanned"))?;
            db.conn.0.execute("INSERT INTO ironwood_swap_recovery_targets(receiving_key_id,height,block_hash) VALUES (?1,?2,?3) ON CONFLICT(receiving_key_id) DO UPDATE SET height=excluded.height,block_hash=excluded.block_hash", params![id,u32::from(height),hash.0])?;
            Ok(Some(ChainAnchor {height,hash:hash.0}))
        })
    }
}
