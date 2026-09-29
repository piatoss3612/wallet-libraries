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

/// SQLite gates that abort Phase 1 inserts/updates of ordinary retrieval codes (`0`/`1`) while
/// durable `PrivateRequired` is active. Withheld codes (`+10`) are unaffected. When the durable
/// mode is not private-required the `WHEN` clause is false, so public and PrivateShadow writers
/// keep their ordinary queue codes.
pub(crate) const LEGACY_RETRIEVAL_GATE_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS tpir_forbid_legacy_retrieval_insert
BEFORE INSERT ON tx_retrieval_queue
FOR EACH ROW
WHEN NEW.query_type IN (0, 1)
 AND EXISTS (SELECT 1 FROM tpir_meta WHERE id = 0 AND applied_mode = 2)
BEGIN
  SELECT RAISE(ABORT, 'transparent ledger requires a newer reader');
END;

CREATE TRIGGER IF NOT EXISTS tpir_forbid_legacy_retrieval_update
BEFORE UPDATE OF query_type ON tx_retrieval_queue
FOR EACH ROW
WHEN NEW.query_type IN (0, 1)
 AND EXISTS (SELECT 1 FROM tpir_meta WHERE id = 0 AND applied_mode = 2)
BEGIN
  SELECT RAISE(ABORT, 'transparent ledger requires a newer reader');
END;
"#;

/// Installs the Phase 1 retrieval-queue gate. Idempotent; safe to call on every
/// `PrivateRequired` apply (including same-mode reapplication).
pub(crate) fn ensure_legacy_retrieval_gate(
    conn: &rusqlite::Connection,
) -> Result<(), SqliteClientError> {
    conn.execute_batch(LEGACY_RETRIEVAL_GATE_SQL)?;
    Ok(())
}

/// Relocates ordinary status/enhancement codes so Phase 1 enumerators cannot see them.
fn relocate_legacy_retrieval_codes(conn: &rusqlite::Connection) -> Result<(), SqliteClientError> {
    conn.execute(
        "UPDATE tx_retrieval_queue
         SET query_type = query_type + :offset
         WHERE query_type IN (0, 1)",
        rusqlite::named_params![
            ":offset": crate::wallet::TxQueryType::WITHHELD_OFFSET,
        ],
    )?;
    Ok(())
}

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
            // Same-mode reapply: leave generation unchanged. Under PrivateRequired, still
            // install the write gate and relocate any ordinary codes a Phase 1 writer may
            // have inserted after an earlier transition (the transition itself runs once).
            if mode == TransparentLedgerMode::PrivateRequired {
                ensure_legacy_retrieval_gate(conn)?;
                relocate_legacy_retrieval_codes(conn)?;
            }
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
        // Keep still-required retrieval obligations on the new generation. Leaving the old
        // stamp would hide them from public dispatch after a transition that still retains
        // public authority (Public → PrivateShadow). Under PrivateRequired, matching
        // generation does not restore public follow-on: authority is absent.
        let generation_i64 = i64::try_from(generation).map_err(|_| {
            SqliteClientError::CorruptedData("policy_generation does not fit i64".into())
        })?;
        conn.execute(
            "UPDATE tx_retrieval_queue SET policy_generation = :generation",
            rusqlite::named_params![":generation": generation_i64],
        )?;
        if mode == TransparentLedgerMode::PrivateRequired {
            // Unresolved public LWD markers become sticky private-details rows so pending
            // private recovery can observe them after the transition.
            conn.execute(
                "UPDATE ironwood_enhance_routing
                 SET route = 2
                 WHERE route = 1
                   AND EXISTS (
                       SELECT 1 FROM transactions t
                       WHERE t.id_tx = ironwood_enhance_routing.transaction_id
                         AND t.raw IS NULL
                   )",
                [],
            )?;
            // Relocate ordinary status/enhancement obligations out of the Phase 1 enumerator
            // codes. Older readers never check min_reader_version and would otherwise dispatch
            // these txids publicly despite the raised gate. The write gate then rejects any
            // later Phase 1 inserts of codes 0/1 while this mode remains durable.
            relocate_legacy_retrieval_codes(conn)?;
            ensure_legacy_retrieval_gate(conn)?;
        } else if mode.retains_public_authority() {
            // Restore Phase 1-visible codes before converting sticky private-details markers.
            conn.execute(
                "UPDATE tx_retrieval_queue
                 SET query_type = query_type - :offset
                 WHERE query_type IN (:withheld_status, :withheld_enhancement)",
                rusqlite::named_params![
                    ":offset": crate::wallet::TxQueryType::WITHHELD_OFFSET,
                    ":withheld_status": crate::wallet::TxQueryType::Status.withheld_code(),
                    ":withheld_enhancement": crate::wallet::TxQueryType::Enhancement.withheld_code(),
                ],
            )?;
            // Sticky route 2 was assigned while public enhancement was forbidden. With
            // public authority restored, unresolved mixed rows become ordinary LWD work
            // (route 1). Route codes match enhance_pir::{LWD_REQUIRED, PRIVATE_DETAILS_UNSUPPORTED}.
            conn.execute(
                "UPDATE ironwood_enhance_routing
                 SET route = 1
                 WHERE route = 2
                   AND EXISTS (
                       SELECT 1 FROM transactions t
                       WHERE t.id_tx = ironwood_enhance_routing.transaction_id
                         AND t.raw IS NULL
                   )",
                [],
            )?;
            conn.execute(
                "INSERT INTO tx_retrieval_queue (txid, query_type, policy_generation)
                 SELECT t.txid, 1, :generation
                 FROM ironwood_enhance_routing r
                 JOIN transactions t ON t.id_tx = r.transaction_id
                 WHERE r.route = 1 AND t.raw IS NULL
                 ON CONFLICT (txid, query_type) DO NOTHING",
                rusqlite::named_params![":generation": generation_i64],
            )?;
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
         WHERE q.query_type IN (1, 1 + :offset)
           AND q.dependent_transaction_id IS NOT NULL
         ORDER BY q.txid",
    )?;
    for txid in parents.query_map(
        rusqlite::named_params![":offset": crate::wallet::TxQueryType::WITHHELD_OFFSET],
        |row| row.get::<_, [u8; 32]>(0),
    )? {
        details.push(PrivateTransparentDetail::ParentTransaction {
            txid: TxId::from_bytes(txid?),
        });
    }

    // Unresolved mixed/LWD follow-on: sticky route 2, and any remaining public LWD route
    // that a concurrent transition has not yet relocated.
    let mut mixed = conn.prepare_cached(
        "SELECT t.txid FROM ironwood_enhance_routing r
         JOIN transactions t ON t.id_tx = r.transaction_id
         WHERE r.route IN (1, 2) AND t.raw IS NULL
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
