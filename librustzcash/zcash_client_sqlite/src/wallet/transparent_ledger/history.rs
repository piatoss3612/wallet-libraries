//! History completeness, derived from the facts the wallet holds.
//!
//! Nothing here is stored. Completeness follows from the transaction row, the account's recorded
//! outputs and spends, the scan queue, the account's ledger coverage, and queued follow-on work,
//! so a rewind, promotion, account change, or later enhancement changes the result as soon as it
//! changes those facts.

use rusqlite::{OptionalExtension as _, named_params};
use zcash_client_backend::data_api::transparent_ledger::{
    DetailCompleteness, EffectCompleteness, FeeState, HistoryClassification, PoolEffect,
    TransactionHistoryDetails, TransparentLedgerMode,
};
use zcash_primitives::transaction::TxId;
use zcash_protocol::{
    PoolType,
    consensus::{self, BlockHeight},
    value::Zatoshis,
};

use crate::{
    AccountUuid,
    error::SqliteClientError,
    wallet::{
        chain_tip_height,
        encoding::{parse_pool_code, pool_code},
        fully_scanned_height,
    },
};

use super::{policy::pending_details, resolve_mode};

#[cfg(feature = "transparent-inputs")]
use {
    super::recovery::account_ledger,
    zcash_client_backend::data_api::transparent_ledger::AccountLifecycle,
    zcash_keys::keys::transparent::gap_limits::GapLimits,
};

/// The pools this build supports, in the order history entries report them.
fn supported_pools() -> Vec<PoolType> {
    vec![
        #[cfg(feature = "transparent-inputs")]
        PoolType::Transparent,
        PoolType::SAPLING,
        #[cfg(feature = "orchard")]
        PoolType::ORCHARD,
        #[cfg(feature = "orchard")]
        PoolType::IRONWOOD,
    ]
}

/// What the account's transparent evidence can establish, independent of any one transaction.
#[cfg_attr(not(feature = "transparent-inputs"), allow(dead_code))]
enum TransparentDiscovery {
    /// Public discovery holds authority.
    Public,
    /// Private authority applies. An active, unquarantined account's ledger covers every watched
    /// address through `covered_through`; otherwise nothing is covered.
    Private {
        covered_through: Option<BlockHeight>,
    },
}

#[cfg(feature = "transparent-inputs")]
fn transparent_discovery<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    mode: TransparentLedgerMode,
    account: AccountUuid,
) -> Result<TransparentDiscovery, SqliteClientError> {
    if mode.retains_public_authority() {
        return Ok(TransparentDiscovery::Public);
    }
    let ledger = account_ledger(conn, params, gap_limits, account)?;
    let covered_through = (ledger.lifecycle == AccountLifecycle::Active && !ledger.quarantined)
        .then_some(ledger.status.covered_through)
        .flatten();
    Ok(TransparentDiscovery::Private { covered_through })
}

/// The `transactions` row facts a history entry depends on.
struct TransactionFacts {
    id: i64,
    mined_height: Option<BlockHeight>,
    has_full_data: bool,
    fee: Option<Zatoshis>,
    /// The wallet constructed and stored the transaction, recording every input it spent and
    /// every output it created, with recipients and memos. Deleting the funding account deletes
    /// those outputs, and with them this evidence.
    constructed: bool,
    /// The wallet created the transaction, whether or not it stored the construction details. An
    /// outbox records only this evidence; its details live outside the wallet database.
    created_locally: bool,
}

fn transaction_facts(
    conn: &rusqlite::Connection,
    txid: &TxId,
) -> Result<Option<TransactionFacts>, SqliteClientError> {
    conn.query_row(
        "SELECT id_tx, mined_height, raw IS NOT NULL, fee,
                created IS NOT NULL
                    AND EXISTS (SELECT 1 FROM sent_notes s WHERE s.transaction_id = id_tx),
                created IS NOT NULL OR target_height IS NOT NULL
         FROM transactions WHERE txid = :txid",
        named_params![":txid": txid.as_ref()],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<u32>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, bool>(5)?,
            ))
        },
    )
    .optional()?
    .map(
        |(id, mined_height, has_full_data, fee, constructed, created_locally)| {
            Ok(TransactionFacts {
                id,
                mined_height: mined_height.map(BlockHeight::from_u32),
                has_full_data,
                fee: fee.map(zatoshis).transpose()?,
                constructed,
                created_locally,
            })
        },
    )
    .transpose()
}

