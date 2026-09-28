//! SQLite storage for the transparent ledger (`tpir_*` tables).
//!
//! Projection origins record why each transparent output and spend exists in the wallet, so
//! that invalidating one source never removes a record another source still supports. Legacy
//! and local origins are provenance only; they never constitute ledger coverage.
//!
//! Handle configuration is enforced here. Until private recovery is implemented, private
//! authority is never available: ledger commits and promotion are rejected, and
//! `PrivateRequired` handles cannot authorize transparent inputs.

use rusqlite::OptionalExtension as _;
use zcash_client_backend::data_api::{
    AccountBalance,
    transparent_ledger::{
        CommitOutcome, CommitRejection, LastKnownBalance, LastKnownSource, PromotionContext,
        PromotionOutcome, PromotionRejection, RecoveryBlocker, RecoveryCompletion,
        RecoveryDiagnostics, TransparentAuthority, TransparentLedgerBalance,
        TransparentLedgerCommit, TransparentLedgerMode, TransparentLedgerSnapshot,
    },
    wallet::{ConfirmationsPolicy, TargetHeight},
};

use crate::{AccountUuid, error::SqliteClientError, wallet::chain_tip_height};

#[cfg(feature = "transparent-inputs")]
use {
    crate::{TxRef, UtxoId},
    rusqlite::named_params,
    transparent::bundle::OutPoint,
};

fn mode_from_code(code: i64) -> Result<TransparentLedgerMode, SqliteClientError> {
    match code {
        0 => Ok(TransparentLedgerMode::Public),
        1 => Ok(TransparentLedgerMode::PrivateShadow),
        2 => Ok(TransparentLedgerMode::PrivateRequired),
        other => Err(SqliteClientError::CorruptedData(format!(
            "unknown transparent ledger mode code {other}"
        ))),
    }
}

#[cfg(test)]
fn mode_code(mode: TransparentLedgerMode) -> i64 {
    match mode {
        TransparentLedgerMode::Public => 0,
        TransparentLedgerMode::PrivateShadow => 1,
        TransparentLedgerMode::PrivateRequired => 2,
    }
}

/// The policy durably applied to the wallet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DurablePolicy {
    pub(crate) mode: TransparentLedgerMode,
    pub(crate) generation: u64,
}

/// Reads the durable policy. A wallet that predates the ledger schema has none, and so cannot
/// hold a stricter policy than any handle.
pub(crate) fn durable_policy(
    conn: &rusqlite::Connection,
) -> Result<Option<DurablePolicy>, SqliteClientError> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tpir_meta')",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(None);
    }
    conn.query_row(
        "SELECT applied_mode, policy_generation FROM tpir_meta WHERE id = 0",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )
    .optional()?
    .map(|(mode, generation)| {
        Ok(DurablePolicy {
            mode: mode_from_code(mode)?,
            generation: u64::try_from(generation).map_err(|_| {
                SqliteClientError::CorruptedData("negative transparent policy generation".into())
            })?,
        })
    })
    .transpose()
}

/// Rejects a handle whose configured mode is weaker than a durably applied private-required
/// policy. The stored policy is never changed here.
fn check_not_weaker(
    configured: Option<TransparentLedgerMode>,
    durable: Option<DurablePolicy>,
) -> Result<(), SqliteClientError> {
    match durable {
        Some(DurablePolicy {
            mode: applied @ TransparentLedgerMode::PrivateRequired,
            ..
        }) if configured != Some(TransparentLedgerMode::PrivateRequired) => {
            Err(SqliteClientError::TransparentLedgerPolicyConflict {
                configured,
                applied,
            })
        }
        _ => Ok(()),
    }
}

