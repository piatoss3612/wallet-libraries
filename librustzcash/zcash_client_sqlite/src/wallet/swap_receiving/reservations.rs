//! Incoming allocation fills old holes without forgetting issued payment instructions.
use super::{
    Discovery, Error, KeyId, Purpose, RegisteredKey, account_key, corrupt, decode_index,
    payments::key_ref, register,
};
use crate::{AccountUuid, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::lifecycle::{ProviderStatus, near_observation};
use zcash_protocol::consensus::{BlockHeight, Parameters};

/// Incoming seed recovery must search at least this many consecutive empty indices.
pub const RECEIVE_GAP_LIMIT: u64 = 30;
/// Grace after the last deposit deadline before an unpaid reservation can be recycled.
pub const RECEIVE_RECLAIM_SECONDS: i64 = 48 * 60 * 60;
/// Maximum number of distinct addresses held by unfunded drafts or swaps per account.
pub const RECEIVE_UNFUNDED_LIMIT: u32 = 3;
const STATUS_FRESH_SECONDS: i64 = 120;

/// A durable draft or started swap, independent of whether it received funds.
pub struct ReceiveReservation {
    /// Stable identity; quote attempts and retries must retain it.
    pub id: i64,
    /// Its receiving key. Never log viewing material.
    pub key: RegisteredKey,
}

/// Provider lookup information retained even when a quote was never started in the UI.
#[derive(Clone, Debug)]
pub struct ReceiveQuote {
    /// Local request identity created before contacting the provider.
    pub request_id: String,
    /// Reservation shared by retries and edits of this draft.
    pub reservation_id: i64,
    /// Provider deposit address used for status lookups.
    pub operation_id: String,
    /// Provider memo required for memo-based deposits.
    pub deposit_memo: Option<String>,
}

pub(super) fn reservation_key(
    conn: &Connection,
    account: AccountUuid,
    id: i64,
) -> Result<i64, Error> {
    conn.query_row("SELECT r.receiving_key_id FROM ironwood_swap_receive_reservations r
        JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id JOIN accounts a ON a.id=k.account_id
        WHERE a.uuid=?1 AND r.id=?2", params![account.0,id], |r| r.get(0)).map_err(Into::into)
}

fn used(conn: &Connection, key: i64) -> Result<bool, Error> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_swap_receive_used WHERE receiving_key_id=?1)",
        [key],
        |r| r.get(0),
    )?)
}

// Reuse relies on the key having been scanned since issuance, so its whole
// active range must be scanned and free of payments and queued candidates.
fn scanned_empty(conn: &Connection, key: i64) -> Result<bool, Error> {
    let active: bool = conn.query_row(
        "SELECT active_from IS NOT NULL AND closed_at IS NULL FROM ironwood_receiving_keys WHERE id=?1",
        [key],
        |r| r.get(0),
    )?;
    let pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery WHERE receiving_key_id=?1)",
        [key],
        |r| r.get(0),
    )?;
    let tip = wallet::chain_tip_height(conn)?;
    Ok(active
        && !pending
        && !used(conn, key)?
        && tip.is_some()
        && wallet::fully_scanned_height(conn)? == tip)
}

// Local issuance and provider deposits cannot move the seed recovery boundary.
fn recovery_end(conn: &Connection, account: i64) -> Result<u64, Error> {
    let paid: Option<Vec<u8>> = conn.query_row(
        "SELECT MAX(k.key_index) FROM ironwood_receiving_keys k
         JOIN ironwood_received_notes n ON n.receiving_key_id=k.id
         JOIN transactions t ON t.id_tx=n.transaction_id JOIN blocks b ON b.height=t.mined_height
         WHERE k.account_id=?1 AND k.purpose=1",
        [account],
        |r| r.get(0),
    )?;
    paid.map(decode_index)
        .transpose()?
        .map(|i| i.checked_add(1).ok_or(Error::IndexExhausted))
        .transpose()?
        .unwrap_or(0)
        .checked_add(RECEIVE_GAP_LIMIT)
        .ok_or(Error::IndexExhausted)
}

