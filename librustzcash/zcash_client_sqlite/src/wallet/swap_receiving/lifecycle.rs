//! Local operations enable temporary trial decryption. Restored uses remain directory work.
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
        "SELECT scan_from,scan_through FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1 AND local=1",
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
    /// Persist an API observation immediately. A terminal observation has no height
    /// deadline until `anchor_swap_observations` supplies a later fresh chain view.
    /// `local` is true only for operations initiated on this wallet, never on restore.
    pub fn record_swap_observation(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        operation: &str,
        status: zakura_swap_receiving::lifecycle::OperationStatus,
        now: i64,
        local: bool,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            record_observation(db.conn.0, id, operation, status, now, local)
        })
    }

    /// Anchor observations to a chain request started at `requested_at`. Call only
    /// after independently refreshing and accepting this tip, including its hash.
    /// An observation during that request waits for the next refresh.
    pub fn anchor_swap_observations(
        &mut self,
        through: ChainAnchor,
        requested_at: i64,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            if wallet::get_block_hash(db.conn.0, through.height)? != Some(BlockHash(through.hash)) {
                return Err(corrupt("swap observation anchor changed"));
            }
            let end = CompletionPolicy::default()
                .scan_through(through.height)
                .map_err(|e| corrupt(&e.to_string()))?;
            db.conn.0.execute(
                "UPDATE ironwood_swap_scan_uses SET scan_through=?1,anchor_height=?2
                WHERE terminal_at IS NOT NULL AND terminal_at<?3 AND anchor_height IS NULL",
                params![u32::from(end), u32::from(through.height), requested_at],
            )?;
            // Status outages change the discovery mode, not the operation outcome.
            // A repeatedly confirmed active operation refreshes observed_at and has no age cap.
            db.conn.0.execute(
                "UPDATE ironwood_swap_scan_uses SET local=0
                WHERE terminal_at IS NULL AND observed_at>0 AND observed_at<?1",
                [requested_at.saturating_sub(48 * 60 * 60)],
            )?;
            Ok(())
        })
    }
}

pub(super) fn record_observation(
    conn: &Connection,
    id: i64,
    operation: &str,
    status: zakura_swap_receiving::lifecycle::OperationStatus,
    now: i64,
    local: bool,
) -> Result<(), Error> {
    use zakura_swap_receiving::lifecycle::{OperationStatus, ReceiptExpectation};
    if now < 0 || operation.is_empty() {
        return Err(corrupt("invalid swap observation"));
    }
    let previous:Option<i64>=conn.query_row("SELECT observed_at FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1 AND operation_id=?2",params![id,operation],|r|r.get(0)).optional()?;
    if previous.is_some_and(|at| at > now) {
        return Ok(());
    }
    let expectation = match status {
        OperationStatus::Active => 0,
        OperationStatus::Terminal(ReceiptExpectation::None) => 1,
        OperationStatus::Terminal(ReceiptExpectation::Positive(_)) => 2,
        OperationStatus::Terminal(ReceiptExpectation::Unknown) => 0,
    };
    let amount = match status {
        OperationStatus::Terminal(ReceiptExpectation::Positive(Some(value))) => {
            Some(u64::from(value))
        }
        _ => None,
    };
    if amount == Some(0) {
        return Err(corrupt("expected receipt must be positive"));
    }
    let terminal = matches!(status, OperationStatus::Terminal(_));
    let changed:bool=conn.query_row("SELECT NOT EXISTS(SELECT 1 FROM ironwood_swap_scan_uses
        WHERE receiving_key_id=?1 AND operation_id=?2 AND (terminal_at IS NOT NULL)=?3 AND expectation=?4 AND expected_value IS ?5)",
        params![id,operation,terminal,expectation,amount],|r|r.get(0))?;
    conn.execute("INSERT INTO ironwood_swap_scan_uses
        (receiving_key_id,operation_id,scan_from,local,observed_at,terminal_at,expectation)
        SELECT id,?2,scan_from,?3,?4,CASE WHEN ?5 THEN ?4 END,?6 FROM ironwood_receiving_keys WHERE id=?1
        ON CONFLICT(receiving_key_id,operation_id) DO UPDATE SET
          observed_at=MAX(observed_at,excluded.observed_at),local=MAX(local,excluded.local),
          terminal_at=CASE WHEN ?5 THEN COALESCE(terminal_at,excluded.terminal_at) END,
          expectation=excluded.expectation,
          scan_through=CASE WHEN ?5 THEN scan_through END,
          anchor_height=CASE WHEN ?5 THEN anchor_height END
        WHERE observed_at<=excluded.observed_at",
        params![id,operation,local,now,terminal,expectation])?;
    if changed {
        conn.execute("UPDATE ironwood_swap_discovery SET closed=0,next_attempt_at=0,completed_at=NULL WHERE receiving_key_id=?1",[id])?;
        conn.execute("UPDATE ironwood_swap_scan_uses SET expected_value=?3 WHERE receiving_key_id=?1 AND operation_id=?2",params![id,operation,amount])?;
        // Canonical lookup coverage remains usable. Only the future milestone changes.
        conn.execute(
            "DELETE FROM ironwood_swap_recovery_targets WHERE receiving_key_id=?1",
            [id],
        )?;
    }
    Ok(())
}