/// Resolves the mode a handle operates under for transparent ledger APIs, which require
/// explicit configuration even for an empty wallet.
pub(crate) fn resolve_mode(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<(TransparentLedgerMode, Option<DurablePolicy>), SqliteClientError> {
    let mode = configured.ok_or(SqliteClientError::TransparentLedgerModeNotConfigured)?;
    let durable = durable_policy(conn)?;
    check_not_weaker(configured, durable)?;
    Ok((mode, durable))
}

/// Checks that the handle may authorize consuming transparent inputs.
///
/// Public authority is retained by `Public` and `PrivateShadow` handles, and by unconfigured
/// handles unless the wallet durably requires private authority. Private authority is not yet
/// available, so `PrivateRequired` handles are rejected.
pub(crate) fn check_transparent_authority(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<(), SqliteClientError> {
    check_not_weaker(configured, durable_policy(conn)?)?;
    match configured {
        Some(TransparentLedgerMode::PrivateRequired) => {
            Err(SqliteClientError::TransparentAuthorityUnavailable)
        }
        _ => Ok(()),
    }
}

fn transparent_balance(
    conn: &rusqlite::Connection,
    account: AccountUuid,
    target_height: TargetHeight,
    confirmations_policy: ConfirmationsPolicy,
) -> Result<TransparentLedgerBalance, SqliteClientError> {
    #[allow(unused_mut)]
    let mut balances = std::collections::HashMap::<AccountUuid, AccountBalance>::new();
    #[cfg(feature = "transparent-inputs")]
    super::transparent::add_transparent_account_balances(
        conn,
        target_height,
        confirmations_policy,
        &mut balances,
    )?;
    #[cfg(not(feature = "transparent-inputs"))]
    let _ = (conn, target_height, confirmations_policy);
    let balance = balances.remove(&account).unwrap_or(AccountBalance::ZERO);
    Ok(TransparentLedgerBalance {
        regular: *balance.unshielded_regular_balance(),
        coinbase: *balance.unshielded_coinbase_balance(),
    })
}

/// Builds the snapshot for `account` from the connection's current state. The caller provides
/// the read transaction.
pub(crate) fn snapshot(
    conn: &rusqlite::Connection,
    account: AccountUuid,
    mode: TransparentLedgerMode,
    confirmations_policy: ConfirmationsPolicy,
) -> Result<TransparentLedgerSnapshot<AccountUuid>, SqliteClientError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM accounts WHERE uuid = ?1)",
        [account.0],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(SqliteClientError::AccountUnknown);
    }

    // No account can hold private ledger state yet, so authority follows the mode alone.
    let tip = chain_tip_height(conn)?;
    let balance = tip
        .map(|tip| {
            transparent_balance(
                conn,
                account,
                TargetHeight::from(tip + 1),
                confirmations_policy,
            )
        })
        .transpose()?;
    let mut blockers = vec![];
    let (authority, authorized, last_known, completion) = if mode.retains_public_authority() {
        if balance.is_none() {
            blockers.push(RecoveryBlocker::ChainUnknown);
        }
        (
            TransparentAuthority::Public,
            balance,
            None,
            RecoveryCompletion::NotApplicable,
        )
    } else {
        blockers.push(RecoveryBlocker::PrivateRecoveryUnavailable);
        (
            TransparentAuthority::Unavailable,
            None,
            balance.map(|balance| LastKnownBalance {
                balance,
                source: LastKnownSource::LegacyPublic,
                at: None,
            }),
            RecoveryCompletion::Blocked,
        )
    };

    Ok(TransparentLedgerSnapshot {
        account,
        mode,
        authority,
        target: None,
        covered_through: None,
        settled_through: None,
        authorized,
        last_known,
        recovered_net: None,
        completion,
        blockers,
        diagnostics: RecoveryDiagnostics::default(),
    })
}

/// Validates a commit's context and rejects it: no ledger state is writable yet. Nothing is
/// written.
pub(crate) fn apply_commit(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
    commit: &TransparentLedgerCommit<AccountUuid>,
) -> Result<CommitOutcome, SqliteClientError> {
    let (mode, durable) = resolve_mode(conn, configured)?;
    if commit.context.mode != mode {
        return Ok(CommitOutcome::Rejected(CommitRejection::ModeMismatch));
    }
    if durable.map_or(0, |p| p.generation) != commit.context.policy_generation {
        return Ok(CommitOutcome::Rejected(CommitRejection::StalePolicy));
    }
    Ok(CommitOutcome::Rejected(CommitRejection::Unavailable))
}

/// Validates a promotion request and rejects it: private authority is not yet available.
/// Nothing is changed.
pub(crate) fn promote(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
    context: &PromotionContext<AccountUuid>,
) -> Result<PromotionOutcome, SqliteClientError> {
    let (_, durable) = resolve_mode(conn, configured)?;
    if durable.map_or(0, |p| p.generation) != context.policy_generation {
        return Ok(PromotionOutcome::Rejected(PromotionRejection::StalePolicy));
    }
    Ok(PromotionOutcome::Rejected(PromotionRejection::Unavailable))
}

/// Durably applies `mode` for test fixtures. Production policy transitions are not yet
/// implemented; this exists so tests can model a wallet written by a privacy-aware release.
#[cfg(test)]
pub(crate) fn set_durable_policy_for_testing(
    conn: &rusqlite::Connection,
    mode: TransparentLedgerMode,
    generation: u64,
) -> Result<(), SqliteClientError> {
    conn.execute(
        "UPDATE tpir_meta SET applied_mode = ?1, policy_generation = ?2 WHERE id = 0",
        rusqlite::params![mode_code(mode), i64::try_from(generation).unwrap()],
    )?;
    Ok(())
}

