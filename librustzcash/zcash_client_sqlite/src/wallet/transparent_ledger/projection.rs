//! Projection of an active account's recovered events into the wallet's outputs and spends.
//!
//! Projection writes through the same functions public discovery and local construction use,
//! recording the ledger-event origin. Mixed transactions therefore join on the shared
//! `transactions` row, which keeps its raw data, fee, local creation evidence, and notes; other
//! origins of an output or spend are never removed. Placement or content that disagrees with
//! what the wallet already holds is an integrity failure, never last-writer-wins.

use rusqlite::{OptionalExtension as _, named_params};
use transparent::bundle::TxOut;
use zcash_client_backend::{
    data_api::transparent_ledger::{CommitRejection, IntegrityFailure, ReceiveEvent, SpendEvent},
    wallet::WalletTransparentOutput,
};
use zcash_keys::keys::transparent::gap_limits::GapLimits;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{self, BlockHeight};

use super::ProjectionOrigin;
use crate::{
    AccountUuid, TxRef,
    error::SqliteClientError,
    wallet::transparent::{mark_transparent_utxo_spent, put_transparent_output, update_gap_limits},
};

fn integrity(failure: IntegrityFailure) -> SqliteClientError {
    SqliteClientError::TransparentLedgerCommitRejected(CommitRejection::Integrity(failure))
}

/// Refuses placing `txid` at `mined` when the wallet holds it at another height, or disagrees on
/// whether it is a coinbase transaction. `coinbase` is `None` for a spending transaction, which
/// cannot be a coinbase.
fn check_transaction(
    conn: &rusqlite::Connection,
    txid: &TxId,
    mined: BlockHeight,
    coinbase: Option<bool>,
) -> Result<(), SqliteClientError> {
    let stored = conn
        .query_row(
            "SELECT mined_height, tx_index FROM transactions WHERE txid = :txid",
            named_params![":txid": txid.as_ref()],
            |row| Ok((row.get::<_, Option<u32>>(0)?, row.get::<_, Option<u32>>(1)?)),
        )
        .optional()?;
    let Some((stored_mined, tx_index)) = stored else {
        return Ok(());
    };
    if stored_mined.is_some_and(|h| h != u32::from(mined)) {
        return Err(integrity(IntegrityFailure::TransactionPlacement(*txid)));
    }
    if let Some(index) = tx_index
        && (index == 0) != coinbase.unwrap_or(false)
    {
        return Err(integrity(IntegrityFailure::TransactionCoinbase(*txid)));
    }
    Ok(())
}