fn zatoshis(value: i64) -> Result<Zatoshis, SqliteClientError> {
    Zatoshis::from_nonnegative_i64(value)
        .map_err(|_| SqliteClientError::CorruptedData(format!("invalid value {value}")))
}

/// The account's recorded received and spent amounts per pool in the transaction. Empty when the
/// account has no recorded output or spend in it.
fn known_amounts(
    conn: &rusqlite::Connection,
    account_id: i64,
    transaction_id: i64,
) -> Result<Vec<(PoolType, Zatoshis, Zatoshis)>, SqliteClientError> {
    let mut stmt = conn.prepare_cached(
        "SELECT pool, SUM(received), SUM(spent) FROM (
             SELECT ro.pool, ro.value AS received, 0 AS spent
             FROM v_received_outputs ro
             WHERE ro.account_id = :account_id AND ro.transaction_id = :transaction_id
             UNION ALL
             SELECT ro.pool, 0, ro.value
             FROM v_received_outputs ro
             JOIN v_received_output_spends ros
                  ON ros.pool = ro.pool AND ros.received_output_id = ro.id_within_pool_table
             WHERE ro.account_id = :account_id AND ros.transaction_id = :transaction_id
         )
         GROUP BY pool",
    )?;
    let mut rows = stmt.query(named_params![
        ":account_id": account_id,
        ":transaction_id": transaction_id,
    ])?;
    let mut amounts = vec![];
    while let Some(row) = rows.next()? {
        amounts.push((
            parse_pool_code(row.get(0)?)?,
            zatoshis(row.get(1)?)?,
            zatoshis(row.get(2)?)?,
        ));
    }
    Ok(amounts)
}

/// Whether a shielded output the account received or sent in the transaction lacks its memo.
/// Compact scanning does not retrieve memos.
fn has_unretrieved_memo(
    conn: &rusqlite::Connection,
    account_id: i64,
    transaction_id: i64,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM v_received_outputs
             WHERE account_id = :account_id AND transaction_id = :transaction_id
             AND pool != 0 AND memo IS NULL
         ) OR EXISTS (
             SELECT 1 FROM sent_notes
             WHERE from_account_id = :account_id AND transaction_id = :transaction_id
             AND output_pool != 0 AND memo IS NULL
         )",
        named_params![":account_id": account_id, ":transaction_id": transaction_id],
        |row| row.get(0),
    )?)
}

/// The value of the recorded outputs the account sent in the transaction to anyone but itself.
fn sent_elsewhere(
    conn: &rusqlite::Connection,
    account_id: i64,
    transaction_id: i64,
) -> Result<u64, SqliteClientError> {
    let value: i64 = conn.query_row(
        "SELECT COALESCE(SUM(s.value), 0) FROM sent_notes s
         WHERE s.from_account_id = :account_id AND s.transaction_id = :transaction_id
         AND NOT EXISTS (
             SELECT 1 FROM v_received_outputs ro
             WHERE ro.account_id = :account_id AND ro.transaction_id = s.transaction_id
             AND ro.pool = s.output_pool AND ro.output_index = s.output_index
         )",
        named_params![":account_id": account_id, ":transaction_id": transaction_id],
        |row| row.get(0),
    )?;
    Ok(zatoshis(value)?.into_u64())
}

/// Whether the transaction has an output in `pool` that the wallet created for an external
/// address without recording its receipt. Local construction defers the receipt of a shielded
/// payment to one of the wallet's own external addresses until scanning or enhancement finds it,
/// so any such output may be an owned receipt not yet recorded.
fn has_unrecorded_sent_output(
    conn: &rusqlite::Connection,
    transaction_id: i64,
    pool: PoolType,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM sent_notes s
             WHERE s.transaction_id = :transaction_id AND s.output_pool = :pool
             AND s.to_account_id IS NULL
             AND NOT EXISTS (
                 SELECT 1 FROM v_received_outputs ro
                 WHERE ro.transaction_id = s.transaction_id
                 AND ro.pool = s.output_pool AND ro.output_index = s.output_index
             )
         )",
        named_params![":transaction_id": transaction_id, ":pool": pool_code(pool)],
        |row| row.get(0),
    )?)
}