/// Why a transparent output or spend exists in the wallet's projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "transparent-inputs"), allow(dead_code))]
pub(crate) enum ProjectionOrigin {
    /// Written by public discovery, or present before the ledger schema existed.
    LegacyPublic,
    /// Written by local transaction construction.
    LocalConstruction,
}

impl ProjectionOrigin {
    #[cfg_attr(not(feature = "transparent-inputs"), allow(dead_code))]
    fn code(self) -> i64 {
        match self {
            Self::LegacyPublic => 0,
            Self::LocalConstruction => 1,
        }
    }
}

/// Records `origin` for a transparent output. Idempotent.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn record_output_origin(
    conn: &rusqlite::Connection,
    output: UtxoId,
    origin: ProjectionOrigin,
) -> Result<(), SqliteClientError> {
    conn.prepare_cached(
        "INSERT INTO tpir_output_origins (output_id, origin)
         VALUES (:output_id, :origin)
         ON CONFLICT (output_id, origin) DO NOTHING",
    )?
    .execute(named_params![":output_id": output.0, ":origin": origin.code()])?;
    // Local creation evidence may already exist, recorded by an outbox before the transaction
    // was projected; the record then has a local origin whatever path projected it.
    conn.prepare_cached(
        "INSERT INTO tpir_output_origins (output_id, origin)
         SELECT o.id, 1
         FROM transparent_received_outputs o
         JOIN transactions t ON t.id_tx = o.transaction_id
         WHERE o.id = :output_id
         AND (t.created IS NOT NULL OR t.target_height IS NOT NULL)
         ON CONFLICT (output_id, origin) DO NOTHING",
    )?
    .execute(named_params![":output_id": output.0])?;
    Ok(())
}

/// Records `origin` for the spend of `outpoint` by `spent_in_tx`, whether or not the spent
/// output is known yet. Idempotent.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn record_spend_origin(
    conn: &rusqlite::Connection,
    spent_in_tx: TxRef,
    outpoint: &OutPoint,
    origin: ProjectionOrigin,
) -> Result<(), SqliteClientError> {
    conn.prepare_cached(
        "INSERT INTO tpir_spend_origins (
             spending_transaction_id, prevout_txid, prevout_output_index, origin
         )
         VALUES (:spent_in_tx, :prevout_txid, :prevout_idx, :origin)
         ON CONFLICT (spending_transaction_id, prevout_txid, prevout_output_index, origin)
         DO NOTHING",
    )?
    .execute(named_params![
        ":spent_in_tx": spent_in_tx.0,
        ":prevout_txid": outpoint.hash(),
        ":prevout_idx": outpoint.n(),
        ":origin": origin.code(),
    ])?;
    conn.prepare_cached(
        "INSERT INTO tpir_spend_origins (
             spending_transaction_id, prevout_txid, prevout_output_index, origin
         )
         SELECT id_tx, :prevout_txid, :prevout_idx, 1
         FROM transactions
         WHERE id_tx = :spent_in_tx
         AND (created IS NOT NULL OR target_height IS NOT NULL)
         ON CONFLICT (spending_transaction_id, prevout_txid, prevout_output_index, origin)
         DO NOTHING",
    )?
    .execute(named_params![
        ":spent_in_tx": spent_in_tx.0,
        ":prevout_txid": outpoint.hash(),
        ":prevout_idx": outpoint.n(),
    ])?;
    Ok(())
}

/// Adds local origins to the transparent records already projected for `txid`, when local
/// creation evidence is recorded after projection.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn record_local_origins_for_tx(
    conn: &rusqlite::Connection,
    txid: &[u8],
) -> Result<(), SqliteClientError> {
    conn.execute(
        "INSERT INTO tpir_output_origins (output_id, origin)
         SELECT o.id, 1
         FROM transparent_received_outputs o
         JOIN transactions t ON t.id_tx = o.transaction_id
         WHERE t.txid = :txid
         ON CONFLICT (output_id, origin) DO NOTHING",
        named_params![":txid": txid],
    )?;
    conn.execute(
        "INSERT INTO tpir_spend_origins (
             spending_transaction_id, prevout_txid, prevout_output_index, origin
         )
         SELECT so.spending_transaction_id, so.prevout_txid, so.prevout_output_index, 1
         FROM tpir_spend_origins so
         JOIN transactions t ON t.id_tx = so.spending_transaction_id
         WHERE t.txid = :txid
         ON CONFLICT (spending_transaction_id, prevout_txid, prevout_output_index, origin)
         DO NOTHING",
        named_params![":txid": txid],
    )?;
    Ok(())
}

#[cfg(all(test, feature = "transparent-inputs"))]
mod tests;
