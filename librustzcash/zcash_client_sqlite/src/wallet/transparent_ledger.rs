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
    transparent_ledger::{
        AccountLifecycle, ChainPoint, CommitOutcome, CommitRejection, LastKnownBalance,
        LastKnownSource, LedgerLifecycle, OutstandingPage, PageId, PendingPage, PromotionContext,
        PromotionOutcome, PromotionRejection, PublicationAnchor, PublicationStatus,
        RecoveryBlocker, RecoveryCompletion, RecoveryDiagnostics, RecoveryStart, RevisionId,
        SourceId, SourceRevision, TransparentAuthority, TransparentLedgerBalance,
        TransparentLedgerCommit, TransparentLedgerMode, TransparentLedgerSnapshot, WatchedAccount,
        WatchedScript, WatchedScriptSnapshot,
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
    conn.query_row(
        "SELECT applied_mode, policy_generation, min_reader_version FROM tpir_meta WHERE id = 0",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()?
    // Once the table exists, a missing singleton is damage, not the absence of a policy.
    .ok_or_else(|| SqliteClientError::CorruptedData("tpir_meta policy row is missing".into()))
    .map(Some)?
    .map(|(mode, generation, min_reader_version)| {
        if min_reader_version > TPIR_READER_VERSION {
            return Err(SqliteClientError::TransparentLedgerIncompatible {
                required: min_reader_version,
            });
        }
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
/// Public authority is retained only by explicitly configured `Public` and `PrivateShadow`
/// handles; financial authorization never defaults to public. Private authority is not yet
/// available, so `PrivateRequired` handles are rejected.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn check_transparent_authority(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<(), SqliteClientError> {
    match resolve_mode(conn, configured)?.0 {
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
    Ok(resolve_mode(conn, configured)?.0.retains_public_authority())
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

/// Returns whether the whole-wallet summary may report transparent funds as current.
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

/// Reads the watched scripts and the generations a recovery run must capture.
///
/// Script enumeration is not yet implemented, so accounts are listed with their current
/// generations and no scripts; that is not evidence of empty history.
pub(crate) fn watched_scripts(
    conn: &rusqlite::Connection,
    durable: Option<DurablePolicy>,
) -> Result<WatchedScriptSnapshot<AccountUuid>, SqliteClientError> {
    let mut accounts = vec![];
    let mut stmt = conn.prepare(
        "SELECT a.uuid, IFNULL(s.watch_generation, 0), s.lifecycle, IFNULL(s.quarantined, 0)
         FROM accounts a
         LEFT JOIN tpir_account_state s ON s.account_id = a.id
         ORDER BY a.id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        accounts.push(WatchedAccount {
            account: AccountUuid(row.get(0)?),
            watch_generation: to_u64(row.get(1)?)?,
            lifecycle: match row.get::<_, Option<i64>>(2)? {
                None => AccountLifecycle::LegacyPublic,
                Some(0) => AccountLifecycle::Candidate,
                Some(1) => AccountLifecycle::Active,
                Some(other) => {
                    return Err(SqliteClientError::CorruptedData(format!(
                        "unknown transparent ledger lifecycle {other}"
                    )));
                }
            },
            quarantined: row.get(3)?,
        });
    }
    let scripts = conn
        .prepare(
            "SELECT a.uuid, s.script, s.required_from
             FROM tpir_scripts s
             JOIN accounts a ON a.id = s.account_id
             ORDER BY s.id",
        )?
        .query_map([], |row| {
            Ok(WatchedScript {
                account: AccountUuid(row.get(0)?),
                script: transparent::address::Script(zcash_script::script::Code(row.get(1)?)),
                required_from: match row.get::<_, Option<u32>>(2)? {
                    Some(height) => RecoveryStart::From(height.into()),
                    None => RecoveryStart::Unknown,
                },
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(WatchedScriptSnapshot {
        policy_generation: durable.map_or(0, |p| p.generation),
        accounts,
        scripts,
    })
}

fn to_u64(value: i64) -> Result<u64, SqliteClientError> {
    u64::try_from(value)
        .map_err(|_| SqliteClientError::CorruptedData(format!("negative ledger counter {value}")))
}

fn to_height(value: i64) -> Result<zcash_protocol::consensus::BlockHeight, SqliteClientError> {
    u32::try_from(value)
        .map(Into::into)
        .map_err(|_| SqliteClientError::CorruptedData(format!("invalid ledger height {value}")))
}

fn to_hash(bytes: Vec<u8>) -> Result<zcash_primitives::block::BlockHash, SqliteClientError> {
    zcash_primitives::block::BlockHash::try_from_slice(&bytes)
        .ok_or_else(|| SqliteClientError::CorruptedData("invalid ledger block hash".into()))
}

fn opaque<T>(
    bytes: Vec<u8>,
    new: impl FnOnce(
        Vec<u8>,
    )
        -> Result<T, zcash_client_backend::data_api::transparent_ledger::OpaqueIdError>,
) -> Result<T, SqliteClientError> {
    new(bytes).map_err(|e| SqliteClientError::CorruptedData(format!("invalid ledger id: {e:?}")))
}

/// Reads every durable pending page with its source revision, captured context, and affected
/// watched scripts.
pub(crate) fn pending_pages(
    conn: &rusqlite::Connection,
) -> Result<Vec<OutstandingPage>, SqliteClientError> {
    let mut scripts_stmt = conn.prepare(
        "SELECT s.script FROM tpir_pending_page_scripts ps
         JOIN tpir_scripts s ON s.id = ps.script_id
         WHERE ps.pending_page_id = ?1
         ORDER BY s.id",
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, source_id, revision_id, lineage, sealed, anchor_height, anchor_hash,
                page_id, from_height, to_height, lifecycle, policy_generation,
                target_height, target_hash
         FROM tpir_pending_pages
         ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut pages = vec![];
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let scripts = scripts_stmt
            .query_map([id], |r| {
                Ok(transparent::address::Script(zcash_script::script::Code(
                    r.get(0)?,
                )))
            })?
            .collect::<Result<_, _>>()?;
        pages.push(OutstandingPage {
            source: SourceRevision {
                source: opaque(row.get(1)?, SourceId::new)?,
                revision: opaque(row.get(2)?, RevisionId::new)?,
                lineage: to_u64(row.get(3)?)?,
                status: if row.get(4)? {
                    PublicationStatus::Sealed
                } else {
                    PublicationStatus::Provisional
                },
                anchor: PublicationAnchor {
                    height: to_height(row.get(5)?)?,
                    hash: to_hash(row.get(6)?)?,
                },
            },
            page: PendingPage {
                page: opaque(row.get(7)?, PageId::new)?,
                from: to_height(row.get(8)?)?,
                to: to_height(row.get(9)?)?,
                scripts,
            },
            lifecycle: if row.get::<_, i64>(10)? == 1 {
                LedgerLifecycle::Active
            } else {
                LedgerLifecycle::Candidate
            },
            policy_generation: to_u64(row.get(11)?)?,
            target: ChainPoint {
                height: to_height(row.get(12)?)?,
                hash: to_hash(row.get(13)?)?,
            },
        });
    }
    Ok(pages)
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