/// Whether a parent of one of the transaction's unresolved transparent inputs is queued for
/// retrieval. Until it arrives, the input may spend one of the account's outputs.
#[cfg(feature = "transparent-inputs")]
fn has_pending_parent(
    conn: &rusqlite::Connection,
    transaction_id: i64,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tx_retrieval_queue q
             WHERE q.query_type = 1 AND q.dependent_transaction_id IS NOT NULL
             AND (
                 q.dependent_transaction_id = :transaction_id
                 OR q.txid IN (
                     SELECT m.prevout_txid FROM transparent_spend_map m
                     WHERE m.spending_transaction_id = :transaction_id
                 )
             )
         )",
        named_params![":transaction_id": transaction_id],
        |row| row.get(0),
    )?)
}

/// Whether an active account's ledger records a spend in `txid`. Such a spend makes the account a
/// party to the transaction even before the output it consumes is recovered. A candidate ledger
/// is isolated from history.
#[cfg(feature = "transparent-inputs")]
fn has_active_ledger_spend(
    conn: &rusqlite::Connection,
    account_id: i64,
    txid: &TxId,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_spend_events s
             JOIN tpir_active_accounts a ON a.account_id = s.account_id
             WHERE s.account_id = :account_id AND s.spending_txid = :txid
         )",
        named_params![":account_id": account_id, ":txid": txid.as_ref()],
        |row| row.get(0),
    )?)
}

/// Whether the account's ledger records a spend in `txid` whose output it has not recovered.
#[cfg(feature = "transparent-inputs")]
fn has_unresolved_spend(
    conn: &rusqlite::Connection,
    account_id: i64,
    txid: &TxId,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_spend_events s
             WHERE s.account_id = :account_id AND s.spending_txid = :txid
             AND NOT EXISTS (
                 SELECT 1 FROM tpir_receive_events r
                 WHERE r.account_id = :account_id AND r.mined_height IS NOT NULL
                 AND r.txid = s.prevout_txid AND r.output_index = s.prevout_output_index
             )
         )",
        named_params![":account_id": account_id, ":txid": txid.as_ref()],
        |row| row.get(0),
    )?)
}

