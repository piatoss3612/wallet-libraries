//! Reconciles what builds older than the transparent ledger wrote to a wallet this build upgraded.
//!
//! Published zakura-client-sqlite 0.1.0-rc5 and 0.1.0-rc7 can open a wallet this build upgraded:
//! the schema keeps everything they read and write (see `migrations::retain_zip318_kind`). They do
//! not maintain the transparent ledger's provenance, so after one of them has run, this build
//! reconciles its writes during initialization, before anything else uses the wallet:
//!
//! - Every transparent output and spend recorded without an origin was written by an older build.
//!   It gets the legacy public origin, plus local construction when its transaction carries local
//!   creation evidence, exactly as the `transparent_ledger_schema` migration classified records
//!   that predate the ledger. Neither origin is coverage, so this grants no private authority,
//!   and records that already have an origin are left as they are.
//! - `tpir_legacy_writes`, which records that an older build stored a transaction, is cleared.
//!   Until then the transparent ledger refuses the wallet.
//!
//! Nothing has to run before the older build opens the wallet.
//!
//! TODO(zakura-core/wallet-libraries#85): remove with the legacy `zip318_kind` column.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::WalletMigrationError;

/// Records that an older build stored a transaction, and the reconciliation it requires has not
/// run yet.
pub(in crate::wallet) fn pending(conn: &Connection) -> Result<bool, rusqlite::Error> {
    if !marker_installed(conn)? {
        return Ok(false);
    }
    Ok(conn
        .query_row("SELECT 1 FROM tpir_legacy_writes", [], |_| Ok(()))
        .optional()?
        .is_some())
}

fn marker_installed(conn: &Connection) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tpir_legacy_writes')",
        [],
        |row| row.get(0),
    )
}

/// Origins for the transparent outputs and spends that have none. Each `INSERT` reads the table it
/// writes, so SQLite evaluates the whole selection before inserting any row.
const RECONCILE_ORIGINS: &str = "
    INSERT INTO tpir_output_origins (output_id, origin)
    SELECT o.id, 0 FROM transparent_received_outputs o
    WHERE NOT EXISTS (SELECT 1 FROM tpir_output_origins x WHERE x.output_id = o.id)
    UNION ALL
    SELECT o.id, 1 FROM transparent_received_outputs o
    JOIN transactions t ON t.id_tx = o.transaction_id
    WHERE (t.created IS NOT NULL OR t.target_height IS NOT NULL)
    AND NOT EXISTS (SELECT 1 FROM tpir_output_origins x WHERE x.output_id = o.id);

    WITH spends (spending_transaction_id, prevout_txid, prevout_output_index) AS (
        SELECT s.transaction_id, t.txid, o.output_index
        FROM transparent_received_output_spends s
        JOIN transparent_received_outputs o ON o.id = s.transparent_received_output_id
        JOIN transactions t ON t.id_tx = o.transaction_id
        UNION
        SELECT spending_transaction_id, prevout_txid, prevout_output_index
        FROM transparent_spend_map
    ),
    unclassified AS (
        SELECT * FROM spends s
        WHERE NOT EXISTS (
            SELECT 1 FROM tpir_spend_origins x
            WHERE x.spending_transaction_id = s.spending_transaction_id
            AND x.prevout_txid = s.prevout_txid
            AND x.prevout_output_index = s.prevout_output_index
        )
    )
    INSERT INTO tpir_spend_origins (
        spending_transaction_id, prevout_txid, prevout_output_index, origin
    )
    SELECT spending_transaction_id, prevout_txid, prevout_output_index, 0 FROM unclassified
    UNION ALL
    SELECT u.spending_transaction_id, u.prevout_txid, u.prevout_output_index, 1
    FROM unclassified u JOIN transactions t ON t.id_tx = u.spending_transaction_id
    WHERE t.created IS NOT NULL OR t.target_height IS NOT NULL;";

/// Reconciles an older build's writes, atomically. Runs on every initialization once the schema is
/// current; it changes nothing when no older build has written since the last run.
pub(super) fn reconcile(conn: &mut Connection) -> Result<(), WalletMigrationError> {
    if !marker_installed(conn)? {
        // Initialization was asked to stop before `retain_zip318_kind`.
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(RECONCILE_ORIGINS)?;
    tx.execute("DELETE FROM tpir_legacy_writes", [])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;
