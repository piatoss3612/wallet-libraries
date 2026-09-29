//! SQLite storage for the transparent ledger (`tpir_*` tables).
//!
//! Projection origins record why each transparent output and spend exists in the wallet, so
//! that invalidating one source never removes a record another source still supports. Legacy
//! and local origins are provenance only; they never constitute ledger coverage.
//!
//! Handle configuration and the durable policy are enforced here. Until private recovery is
//! implemented, private authority is never available, so `PrivateRequired` handles cannot
//! authorize transparent inputs.

use rusqlite::OptionalExtension as _;
use zcash_client_backend::data_api::{
    transparent_ledger::{
        LastKnownBalance, LastKnownSource, RecoveryBlocker, RecoveryCompletion,
        TransparentAuthority, TransparentLedgerBalance, TransparentLedgerMode,
        TransparentLedgerSnapshot,
    },
    wallet::{ConfirmationsPolicy, TargetHeight},
};

use crate::{AccountUuid, error::SqliteClientError, wallet::chain_tip_height};

#[cfg(feature = "transparent-inputs")]
use {
    crate::{TxRef, UtxoId},
    rusqlite::named_params,
    transparent::bundle::OutPoint,
    zcash_client_backend::data_api::AccountBalance,
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

#[cfg(all(test, feature = "transparent-inputs"))]
fn mode_code(mode: TransparentLedgerMode) -> i64 {
    match mode {
        TransparentLedgerMode::Public => 0,
        TransparentLedgerMode::PrivateShadow => 1,
        TransparentLedgerMode::PrivateRequired => 2,
    }
}

/// The highest `tpir_meta.min_reader_version` this build can interpret. A wallet requiring a
/// newer reader is refused rather than operated on with semantics this build lacks.
pub(crate) const TPIR_READER_VERSION: i64 = 1;

/// The policy durably applied to the wallet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DurablePolicy {
    pub(crate) mode: TransparentLedgerMode,
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
        // Only a wallet that never ran the ledger migration may lack the policy table. Once
        // the migration is recorded, a missing table is damage and must not read as public.
        let migrated: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM schemer_migrations WHERE id = ?1)",
            [super::init::migrations::TRANSPARENT_LEDGER_SCHEMA_ID
                .as_bytes()
                .to_vec()],
            |row| row.get(0),
        )?;
        return if migrated {
            Err(SqliteClientError::CorruptedData(
                "tpir_meta is missing after the transparent ledger migration".into(),
            ))
        } else {
            Ok(None)
        };
    }
    let (mode, min_reader_version) = conn
        .query_row(
            "SELECT applied_mode, min_reader_version FROM tpir_meta WHERE id = 0",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        // Once the table exists, a missing singleton is damage, not the absence of a policy.
        .ok_or_else(|| {
            SqliteClientError::CorruptedData("tpir_meta policy row is missing".into())
        })?;
    if min_reader_version > TPIR_READER_VERSION {
        return Err(SqliteClientError::TransparentLedgerIncompatible {
            required: min_reader_version,
        });
    }
    Ok(Some(DurablePolicy {
        mode: mode_from_code(mode)?,
    }))
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
) -> Result<TransparentLedgerMode, SqliteClientError> {
    let mode = configured.ok_or(SqliteClientError::TransparentLedgerModeNotConfigured)?;
    check_not_weaker(configured, durable_policy(conn)?)?;
    Ok(mode)
}

