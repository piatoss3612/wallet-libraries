//! Durable discovery scheduling. Preparing metadata does not derive viewing keys.
use super::{Error, KeyId, PendingPayment, account_key, corrupt, payments::key_ref, stored_key_id};
use crate::{AccountUuid, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Parameters};

/// One receiver's fixed recovery milestone. Public metadata is not note ownership.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveryWork {
    /// Derivation identity. Derive only when authenticating returned notes.
    pub key: KeyId,
    /// Canonical address bytes already stored at registration.
    pub receiver: [u8; 43],
    /// Persisted, independently accepted target that retries must reach.
    pub target: ChainAnchor,
    /// A completed lookup whose candidates are durably queued. Resume those first.
    pub lookup: Option<ChainAnchor>,
}
/// Bounded work plus the entire job's due, uncached lookup count for transport choice.
pub struct DiscoveryBatch {
    /// At most the requested number of records. Taking a batch does not lease its tail.
    pub work: Vec<DiscoveryWork>,
    /// Remaining due receivers needing network discovery, not lifetime registrations.
    pub remaining_lookups: usize,
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Last atomically persisted lookup, independent of candidate application.
    pub fn swap_lookup_coverage(
        &self,
        account: AccountUuid,
        key: KeyId,
    ) -> Result<Option<ChainAnchor>, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let anchor = conn
            .query_row(
                "SELECT lookup_height,lookup_hash FROM ironwood_swap_discovery
            WHERE receiving_key_id=?1 AND lookup_height IS NOT NULL",
                [id],
                |r| {
                    Ok(ChainAnchor {
                        height: BlockHeight::from(r.get::<_, u32>(0)?),
                        hash: r.get(1)?,
                    })
                },
            )
            .optional()?;
        match anchor {
            Some(anchor)
                if wallet::get_block_hash(conn, anchor.height)? == Some(BlockHash(anchor.hash)) =>
            {
                Ok(Some(anchor))
            }
            _ => Ok(None),
        }
    }
}
impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Select due metadata without reconstructing all historical keys. Attempts are
    /// leased separately immediately before I/O, so a stopped batch cannot starve its tail.
    pub fn prepare_swap_discovery_batch(
        &mut self,
        account: AccountUuid,
        through: ChainAnchor,
        now: i64,
        limit: std::num::NonZeroU32,
    ) -> Result<DiscoveryBatch, Error> {
        if now < 0 {
            return Err(corrupt("invalid recovery time"));
        }
        self.transactionally(|db| {
            let (owner,_)=account_key(db.conn.0,&db.params,account)?;
            if wallet::get_block_hash(db.conn.0,through.height)?!=Some(BlockHash(through.hash)) {
                return Err(corrupt("discovery target is not canonical"));
            }
            // Filtering and counting stay in SQLite. Only this batch's targets
            // are materialized, avoiding a full registry walk for every 64 keys.
            let eligible="FROM ironwood_swap_discovery d JOIN ironwood_receiving_keys k ON k.id=d.receiving_key_id
                LEFT JOIN ironwood_swap_directory_checks c ON c.receiving_key_id=k.id
                LEFT JOIN ironwood_swap_recovery_targets t ON t.receiving_key_id=k.id
                WHERE k.account_id=?1 AND k.scan_from<=?2 AND d.closed=0 AND d.next_attempt_at<=?3
                AND (EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery p WHERE p.receiving_key_id=k.id)
                  OR (NOT EXISTS(SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id
                      AND u.local=1 AND (u.scan_through IS NULL OR u.scan_through>=?2))
                    AND (c.height IS NULL OR d.completed_at IS NULL
                      OR EXISTS(SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id AND u.expectation=2
                        AND (SELECT COALESCE(SUM(n.value),0) FROM ironwood_received_notes n JOIN transactions tx ON tx.id_tx=n.transaction_id
                          JOIN blocks b ON b.height=tx.mined_height WHERE n.receiving_key_id=k.id AND tx.mined_height<=?2)<MAX(1,COALESCE(u.expected_value,1)))
                      OR EXISTS(SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id AND u.terminal_at IS NULL)
                      OR (EXISTS(SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id)
                        AND NOT EXISTS(SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id
                          AND (u.anchor_height IS NULL OR u.scan_through>?2 OR u.terminal_at>?3-43200))))))";
            let remaining_lookups=db.conn.0.query_row(&format!("SELECT COUNT(*) {eligible}
                AND (d.lookup_height IS NULL OR d.lookup_height < CASE WHEN t.height>COALESCE(c.height,-1) THEN t.height ELSE ?2 END)"),
                params![owner.0,u32::from(through.height),now],|r|r.get::<_,usize>(0))?;
            let metadata={
                let mut stmt=db.conn.0.prepare(&format!("SELECT k.purpose,k.derivation_version,k.key_index,k.id,k.receiver {eligible}
                    ORDER BY d.next_attempt_at,d.receiving_key_id LIMIT ?4"))?;
                let mut rows=stmt.query(params![owner.0,u32::from(through.height),now,limit.get()])?;
                let mut ids=Vec::new();
                while let Some(r)=rows.next()? {ids.push((stored_key_id(r)?,r.get::<_,i64>(3)?,r.get::<_,Vec<u8>>(4)?));}
                ids
            };
            let mut work=Vec::new();
            for (key,id,receiver) in metadata {
                let checked=db.swap_directory_check(account,key)?;
                let queued:bool=db.conn.0.query_row("SELECT EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery WHERE receiving_key_id=?1)",[id],|r|r.get(0))?;
                let target=match db.swap_recovery_target(account,key)? {
                    Some(t) if checked.is_none_or(|c| c.height<t.height) || queued => t,
                    _ => {
                        db.conn.0.execute("INSERT INTO ironwood_swap_recovery_targets(receiving_key_id,height,block_hash)
                            VALUES(?1,?2,?3) ON CONFLICT(receiving_key_id) DO UPDATE SET height=excluded.height,block_hash=excluded.block_hash",
                            params![id,u32::from(through.height),through.hash])?;
                        through
                    }
                };
                let lookup=db.swap_lookup_coverage(account,key)?.filter(|a|a.height>=target.height);
                if work.len()<(limit.get() as usize) {
                    work.push(DiscoveryWork{key,receiver:receiver.try_into().map_err(|_|corrupt("invalid stored receiver"))?,target,lookup});
                }
            }
            Ok(DiscoveryBatch{work,remaining_lookups})
        })
    }

    /// Lease only work whose network attempt is starting. Failure or process exit
    /// retains its target and backs off from one minute to twelve hours.
    pub fn begin_swap_discovery_attempt(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        now: i64,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id=key_ref(db.conn.0,account,key)?;
            let attempt:u32=db.conn.0.query_row("SELECT attempts FROM ironwood_swap_discovery WHERE receiving_key_id=?1",[id],|r|r.get(0))?;
            let delay=(60i64<<attempt.min(10)).min(43200);
            db.conn.0.execute("UPDATE ironwood_swap_discovery SET attempts=MIN(attempts+1,30),next_attempt_at=?2 WHERE receiving_key_id=?1",params![id,now.saturating_add(delay)])?;
            Ok(())
        })
    }

    /// Atomically persist an entire validated lookup and authenticated ciphertexts.
    /// No balance is credited here. The caller validates the publication's complete
    /// coverage and pagination before invoking this method, including for empty results.
    pub fn queue_swap_lookup(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainAnchor,
        payments: &[PendingPayment],
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            if wallet::get_block_hash(db.conn.0, anchor.height)? != Some(BlockHash(anchor.hash)) {
                return Err(corrupt("lookup anchor changed"));
            }
            for payment in payments {
                if payment.height > anchor.height {
                    return Err(corrupt("payment exceeds lookup coverage"));
                }
                db.queue_swap_payment(account, key, payment)?;
            }
            db.conn.0.execute(
                "UPDATE ironwood_swap_discovery SET lookup_height=?2,lookup_hash=?3
                WHERE receiving_key_id=?1 AND (lookup_height IS NULL OR lookup_height<=?2)",
                params![id, u32::from(anchor.height), anchor.hash],
            )?;
            Ok(())
        })
    }

    /// Schedule follow-up after all candidates have been applied. Provider completion
    /// alone cannot satisfy an expected payment. Shared receivers remain unresolved
    /// when individual operation attribution cannot be established.
    pub fn finish_swap_discovery_attempt(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainAnchor,
        now: i64,
    ) -> Result<bool, Error> {
        self.transactionally(|db| {
            super::private::mark_checked(db.conn.0,account,key,anchor)?;
            let id=key_ref(db.conn.0,account,key)?;
            let (uses,unresolved,expected,last_terminal):(u32,u32,u32,Option<i64>)=db.conn.0.query_row(
                "SELECT COUNT(*),COALESCE(SUM(terminal_at IS NULL OR anchor_height IS NULL OR scan_through>?2),0),
                 COALESCE(SUM(expectation=2),0),MAX(terminal_at) FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1",
                params![id,u32::from(anchor.height)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let paid:bool=db.conn.0.query_row("SELECT COALESCE(SUM(n.value),0)>=MAX(1,COALESCE((SELECT MAX(expected_value) FROM ironwood_swap_scan_uses WHERE receiving_key_id=?1),1)) FROM ironwood_received_notes n
                JOIN transactions t ON t.id_tx=n.transaction_id JOIN blocks b ON b.height=t.mined_height
                WHERE n.receiving_key_id=?1 AND n.value>0 AND t.mined_height<=?2",params![id,u32::from(anchor.height)],|r|r.get(0))?;
            let complete=uses==0 || (unresolved==0 && last_terminal.is_some_and(|t|now>=t.saturating_add(43200)) && (expected==0 || (uses==1 && paid)));
            let attempts:u32=db.conn.0.query_row("SELECT attempts FROM ironwood_swap_discovery WHERE receiving_key_id=?1",[id],|r|r.get(0))?;
            let retry=now.saturating_add((3600i64<<attempts.saturating_sub(1).min(4)).min(43200));
            let final_check=last_terminal.map(|t|t.saturating_add(43200)).filter(|t|*t>now);
            let due=if expected>0 && !paid {final_check.map_or(retry,|t|t.min(retry))} else {final_check.unwrap_or(retry)};
            db.conn.0.execute("UPDATE ironwood_swap_discovery SET closed=?2,completed_at=?3,next_attempt_at=?4 WHERE receiving_key_id=?1",params![id,complete,now,due])?;
            Ok(complete)
        })
    }
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Whether an already imported output agrees with this directory location.
    /// A conflicting identity fails rather than treating it as another payment.
    pub fn has_swap_payment(
        &self,
        account: AccountUuid,
        key: KeyId,
        txid: zcash_primitives::transaction::TxId,
        action: u32,
        height: BlockHeight,
        hash: BlockHash,
        position: u32,
    ) -> Result<bool, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let found:Option<(i64,u32,u32)>=conn.query_row("SELECT n.receiving_key_id,t.mined_height,n.commitment_tree_position
            FROM ironwood_received_notes n JOIN transactions t ON t.id_tx=n.transaction_id
            WHERE t.txid=?1 AND n.action_index=?2 AND t.mined_height IS NOT NULL AND n.commitment_tree_position IS NOT NULL",
            params![txid.as_ref(),action],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((owner, h, p)) = found {
            if owner != id
                || h != u32::from(height)
                || p != position
                || wallet::get_block_hash(conn, height)? != Some(hash)
            {
                return Err(corrupt("conflicting recovered payment identity"));
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Historical recovery is distinct from future provider follow-ups. Retry
    /// backoff must not make an unfinished restore appear complete to the caller.
    pub fn swap_history_pending(
        &self,
        account: AccountUuid,
        through: BlockHeight,
    ) -> Result<bool, Error> {
        let conn = self.conn.borrow();
        let (owner, _) = account_key(conn, &self.params, account)?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_receiving_keys k
            LEFT JOIN ironwood_swap_directory_checks c ON c.receiving_key_id=k.id
            LEFT JOIN ironwood_swap_recovery_targets t ON t.receiving_key_id=k.id
            WHERE k.account_id=?1 AND k.scan_from<=?2 AND
            (EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery p WHERE p.receiving_key_id=k.id)
            OR ((c.height IS NULL OR t.height>c.height) AND NOT EXISTS(
                SELECT 1 FROM ironwood_swap_scan_uses u WHERE u.receiving_key_id=k.id
                AND u.local=1 AND (u.scan_through IS NULL OR u.scan_through>=?2)))))",
            params![owner.0, u32::from(through)],
            |r| r.get(0),
        )?)
    }
}
