//! Outgoing swap funding: the refund quote record, its recovery memo and the proposal shape.
use std::borrow::{Borrow, BorrowMut};

use rusqlite::{Connection, params};
use zakura_swap_receiving::RefundMemo;
use zcash_client_backend::proposal::Proposal;
use zcash_keys::address::Address;
use zcash_protocol::{PoolType, consensus::Parameters, memo::MemoBytes};

use super::{Error, account_key, corrupt};
use crate::{AccountUuid, WalletDb};

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// The recovery memo for funding a refund quote recorded with
    /// [`WalletDb::record_swap_refund_quote`]. Put it on the funding transaction's
    /// internal Ironwood change, and check the proposal with
    /// [`verify_swap_funding_proposal`] before signing.
    pub fn swap_funding_memo(
        &self,
        account: AccountUuid,
        index: u64,
        deposit: &str,
    ) -> Result<MemoBytes, Error> {
        check_deposit(&self.params, deposit)?;
        let conn = self.conn.borrow();
        let key = refund_key(conn, &self.params, account, index)?;
        let quoted: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_swap_operations
             WHERE receiving_key_id = ?1 AND operation_id = ?2)",
            params![key, deposit],
            |r| r.get(0),
        )?;
        if !quoted {
            return Err(corrupt("swap refund quote was not recorded for this key"));
        }
        MemoBytes::from_bytes(&RefundMemo::new(index).encode())
            .map_err(|_| corrupt("invalid swap refund memo"))
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Binds a refund quote's deposit address to the refund key reserved for it, before
    /// the quote is shown. Funding requires this record.
    ///
    /// An unfunded quote expects nothing, so abandoned quotes do not keep the key
    /// scanning. A provider status for `deposit` (see
    /// [`WalletDb::record_swap_observation`]), or recovery of its mined funding memo,
    /// makes the swap's outcome decide when the key closes. Without a status the key
    /// closes [`CompletionPolicy::limit_secs`] after `deadline`.
    ///
    /// [`CompletionPolicy::limit_secs`]: zakura_swap_receiving::lifecycle::CompletionPolicy::limit_secs
    pub fn record_swap_refund_quote(
        &mut self,
        account: AccountUuid,
        index: u64,
        deposit: &str,
        deadline: i64,
        now: i64,
    ) -> Result<(), Error> {
        check_deposit(&self.params, deposit)?;
        if now < 0 || deadline <= now {
            return Err(corrupt("invalid swap refund quote deadline"));
        }
        self.transactionally(|db| {
            let key = refund_key(db.conn.0, &db.params, account, index)?;
            let elsewhere: bool = db.conn.0.query_row(
                "SELECT EXISTS(SELECT 1 FROM ironwood_swap_operations
                 WHERE operation_id = ?2 AND receiving_key_id <> ?1)",
                params![key, deposit],
                |r| r.get(0),
            )?;
            if elsewhere {
                return Err(corrupt("swap deposit address is quoted for another key"));
            }
            // `observed_at` 0 marks a record that no provider status has updated yet.
            db.conn.0.execute(
                "INSERT INTO ironwood_swap_operations
                    (receiving_key_id, operation_id, observed_at, terminal_at, expectation, deadline)
                 VALUES (?1, ?2, 0, ?3, 1, ?4)
                 ON CONFLICT (receiving_key_id, operation_id) DO NOTHING",
                params![key, deposit, now, deadline],
            )?;
            Ok(())
        })
    }
}

/// Checks that `proposal` funds `deposit` in one transaction that carries `memo` on
/// internal Ironwood change, as refund recovery requires. The deposit must be the
/// transaction's only transparent output and its only payment.
pub fn verify_swap_funding_proposal<FeeRuleT, NoteRef>(
    proposal: &Proposal<FeeRuleT, NoteRef>,
    memo: &MemoBytes,
    deposit: &str,
) -> Result<(), Error> {
    if proposal.steps().len() != 1 {
        return Err(corrupt("swap funding must be a single transaction"));
    }
    let step = proposal.steps().first();
    let payments = step.transaction_request().payments();
    if payments.len() != 1
        || payments
            .values()
            .any(|p| p.recipient_address().to_string() != deposit || p.memo().is_some())
    {
        return Err(corrupt("swap funding must pay only the deposit address"));
    }
    let change = step.balance().proposed_change();
    if change
        .iter()
        .any(|c| c.is_ephemeral() || c.output_pool() == PoolType::TRANSPARENT)
        || !change
            .iter()
            .any(|c| c.output_pool() == PoolType::IRONWOOD && c.memo() == Some(memo))
    {
        return Err(corrupt(
            "swap funding requires the recovery memo on internal Ironwood change",
        ));
    }
    Ok(())
}

/// Recovery re-encodes the deposit from the funding transaction's only transparent
/// output, so it must be a canonically encoded P2PKH or P2SH address on this network.
fn check_deposit<P: Parameters>(params: &P, deposit: &str) -> Result<(), Error> {
    match Address::decode(params, deposit) {
        Some(address) if address.encode(params) != deposit => {
            Err(corrupt("invalid swap deposit address for this network"))
        }
        Some(Address::Transparent(_)) => Ok(()),
        Some(_) => Err(corrupt(
            "swap refund recovery requires a transparent deposit address",
        )),
        None => Err(corrupt("invalid swap deposit address for this network")),
    }
}

/// The ID of `account`'s refund key at `index`, which must have been reserved and
/// still be scanned, so its refund cannot be missed.
fn refund_key<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
    index: u64,
) -> Result<i64, Error> {
    let (account_ref, _) = account_key(conn, params, account)?;
    conn.query_row(
        "SELECT id FROM ironwood_receiving_keys
         WHERE account_id = ?1 AND purpose = 0 AND derivation_version = 1 AND key_index = ?2
           AND advances_allocation = 1 AND active_from IS NOT NULL AND closed_at IS NULL",
        params![account_ref.0, index.to_be_bytes()],
        |r| r.get(0),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            corrupt("refund key is not reserved and scanning in this account")
        }
        e => e.into(),
    })
}