// Closing retains all quote associations. The key's scanning ends separately,
// under `close_finished_swap_keys`. Closed quotes are no longer polled, and both
// callers admit only expired unfunded deposits as still open, which expect no receipt.
fn close_reservation(conn: &Connection, id: i64, now: i64) -> Result<(), Error> {
    conn.execute(
        "UPDATE ironwood_swap_receive_reservations SET closed_at=?2 WHERE id=?1",
        params![id, now],
    )?;
    conn.execute(
        "UPDATE ironwood_swap_operations SET terminal_at=?2, expectation=1, expected_value=NULL,
            observed_at=MAX(observed_at, ?2)
         WHERE terminal_at IS NULL
           AND receiving_key_id=(SELECT receiving_key_id FROM ironwood_swap_receive_reservations WHERE id=?1)
           AND operation_id IN (SELECT 'receive-quote:'||request_id FROM ironwood_swap_receive_quotes
               WHERE reservation_id=?1)",
        params![id, now],
    )?;
    Ok(())
}

fn reusable(conn: &Connection, id: i64, now: i64) -> Result<bool, Error> {
    // Unknown request outcomes and unknown statuses stay reserved. A clock change
    // cannot turn a response from the future into a fresh observation.
    Ok(conn.query_row("SELECT r.closed_at IS NULL
        AND r.created_at<=?2-?3
        AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_used u WHERE u.receiving_key_id=r.receiving_key_id)
        AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes q WHERE q.reservation_id=r.id AND q.rejected=0 AND (
            q.operation_id IS NULL OR q.deadline IS NULL OR q.deadline>?2-?3
            OR q.checked_at IS NULL OR q.checked_at<?2-?4 OR q.checked_at>?2
            OR q.status IS NULL OR q.status NOT IN ('PENDING_DEPOSIT','REFUNDED','FAILED')
            OR (q.funded=1 AND q.status='PENDING_DEPOSIT')))
        FROM ironwood_swap_receive_reservations r WHERE r.id=?1", params![id,now,RECEIVE_RECLAIM_SECONDS,STATUS_FRESH_SECONDS], |r| r.get(0))?)
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Refund allocation waits for account-internal funding memos, not unrelated outgoing metadata.
    pub fn swap_refund_memos_pending(&self, account: AccountUuid) -> Result<bool, Error> {
        Ok(self.conn.borrow().query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes n
            JOIN transactions t ON t.id_tx=n.transaction_id JOIN accounts a ON a.id=n.account_id
            WHERE a.uuid=?1 AND n.recipient_key_scope=1 AND n.receiving_key_id IS NULL
            AND t.mined_height IS NOT NULL AND n.memo IS NULL)",
            [account.0],
            |r| r.get(0),
        )?)
    }

    /// Quotes requiring status reconciliation, including locally expired and never-started quotes.
    /// Caller errors must not be recorded as successful observations.
    pub fn swap_receive_quotes_due(
        &self,
        account: AccountUuid,
        now: i64,
    ) -> Result<Vec<ReceiveQuote>, Error> {
        let mut stmt = self.conn.borrow().prepare("SELECT q.request_id,r.id,q.operation_id,q.deposit_memo
            FROM ironwood_swap_receive_quotes q JOIN ironwood_swap_receive_reservations r ON r.id=q.reservation_id
            JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id JOIN accounts a ON a.id=k.account_id
            WHERE a.uuid=?1 AND r.closed_at IS NULL AND q.rejected=0 AND q.operation_id IS NOT NULL
            AND (q.checked_at IS NULL OR q.checked_at<=?2-30 OR q.checked_at>?2)
            ORDER BY r.id,q.requested_at")?;
        Ok(stmt
            .query_map(params![account.0, now], |r| {
                Ok(ReceiveQuote {
                    request_id: r.get(0)?,
                    reservation_id: r.get(1)?,
                    operation_id: r.get(2)?,
                    deposit_memo: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    /// Unpaid reservations old enough to reclaim. This does not release them.
    pub fn swap_receive_reclaim_candidates(
        &self,
        account: AccountUuid,
        now: i64,
    ) -> Result<Vec<i64>, Error> {
        let conn = self.conn.borrow();
        let mut stmt=conn.prepare("SELECT r.id FROM ironwood_swap_receive_reservations r JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id
            JOIN accounts a ON a.id=k.account_id WHERE a.uuid=?1 AND r.closed_at IS NULL ORDER BY k.key_index")?;
        let ids = stmt
            .query_map([account.0], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .filter_map(|id| match reusable(conn, id, now) {
                Ok(true) => Some(Ok(id)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Reloads a reservation under its owning account, including closed history.
    pub fn swap_receive_reservation(
        &self,
        account: AccountUuid,
        id: i64,
    ) -> Result<ReceiveReservation, Error> {
        let key = self.swap_receiving_key_matching(
            account,
            "k.id=(SELECT receiving_key_id FROM ironwood_swap_receive_reservations WHERE id=?2)",
            params![account.0, id],
        )?.ok_or_else(|| corrupt("missing reserved receive key"))?;
        Ok(ReceiveReservation { id, key })
    }

    /// Whether the provider operation is governed by the incoming reservation lifecycle.
    pub fn has_swap_receive_quote(
        &self,
        account: AccountUuid,
        operation: &str,
    ) -> Result<bool, Error> {
        Ok(self.conn.borrow().query_row("SELECT EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes q
            JOIN ironwood_swap_receive_reservations r ON r.id=q.reservation_id JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id
            JOIN accounts a ON a.id=k.account_id WHERE a.uuid=?1 AND q.operation_id=?2)",params![account.0,operation],|r|r.get(0))?)
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Stops polling completed paid reservations while preserving their permanent used marker.
    /// Old unfunded quote edits can finish after the same grace and fresh status checks.
    pub fn close_received_swap_reservations(
        &mut self,
        account: AccountUuid,
        now: i64,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let (a,_)=account_key(db.conn.0,&db.params,account)?;
            let ids={let mut s=db.conn.0.prepare("SELECT r.id FROM ironwood_swap_receive_reservations r
                JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id JOIN ironwood_swap_receive_used u ON u.receiving_key_id=k.id
                WHERE k.account_id=?1 AND r.closed_at IS NULL
                AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes q WHERE q.reservation_id=r.id AND q.rejected=0 AND (
                    q.operation_id IS NULL OR q.status IS NULL OR q.checked_at IS NULL OR q.checked_at>?2
                    OR (q.status NOT IN ('SUCCESS','REFUNDED','FAILED') AND NOT (q.status='PENDING_DEPOSIT' AND q.funded=0
                        AND q.deadline IS NOT NULL AND q.deadline<=?2-?3 AND q.checked_at>=?2-?4))))")?;
                s.query_map(params![a.0,now,RECEIVE_RECLAIM_SECONDS,STATUS_FRESH_SECONDS],|r|r.get::<_,i64>(0))?.collect::<Result<Vec<_>,_>>()?};
            for id in ids {close_reservation(db.conn.0,id,now)?;}
            Ok(())
        })
    }

    /// Atomically resumes the single draft or locks the lowest never-paid free index,
    /// whose key is then scanned from `scan_from`. Does not expose the address.
    /// Waits for pending incoming restore sweeps, which may reveal paid indices.
    /// Only canonical received notes advance the recovery bound; local issuance never does.
    pub fn prepare_swap_receive_reservation(
        &mut self,
        account: AccountUuid,
        now: i64,
        scan_from: BlockHeight,
    ) -> Result<ReceiveReservation, Error> {
        let id=self.transactionally(|db| {
            let (a,_)=account_key(db.conn.0,&db.params,account)?;
            let sweeping:bool=db.conn.0.query_row("SELECT EXISTS(SELECT 1 FROM ironwood_swap_sweeps s
                JOIN ironwood_receiving_keys k ON k.id=s.receiving_key_id
                WHERE k.account_id=?1 AND k.purpose=1 AND s.done_height IS NULL)",[a.0],|r|r.get(0))?;
            if sweeping {return Err(Error::ReservationPolicy(super::ReservationPolicy::Gap));}
            // A received address is permanently excluded, even if later spent or rewound.
            db.conn.0.execute("UPDATE ironwood_swap_receive_reservations SET started=1
                WHERE receiving_key_id IN (SELECT receiving_key_id FROM ironwood_swap_receive_used)",[])?;
            let end=recovery_end(db.conn.0,a.0)?;
            let draft:Option<(i64,Vec<u8>)>=db.conn.0.query_row("SELECT r.id,k.key_index FROM ironwood_swap_receive_reservations r
                JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id WHERE k.account_id=?1
                AND r.closed_at IS NULL AND r.started=0 ORDER BY r.id LIMIT 1",[a.0],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((id,index))=draft {
                if decode_index(index)? >= end {return Err(Error::ReservationPolicy(super::ReservationPolicy::Gap));}
                return Ok::<_,Error>(id);
            }
            let unfunded:u32=db.conn.0.query_row("SELECT COUNT(*) FROM ironwood_swap_receive_reservations r
                JOIN ironwood_receiving_keys k ON k.id=r.receiving_key_id WHERE k.account_id=?1 AND r.closed_at IS NULL
                AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_used u WHERE u.receiving_key_id=k.id)
                AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes q WHERE q.reservation_id=r.id AND q.funded=1)", [a.0],|r|r.get(0))?;
            if unfunded>=RECEIVE_UNFUNDED_LIMIT {return Err(Error::ReservationPolicy(super::ReservationPolicy::Limit));}
            for index in 0..end {
                let blocked:bool=db.conn.0.query_row("SELECT EXISTS(SELECT 1 FROM ironwood_receiving_keys k WHERE k.account_id=?1 AND k.purpose=1 AND k.key_index=?2 AND (
                    EXISTS(SELECT 1 FROM ironwood_swap_receive_used u WHERE u.receiving_key_id=k.id)
                    OR EXISTS(SELECT 1 FROM ironwood_swap_receive_reservations r WHERE r.receiving_key_id=k.id AND r.closed_at IS NULL)
                    OR EXISTS(SELECT 1 FROM ironwood_swap_payment_recovery p WHERE p.receiving_key_id=k.id)))",params![a.0,index.to_be_bytes()],|r|r.get(0))?;
                if blocked {continue;}
                let key=register(db.conn.0,&db.params,account,KeyId::new(Purpose::Receive,index),scan_from,true,Discovery::Scan(scan_from),now)?;
                let key_id=key_ref(db.conn.0,account,key.key_id())?;
                db.conn.0.execute("INSERT INTO ironwood_swap_receive_reservations(receiving_key_id,created_at) VALUES (?1,?2)",params![key_id,now])?;
                return Ok(db.conn.0.last_insert_rowid());
            }
            Err(Error::ReservationPolicy(super::ReservationPolicy::Gap))
        })?;
        self.swap_receive_reservation(account, id)
    }

    /// Saves an unknown quote outcome before a network request. A lost response cannot free it.
    /// Requires the address to be scanned empty through the chain tip, checked atomically.
    pub fn begin_swap_receive_quote(
        &mut self,
        account: AccountUuid,
        reservation: i64,
        request: &str,
        now: i64,
    ) -> Result<(), Error> {
        if request.is_empty() {
            return Err(corrupt("empty receive quote request"));
        }
        self.transactionally(|db| {
            let key=reservation_key(db.conn.0,account,reservation)?;
            let open:bool=db.conn.0.query_row("SELECT closed_at IS NULL AND started=0 FROM ironwood_swap_receive_reservations WHERE id=?1",[reservation],|r|r.get(0))?;
            if !open || used(db.conn.0,key)? {return Err(Error::ReservationPolicy(super::ReservationPolicy::Stale));}
            let (owner,index):(i64,Vec<u8>)=db.conn.0.query_row("SELECT account_id,key_index FROM ironwood_receiving_keys WHERE id=?1",[key],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if decode_index(index)? >= recovery_end(db.conn.0,owner)? {return Err(Error::ReservationPolicy(super::ReservationPolicy::Gap));}
            if !scanned_empty(db.conn.0,key)? {
                return Err(Error::ReservationPolicy(super::ReservationPolicy::Coverage));
            }
            db.conn.0.execute("INSERT INTO ironwood_swap_receive_quotes(request_id,reservation_id,requested_at) VALUES (?1,?2,?3)",params![request,reservation,now])?;
            db.conn.0.execute("INSERT INTO ironwood_swap_operations(receiving_key_id,operation_id,observed_at) VALUES (?1,?2,?3)",params![key,format!("receive-quote:{request}"),now])?;
            Ok(())
        })
    }

    /// Associates an accepted quote with its durable request. Deadlines are provider values.
    pub fn record_swap_receive_quote(
        &mut self,
        account: AccountUuid,
        request: &str,
        operation: &str,
        memo: Option<&str>,
        deadline: i64,
    ) -> Result<(), Error> {
        if operation.is_empty() {
            return Err(corrupt("empty receive quote operation"));
        }
        self.transactionally(|db| {
            let id:i64=db.conn.0.query_row("SELECT reservation_id FROM ironwood_swap_receive_quotes WHERE request_id=?1",[request],|r|r.get(0))?;
            let key=reservation_key(db.conn.0,account,id)?;
            let changed=db.conn.0.execute("UPDATE ironwood_swap_receive_quotes SET operation_id=?2,deposit_memo=?3,deadline=?4
                WHERE request_id=?1 AND rejected=0 AND (operation_id IS NULL OR operation_id=?2)",params![request,operation,memo,deadline])?;
            if changed!=1 {return Err(corrupt("receive quote identity changed"));}
            // The accepted deadline bounds the key's scanning even if status responses omit it.
            db.conn.0.execute("UPDATE ironwood_swap_operations SET deadline=COALESCE(deadline,?3)
                WHERE receiving_key_id=?1 AND operation_id=?2",params![key,format!("receive-quote:{request}"),deadline])?;
            Ok(())
        })
    }

    /// Records a definitive provider rejection, never a timeout or malformed success response.
    pub fn reject_swap_receive_quote(
        &mut self,
        account: AccountUuid,
        request: &str,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id:i64=db.conn.0.query_row("SELECT reservation_id FROM ironwood_swap_receive_quotes WHERE request_id=?1",[request],|r|r.get(0))?;
            let key=reservation_key(db.conn.0,account,id)?;
            db.conn.0.execute("UPDATE ironwood_swap_receive_quotes SET rejected=1 WHERE request_id=?1 AND operation_id IS NULL",[request])?;
            db.conn.0.execute("DELETE FROM ironwood_swap_operations WHERE receiving_key_id=?1 AND operation_id=?2
                AND EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes WHERE request_id=?3 AND rejected=1)",params![key,format!("receive-quote:{request}"),request])?;
            Ok(())
        })
    }

    /// Reserves the draft for this accepted operation before the UI exposes funding instructions.
    pub fn start_swap_receive_quote(
        &mut self,
        account: AccountUuid,
        operation: &str,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let (a,_)=account_key(db.conn.0,&db.params,account)?;
            let changed=db.conn.0.execute("UPDATE ironwood_swap_receive_reservations SET started=1 WHERE closed_at IS NULL
                AND receiving_key_id IN (SELECT id FROM ironwood_receiving_keys WHERE account_id=?1)
                AND id IN (SELECT reservation_id FROM ironwood_swap_receive_quotes WHERE operation_id=?2 AND rejected=0)",params![a.0,operation])?;
            if changed==0 {return Err(Error::ReservationPolicy(super::ReservationPolicy::Stale));} Ok(())
        })
    }

    /// Provider observations are monotonic in request time; deposit evidence is sticky.
    /// Unknown statuses hold the reservation and leave the key's operation alone.
    pub fn observe_swap_receive_quote(
        &mut self,
        account: AccountUuid,
        request: &str,
        status: &ProviderStatus<'_>,
        funded: bool,
        checked_at: i64,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id:i64=db.conn.0.query_row("SELECT reservation_id FROM ironwood_swap_receive_quotes WHERE request_id=?1",[request],|r|r.get(0))?;
            let key=reservation_key(db.conn.0,account,id)?;
            let changed=db.conn.0.execute("UPDATE ironwood_swap_receive_quotes SET status=?2,funded=MAX(funded,?3),checked_at=?4
                WHERE request_id=?1 AND operation_id IS NOT NULL AND (checked_at IS NULL OR checked_at<=?4)",params![request,status.status,funded,checked_at])?;
            if changed==0 {return Ok(());}
            if let Some(observation)=near_observation(Purpose::Receive,status) {
                super::lifecycle::record_observation(db.conn.0,key,&format!("receive-quote:{request}"),observation,checked_at)?;
            }
            Ok(())
        })
    }

    /// Releases an unpaid reservation after successful provider reconciliation, once
    /// local scanning has covered its address through the chain tip without a payment.
    /// The key stays active, so a later reservation reuses it without a scanning gap.
    pub fn reclaim_swap_receive_reservation(
        &mut self,
        account: AccountUuid,
        id: i64,
        now: i64,
    ) -> Result<bool, Error> {
        self.transactionally(|db| {
            let key = reservation_key(db.conn.0, account, id)?;
            if !reusable(db.conn.0, id, now)? || !scanned_empty(db.conn.0, key)? {
                return Ok(false);
            }
            close_reservation(db.conn.0, id, now)?;
            Ok(true)
        })
    }
}