/// Projects a placed receive of `account`.
pub(super) fn project_receive<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    account: AccountUuid,
    receive: &ReceiveEvent,
) -> Result<(), SqliteClientError> {
    let outpoint = &receive.outpoint;
    let txid = *outpoint.txid();
    check_transaction(conn, &txid, receive.mined_height, Some(receive.coinbase))?;

    let script = transparent::address::Script::from(receive.address.script());
    let value = i64::try_from(u64::from(receive.value)).expect("Zatoshis fit in i64");
    let content_conflict: bool = conn
        .query_row(
            "SELECT o.script != :script OR o.value_zat != :value
             FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = :txid AND o.output_index = :output_index",
            named_params![
                ":script": script.0.0,
                ":value": value,
                ":txid": txid.as_ref(),
                ":output_index": outpoint.n(),
            ],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if content_conflict {
        return Err(integrity(IntegrityFailure::ProjectionContent(
            outpoint.clone(),
        )));
    }
    // Stored transaction bytes are the wallet's own evidence of what the transaction contains:
    // the receive must name one of its outputs, with its value and script.
    if let Some((_, tx)) = crate::wallet::get_transaction(conn, params, txid)? {
        let bundle = tx.transparent_bundle();
        let stored = bundle.and_then(|b| b.vout.get(usize::try_from(outpoint.n()).ok()?));
        if !stored.is_some_and(|o| o.value() == receive.value && o.script_pubkey() == &script) {
            return Err(integrity(IntegrityFailure::ProjectionContent(
                outpoint.clone(),
            )));
        }
        if bundle.is_some_and(|b| b.is_coinbase()) != receive.coinbase {
            return Err(integrity(IntegrityFailure::TransactionCoinbase(txid)));
        }
    }

    let output = WalletTransparentOutput::from_parts(
        outpoint.clone(),
        TxOut::new(receive.value, script),
        Some(receive.mined_height),
        Some(account),
        None,
        None,
    )
    .ok_or_else(|| {
        SqliteClientError::CorruptedData("recovered receive is not a standard address".into())
    })?;
    put_transparent_output(
        conn,
        params,
        gap_limits,
        &output,
        receive.mined_height,
        false,
        ProjectionOrigin::LedgerEvent,
    )?;
    if receive.coinbase {
        // A coinbase transaction is the first in its block. Recording that keeps the output's
        // maturity rule without its raw bytes; un-mining clears it with the placement.
        conn.execute(
            "UPDATE transactions SET tx_index = 0 WHERE txid = :txid",
            named_params![":txid": txid.as_ref()],
        )?;
    }
    Ok(())
}

/// Projects a placed spend.
pub(super) fn project_spend<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    spend: &SpendEvent,
) -> Result<(), SqliteClientError> {
    check_transaction(conn, &spend.spending_txid, spend.mined_height, None)?;
    // Stored bytes of the spending transaction must spend the prevout at the reported input.
    if let Some((_, tx)) = crate::wallet::get_transaction(conn, params, spend.spending_txid)? {
        let spent = tx
            .transparent_bundle()
            .and_then(|b| b.vin.get(usize::try_from(spend.input_index).ok()?))
            .map(|input| input.prevout().clone());
        if spent.as_ref() != Some(&spend.prevout) {
            return Err(integrity(IntegrityFailure::SpendContent {
                spending_txid: spend.spending_txid,
                input_index: spend.input_index,
            }));
        }
    }
    // An outpoint is spent once on the accepted chain: another mined spender the wallet holds,
    // whether linked to the output or waiting for it, contradicts this one.
    let conflicting: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM transparent_received_output_spends s
             JOIN transparent_received_outputs o ON o.id = s.transparent_received_output_id
             JOIN transactions ot ON ot.id_tx = o.transaction_id
             JOIN transactions st ON st.id_tx = s.transaction_id
             WHERE ot.txid = :prevout_txid AND o.output_index = :prevout_idx
             AND st.mined_height IS NOT NULL AND st.txid != :spending_txid
         ) OR EXISTS (
             SELECT 1 FROM transparent_spend_map m
             JOIN transactions st ON st.id_tx = m.spending_transaction_id
             WHERE m.prevout_txid = :prevout_txid AND m.prevout_output_index = :prevout_idx
             AND st.mined_height IS NOT NULL AND st.txid != :spending_txid
         )",
        named_params![
            ":prevout_txid": spend.prevout.hash(),
            ":prevout_idx": spend.prevout.n(),
            ":spending_txid": spend.spending_txid.as_ref(),
        ],
        |row| row.get(0),
    )?;
    if conflicting {
        return Err(integrity(IntegrityFailure::ConflictingSpends(
            spend.prevout.clone(),
        )));
    }
    let mined = u32::from(spend.mined_height);
    let block: Option<u32> = conn
        .query_row(
            "SELECT height FROM blocks WHERE height = :height",
            named_params![":height": mined],
            |row| row.get(0),
        )
        .optional()?;
    let spending_tx = conn.query_row(
        "INSERT INTO transactions (txid, block, mined_height, min_observed_height)
         VALUES (:txid, :block, :mined_height, :mined_height)
         ON CONFLICT (txid) DO UPDATE
         SET block = IFNULL(block, :block),
             mined_height = :mined_height,
             min_observed_height = MIN(min_observed_height, :mined_height),
             confirmed_unmined_at_height = NULL
         RETURNING id_tx",
        named_params![
            ":txid": spend.spending_txid.as_ref(),
            ":block": block,
            ":mined_height": mined,
        ],
        |row| row.get::<_, i64>(0).map(TxRef),
    )?;
    mark_transparent_utxo_spent(
        conn,
        spending_tx,
        &spend.prevout,
        Some(ProjectionOrigin::LedgerEvent),
    )?;
    update_gap_limits(
        conn,
        params,
        gap_limits,
        spend.spending_txid,
        spend.mined_height,
    )
}