/// Checks that the handle may authorize consuming transparent inputs.
///
/// Public authority is retained only by explicitly configured `Public` and `PrivateShadow`
/// handles; financial authorization never defaults to public. Private authority is not yet
/// available, so `PrivateRequired` handles are rejected.
pub(crate) fn check_transparent_authority(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<(), SqliteClientError> {
    match resolve_mode(conn, configured)? {
        TransparentLedgerMode::PrivateRequired => {
            Err(SqliteClientError::TransparentAuthorityUnavailable)
        }
        TransparentLedgerMode::Public | TransparentLedgerMode::PrivateShadow => Ok(()),
    }
}

#[cfg(feature = "transparent-inputs")]
/// Returns whether public transparent discovery is permitted for this handle. It requires an
/// explicitly configured mode that retains public authority.
pub(crate) fn public_discovery_permitted(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<bool, SqliteClientError> {
    Ok(resolve_mode(conn, configured)?.retains_public_authority())
}

/// Rejects recording publicly discovered transparent data unless public discovery is permitted.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn check_public_discovery(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<(), SqliteClientError> {
    if public_discovery_permitted(conn, configured)? {
        Ok(())
    } else {
        Err(SqliteClientError::PublicTransparentDiscoveryForbidden)
    }
}

/// Returns whether the whole-wallet summary and direct transparent balance reads may report
/// transparent funds as current.
///
/// Under a required-private policy, whether configured on the handle or durably applied, no
/// current transparent authority exists, so the summary omits those funds; the ledger snapshot
/// reports them as last-known instead. The summary is display-only, so an unconfigured handle
/// on a wallet without a private policy keeps reporting them.
pub(crate) fn summary_includes_transparent(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<bool, SqliteClientError> {
    let durable_private = matches!(
        durable_policy(conn)?,
        Some(DurablePolicy {
            mode: TransparentLedgerMode::PrivateRequired,
            ..
        })
    );
    Ok(!durable_private && configured != Some(TransparentLedgerMode::PrivateRequired))
}

/// Reads the account's transparent balance. Without transparent support this build cannot
/// read transparent state that another build may have written, so it reports none rather
/// than a zero balance.
#[cfg(feature = "transparent-inputs")]
fn transparent_balance(
    conn: &rusqlite::Connection,
    account: AccountUuid,
    target_height: TargetHeight,
    confirmations_policy: ConfirmationsPolicy,
) -> Result<Option<TransparentLedgerBalance>, SqliteClientError> {
    let mut balances = std::collections::HashMap::<AccountUuid, AccountBalance>::new();
    super::transparent::add_transparent_account_balances(
        conn,
        target_height,
        confirmations_policy,
        &mut balances,
    )?;
    let balance = balances.remove(&account).unwrap_or(AccountBalance::ZERO);
    Ok(Some(TransparentLedgerBalance {
        regular: *balance.unshielded_regular_balance(),
        coinbase: *balance.unshielded_coinbase_balance(),
    }))
}

#[cfg(not(feature = "transparent-inputs"))]
fn transparent_balance(
    _: &rusqlite::Connection,
    _: AccountUuid,
    _: TargetHeight,
    _: ConfirmationsPolicy,
) -> Result<Option<TransparentLedgerBalance>, SqliteClientError> {
    Ok(None)
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
    let mut blockers = vec![];
    let chain_known = chain_tip_height(conn)?.is_some();
    let balance = match chain_tip_height(conn)? {
        None => {
            blockers.push(RecoveryBlocker::ChainUnknown);
            None
        }
        Some(tip) => {
            let target = TargetHeight::from(tip + 1);
            let balance = transparent_balance(conn, account, target, confirmations_policy)?;
            if balance.is_none() {
                blockers.push(RecoveryBlocker::TransparentSupportUnavailable);
            }
            balance
        }
    };
    let (authority, authorized, last_known, completion) =
        if !chain_known || (mode.retains_public_authority() && balance.is_none()) {
            // Authority cannot be established before the chain is known, or when this build cannot
            // read transparent state. Unavailable is never reported as public authority.
            (
                TransparentAuthority::Unavailable,
                None,
                None,
                RecoveryCompletion::Blocked,
            )
        } else if mode.retains_public_authority() {
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
                balance
                    .map(|balance| {
                        Ok::<_, SqliteClientError>(LastKnownBalance {
                            balance,
                            source: last_known_source(conn, account)?,
                            at: None,
                        })
                    })
                    .transpose()?,
                RecoveryCompletion::Blocked,
            )
        };

    Ok(TransparentLedgerSnapshot {
        account,
        mode,
        authority,
        authorized,
        last_known,
        completion,
        blockers,
    })
}

/// Classifies the provenance of an account's unspent transparent outputs: legacy public rows
/// alone, or together with rows recorded only by local construction.
fn last_known_source(
    conn: &rusqlite::Connection,
    account: AccountUuid,
) -> Result<LastKnownSource, SqliteClientError> {
    let local_only: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1
             FROM transparent_received_outputs o
             JOIN accounts a ON a.id = o.account_id
             WHERE a.uuid = ?1
             AND NOT EXISTS (
                 SELECT 1 FROM transparent_received_output_spends s
                 WHERE s.transparent_received_output_id = o.id
             )
             AND NOT EXISTS (
                 SELECT 1 FROM tpir_output_origins oo
                 WHERE oo.output_id = o.id AND oo.origin = 0
             )
         )",
        [account.0],
        |row| row.get(0),
    )?;
    Ok(if local_only {
        LastKnownSource::LegacyPublicAndLocal
    } else {
        LastKnownSource::LegacyPublic
    })
}

/// Durably applies `mode` for test fixtures. Production policy transitions are not yet
/// implemented; this exists so tests can model a wallet written by a privacy-aware release.
#[cfg(all(test, feature = "transparent-inputs"))]
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

/// Returns whether `tx` spends any transparent output the wallet has recorded. This reads the
/// feature-independent schema, so it applies in every build.
pub(crate) fn spends_wallet_outputs(
    conn: &rusqlite::Connection,
    tx: &zcash_primitives::transaction::Transaction,
) -> Result<bool, SqliteClientError> {
    let Some(bundle) = tx.transparent_bundle() else {
        return Ok(false);
    };
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS (
             SELECT 1 FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = :prevout_txid AND o.output_index = :prevout_idx
         )",
    )?;
    for input in &bundle.vin {
        let prevout = input.prevout();
        let owned: bool = stmt.query_row(
            rusqlite::named_params![":prevout_txid": prevout.hash(), ":prevout_idx": prevout.n()],
            |row| row.get(0),
        )?;
        if owned {
            return Ok(true);
        }
    }
    Ok(false)
}