/// Returns `account`'s history view of each of `txids` that it has a recorded output or spend
/// in, in request order. The caller provides the read snapshot.
pub(crate) fn transaction_history_details<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    #[cfg_attr(not(feature = "transparent-inputs"), allow(unused_variables))] params: &P,
    #[cfg(feature = "transparent-inputs")] gap_limits: &GapLimits,
    configured: Option<TransparentLedgerMode>,
    account: AccountUuid,
    txids: &[TxId],
) -> Result<Vec<TransactionHistoryDetails>, SqliteClientError> {
    let mode = resolve_mode(conn, configured)?;
    // Scanning detects an account's shielded spends only through the nullifiers its full viewing
    // key derives; an account imported from an incoming viewing key never learns them.
    let (account_id, detects_shielded_spends): (i64, bool) = conn
        .query_row(
            "SELECT id, ufvk IS NOT NULL FROM accounts WHERE uuid = :uuid",
            named_params![":uuid": account.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(SqliteClientError::AccountUnknown)?;
    let fully_scanned = fully_scanned_height(conn)?;
    let tip = chain_tip_height(conn)?;
    #[cfg(feature = "transparent-inputs")]
    let transparent = transparent_discovery(conn, params, gap_limits, mode, account)?;

    let mut entries = vec![];
    for txid in txids {
        let Some(tx) = transaction_facts(conn, txid)? else {
            continue;
        };
        let known = known_amounts(conn, account_id, tx.id)?;
        #[cfg(feature = "transparent-inputs")]
        let involved = !known.is_empty() || has_active_ledger_spend(conn, account_id, txid)?;
        #[cfg(not(feature = "transparent-inputs"))]
        let involved = !known.is_empty();
        if !involved {
            continue;
        }

        let scanned = tx
            .mined_height
            .is_some_and(|mined| fully_scanned.is_some_and(|scanned| mined <= scanned));
        let completeness = |pool: PoolType| -> Result<EffectCompleteness, SqliteClientError> {
            if tx.constructed {
                // The wallet built and stored it, recording every input it spent and every
                // output to its own transparent or internal addresses. A shielded payment to one
                // of its external addresses is recorded only once scanning or enhancement finds
                // it.
                return Ok(match pool {
                    PoolType::Shielded(_)
                        if !scanned && has_unrecorded_sent_output(conn, tx.id, pool)? =>
                    {
                        EffectCompleteness::Incomplete
                    }
                    _ => EffectCompleteness::Complete,
                });
            }
            Ok(match pool {
                PoolType::Transparent => {
                    #[cfg(feature = "transparent-inputs")]
                    match &transparent {
                        // Public discovery is authoritative only once its own pending input
                        // work is done.
                        TransparentDiscovery::Public if has_pending_parent(conn, tx.id)? => {
                            EffectCompleteness::Incomplete
                        }
                        TransparentDiscovery::Public => EffectCompleteness::PublicDiscovery,
                        TransparentDiscovery::Private { covered_through } => {
                            match (tx.mined_height, *covered_through) {
                                (Some(mined), Some(covered))
                                    if mined <= covered
                                        && !has_unresolved_spend(conn, account_id, txid)? =>
                                {
                                    EffectCompleteness::Complete
                                }
                                _ => EffectCompleteness::Incomplete,
                            }
                        }
                    }
                    #[cfg(not(feature = "transparent-inputs"))]
                    EffectCompleteness::Incomplete
                }
                // Without a full viewing key, no amount of scanning reveals the account's spends.
                PoolType::Shielded(_) if !detects_shielded_spends => EffectCompleteness::Incomplete,
                // Scanning a block finds every owned output and every spend of an output found
                // earlier, so everything through the contiguously scanned height is known. An
                // unmined transaction's full data reveals its owned outputs, but its spends are
                // linked only to notes already found: every note it could spend is known only once
                // the scanned chain reaches the tip.
                PoolType::Shielded(_) => match tx.mined_height {
                    Some(_) if scanned => EffectCompleteness::Complete,
                    None if tx.has_full_data
                        && fully_scanned.is_some_and(|scanned| Some(scanned) >= tip) =>
                    {
                        EffectCompleteness::Complete
                    }
                    _ => EffectCompleteness::Incomplete,
                },
            })
        };

        let mut effects = vec![];
        for pool in supported_pools() {
            let (received, spent) = known
                .iter()
                .find(|(p, _, _)| *p == pool)
                .map_or((Zatoshis::ZERO, Zatoshis::ZERO), |(_, r, s)| (*r, *s));
            effects.push(PoolEffect {
                pool,
                received,
                spent,
                completeness: completeness(pool)?,
            });
        }
        let settled = effects.iter().all(|e| e.completeness.is_settled());
        let spent: u64 = known.iter().map(|(_, _, spent)| spent.into_u64()).sum();
        let received: u64 = known
            .iter()
            .map(|(_, received, _)| received.into_u64())
            .sum();

        // The account provably only received: every effect is settled and none is a spend. Creation
        // evidence without the stored construction details means the wallet likely funded the
        // transaction through spends it has not recorded.
        let received_only = settled && spent == 0 && !(tx.created_locally && !tx.constructed);
        // Every unit the account spent is accounted for by what it received back, the recorded
        // outputs it sent elsewhere, and the fee, so no unknown payment of its funds remains.
        // Full data alone proves nothing: outputs that cannot be decrypted are not recorded.
        let payments_accounted = match tx.fee {
            Some(fee) if settled && spent > 0 => {
                let sent = sent_elsewhere(conn, account_id, tx.id)?;
                Some(spent)
                    == received
                        .checked_add(sent)
                        .and_then(|v| v.checked_add(fee.into_u64()))
            }
            _ => false,
        };
        let payment_details = if tx.constructed
            || ((received_only || payments_accounted)
                && !has_unretrieved_memo(conn, account_id, tx.id)?)
        {
            DetailCompleteness::Complete
        } else {
            DetailCompleteness::Incomplete
        };
        let fee = match tx.fee {
            Some(fee) if spent > 0 => FeeState::Known(fee),
            _ if received_only => FeeState::NotApplicable,
            _ => FeeState::Unknown,
        };
        // A missing memo does not change what the transaction did; missing effects or payments
        // can.
        let classification = if tx.created_locally {
            HistoryClassification::LocalIntent
        } else if received_only || payments_accounted {
            HistoryClassification::Reconstructed
        } else {
            HistoryClassification::Provisional
        };

        entries.push(TransactionHistoryDetails {
            txid: *txid,
            mined_height: tx.mined_height,
            effects,
            payment_details,
            fee,
            classification,
            pending_private_details: pending_details(conn, mode, Some(tx.id))?,
        });
    }
    Ok(entries)
}
