//! Durable transparent-policy transitions and generation checks.
//!
//! The stored policy is what dispatch trusts. A mode change increments
//! `policy_generation` in the same SQLite transaction; same-mode reapplication does not.
//! Applying `PrivateRequired` raises `min_reader_version` so older readers fail closed.

use zcash_client_backend::data_api::transparent_ledger::{
    AppliedTransparentPolicy, PrivateTransparentDetail, TransparentLedgerMode,
};
use zcash_primitives::transaction::TxId;

use crate::error::SqliteClientError;

use super::{TPIR_READER_VERSION, durable_policy, mode_code, resolve_mode};

/// Reads the durable policy, including its generation. The handle must already be configured,
/// and a stored `PrivateRequired` policy is never weakened by a weaker handle.
pub(crate) fn applied_transparent_policy(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<AppliedTransparentPolicy, SqliteClientError> {
    // Reject unconfigured handles before reading, matching other ledger APIs.
    let _ = resolve_mode(conn, configured)?;
    read_applied_policy(conn)?
        .ok_or_else(|| SqliteClientError::CorruptedData("tpir_meta policy row is missing".into()))
}

/// Confirms that the durable generation still equals `expected`.
pub(crate) fn check_transparent_policy_generation(
    conn: &rusqlite::Connection,
    expected: u64,
) -> Result<(), SqliteClientError> {
    let applied = read_applied_policy(conn)?.ok_or_else(|| {
        SqliteClientError::CorruptedData("tpir_meta policy row is missing".into())
    })?;
    if applied.generation != expected {
        return Err(SqliteClientError::StaleTransparentPolicy {
            expected,
            applied: applied.generation,
        });
    }
    Ok(())
}

/// Durably applies `mode`. A mode change increments the generation; same-mode reapplication
/// does not. Applying `PrivateRequired` also raises the minimum reader version to
/// [`TPIR_READER_VERSION`].
pub(crate) fn apply_transparent_policy(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
    mode: TransparentLedgerMode,
) -> Result<AppliedTransparentPolicy, SqliteClientError> {
    // The handle must already be configured; the write does not invent a mode for empty wallets.
    let _ = configured.ok_or(SqliteClientError::TransparentLedgerModeNotConfigured)?;
    let write = |conn: &rusqlite::Connection| -> Result<AppliedTransparentPolicy, SqliteClientError> {
        // Re-read under the write lock so a concurrent transition cannot be overwritten.
        let current = read_applied_policy(conn)?.ok_or_else(|| {
            SqliteClientError::CorruptedData("tpir_meta policy row is missing".into())
        })?;
        if current.mode == mode {
            // Same-mode reapply: leave generation and outstanding work unchanged.
            return Ok(current);
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| SqliteClientError::CorruptedData("policy_generation overflow".into()))?;
        let updated = conn.execute(
            "UPDATE tpir_meta
             SET applied_mode = :mode,
                 policy_generation = :generation,
                 min_reader_version = CASE
                     WHEN :raise_reader THEN MAX(min_reader_version, :min_reader)
                     ELSE min_reader_version
                 END
             WHERE id = 0",
            rusqlite::named_params![
                ":mode": mode_code(mode),
                ":generation": i64::try_from(generation).map_err(|_| {
                    SqliteClientError::CorruptedData("policy_generation does not fit i64".into())
                })?,
                ":raise_reader": mode == TransparentLedgerMode::PrivateRequired,
                ":min_reader": TPIR_READER_VERSION,
            ],
        )?;
        if updated != 1 {
            return Err(SqliteClientError::CorruptedData(
                "tpir_meta policy row is missing".into(),
            ));
        }
        Ok(AppliedTransparentPolicy { mode, generation })
    };

    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        let applied = write(&tx)?;
        tx.commit()?;
        Ok(applied)
    } else {
        write(conn)
    }
}

/// Transparent follow-on details withheld from public dispatch under a policy that does not
/// retain public authority.
pub(crate) fn pending_private_transparent_details(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<Vec<PrivateTransparentDetail>, SqliteClientError> {
    let mode = resolve_mode(conn, configured)?;
    if mode.retains_public_authority() {
        // Public authority dispatches matching-generation work; nothing is withheld as private.
        return Ok(vec![]);
    }

    let mut details = Vec::new();
    let mut parents = conn.prepare_cached(
        "SELECT q.txid FROM tx_retrieval_queue q
         WHERE q.query_type = 1
           AND q.dependent_transaction_id IS NOT NULL
         ORDER BY q.txid",
    )?;
    for txid in parents.query_map([], |row| row.get::<_, [u8; 32]>(0))? {
        details.push(PrivateTransparentDetail::ParentTransaction {
            txid: TxId::from_bytes(txid?),
        });
    }

    let mut mixed = conn.prepare_cached(
        "SELECT t.txid FROM ironwood_enhance_routing r
         JOIN transactions t ON t.id_tx = r.transaction_id
         WHERE r.route = 2
         ORDER BY t.txid",
    )?;
    for txid in mixed.query_map([], |row| row.get::<_, [u8; 32]>(0))? {
        details.push(PrivateTransparentDetail::MixedTransaction {
            txid: TxId::from_bytes(txid?),
        });
    }
    Ok(details)
}

/// Reads the durable policy without requiring a configured handle. Used by commit checks that
/// already hold a captured generation from the start of their SQLite transaction.
pub(crate) fn read_applied_policy(
    conn: &rusqlite::Connection,
) -> Result<Option<AppliedTransparentPolicy>, SqliteClientError> {
    Ok(durable_policy(conn)?.map(Into::into))
}

/// Captures the current generation for a commit path, failing closed when the ledger schema
/// is present but the policy row is missing.
pub(crate) fn capture_policy_generation(
    conn: &rusqlite::Connection,
) -> Result<u64, SqliteClientError> {
    Ok(read_applied_policy(conn)?
        .ok_or_else(|| SqliteClientError::CorruptedData("tpir_meta policy row is missing".into()))?
        .generation)
}

/// Like [`check_transparent_policy_generation`], for internal commit paths that do not need a
/// configured handle beyond the generation already captured.
pub(crate) fn ensure_policy_generation(
    conn: &rusqlite::Connection,
    expected: u64,
) -> Result<(), SqliteClientError> {
    check_transparent_policy_generation(conn, expected)
}

/// Returns whether the resolved mode retains public transparent authority.
///
/// When `configured` is present, the handle mode is resolved against the durable policy.
/// When absent (lower-level scan hooks), the durable policy alone decides; a wallet that
/// predates the ledger schema is treated as retaining public authority.
pub(crate) fn retains_public_authority(
    conn: &rusqlite::Connection,
    configured: Option<TransparentLedgerMode>,
) -> Result<bool, SqliteClientError> {
    match configured {
        Some(_) => Ok(resolve_mode(conn, configured)?.retains_public_authority()),
        None => Ok(read_applied_policy(conn)?
            .map(|p| p.mode.retains_public_authority())
            .unwrap_or(true)),
    }
}
