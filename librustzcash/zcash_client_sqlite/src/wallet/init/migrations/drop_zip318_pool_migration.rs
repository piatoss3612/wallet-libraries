//! Drops the schema of the ZIP 318 pool-migration engine and ZIP 318 transaction classification.
//!
//! This fork does not carry the Orchard -> Ironwood pool-migration engine, nor the ZIP 318
//! classification that labelled transactions as migration transfers. The migrations that created
//! their schema stay registered, because they are published and later migrations depend on them;
//! this migration removes what they left behind so that a fresh wallet and an upgraded one converge
//! on the same schema:
//!
//! - the `orchard_ironwood_migration*` tables and their indexes, which nothing reads or writes;
//! - the `zip318_kind` column of `transactions` and `v_transactions`.
//!
//! `v_transactions` is rebuilt from its stored definition with only the `zip318_kind` column
//! removed, so every other column, including `pool_crossing_value` and the Ironwood Enhance
//! expiry, is kept exactly as it was.

use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::status_inclusion_evidence;
use crate::wallet::init::WalletMigrationError;

/// Drops the pool-migration tables and the `zip318_kind` column.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x772a0632_3d0e_4dff_b1f8_c64863eefaaa);

// `status_inclusion_evidence` transitively depends on every migration that created the dropped
// schema, and on `ironwood_enhance`, the last migration to rebuild `v_transactions`.
const DEPENDENCIES: &[Uuid] = &[status_inclusion_evidence::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Drops the ZIP 318 pool-migration tables and transaction classification column."
    }
}

/// The `v_transactions` select-list entry that this migration removes.
const ZIP318_VIEW_COLUMN: &str = "transactions.zip318_kind";

/// `view` with its single [`ZIP318_VIEW_COLUMN`] entry, and the comma that precedes it, removed.
fn remove_zip318_column(view: &str) -> Option<String> {
    let [(start, _)] = view.match_indices(ZIP318_VIEW_COLUMN).collect::<Vec<_>>()[..] else {
        return None;
    };
    let comma = view[..start].trim_end().strip_suffix(',')?.len();
    Some(format!(
        "{}{}",
        &view[..comma],
        &view[start + ZIP318_VIEW_COLUMN.len()..]
    ))
}

/// Fails, naming them, if views or triggers outside this library's schema still depend on the
/// objects this migration removes.
///
/// SQLite rejects `DROP COLUMN` while any view or trigger in the schema fails to resolve, with an
/// error that does not say which one. An application view over `v_transactions`, or one left by a
/// build this fork never shipped, would otherwise make the upgrade fail without explanation.
fn reject_dependents(transaction: &rusqlite::Transaction) -> Result<(), WalletMigrationError> {
    let mut dependents = vec![];
    let views: Vec<String> = transaction
        .prepare("SELECT name FROM sqlite_master WHERE type = 'view'")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for view in views {
        let query = format!("SELECT * FROM \"{}\" LIMIT 0", view.replace('"', "\"\""));
        if transaction.prepare(&query).is_err() {
            dependents.push(view);
        }
    }
    dependents.extend(
        transaction
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger'
                 AND (instr(sql, 'orchard_ironwood_migration') > 0
                      OR instr(sql, 'zip318_kind') > 0
                      OR instr(sql, 'v_transactions') > 0)",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?,
    );
    if dependents.is_empty() {
        Ok(())
    } else {
        Err(WalletMigrationError::CorruptedData(format!(
            "views or triggers depend on the ZIP 318 schema this upgrade removes: {}",
            dependents.join(", ")
        )))
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Children before parents, so no foreign key is left dangling while the batch runs.
        transaction.execute_batch(
            "DROP INDEX IF EXISTS idx_orchard_ironwood_migration_tx_due;
             DROP INDEX IF EXISTS idx_orchard_ironwood_migrations_account;
             DROP TABLE IF EXISTS orchard_ironwood_migration_spend_nullifiers;
             DROP TABLE IF EXISTS orchard_ironwood_migration_transaction_deps;
             DROP TABLE IF EXISTS orchard_ironwood_migration_transactions;
             DROP TABLE IF EXISTS orchard_ironwood_migration_prep_direct_funding;
             DROP TABLE IF EXISTS orchard_ironwood_migration_prep_outputs;
             DROP TABLE IF EXISTS orchard_ironwood_migration_prep_inputs;
             DROP TABLE IF EXISTS orchard_ironwood_migration_crossing_values;
             DROP TABLE IF EXISTS orchard_ironwood_migrations;",
        )?;

        // SQLite refuses to drop a column that a view references, so the view goes first.
        let view: String = transaction.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'v_transactions'",
            [],
            |row| row.get(0),
        )?;
        let updated = remove_zip318_column(&view).ok_or_else(|| {
            WalletMigrationError::CorruptedData(
                "unexpected v_transactions zip318_kind column".into(),
            )
        })?;
        transaction.execute_batch("DROP VIEW v_transactions")?;
        reject_dependents(transaction)?;
        transaction.execute_batch("ALTER TABLE transactions DROP COLUMN zip318_kind")?;
        transaction.execute_batch(&updated)?;
        Ok(())
    }

    fn down(&self, _transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, named_params};
    use secrecy::Secret;
    use tempfile::NamedTempFile;
    use zcash_protocol::consensus::Network;

    use crate::{
        WalletDb,
        testing::db::{test_clock, test_rng},
        wallet::init::{WalletMigrator, migrations::tests::test_migrate},
    };

    use super::{DEPENDENCIES, MIGRATION_ID, remove_zip318_column};

    #[test]
    fn removes_only_the_zip318_column() {
        let view = "CREATE VIEW v AS SELECT a,\n    transactions.trust_status,\n    transactions.zip318_kind\nFROM t";
        assert_eq!(
            remove_zip318_column(view).unwrap(),
            "CREATE VIEW v AS SELECT a,\n    transactions.trust_status\nFROM t"
        );
        assert!(remove_zip318_column("CREATE VIEW v AS SELECT a FROM t").is_none());
    }

    #[test]
    fn migrate() {
        test_migrate(&[MIGRATION_ID]);
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = :name)",
            named_params![":name": name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn has_column(conn: &Connection, relation: &str, column: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM pragma_table_info(:relation) WHERE name = :column
             )",
            named_params![":relation": relation, ":column": column],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A wallet that has pool-migration rows and classified transactions loses both, and keeps its
    /// transactions.
    #[test]
    fn drops_populated_schema_and_keeps_transactions() {
        let data_file = NamedTempFile::new().unwrap();
        let mut db_data = WalletDb::for_path(
            data_file.path(),
            Network::TestNetwork,
            test_clock(),
            test_rng(),
        )
        .unwrap();
        let seed = [0xab; 32];
        WalletMigrator::new()
            .with_seed(Secret::new(seed.to_vec()))
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, DEPENDENCIES)
            .unwrap();

        let conn = &db_data.conn;
        // The pool-migration row names no real account; this test is about the schema.
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO transactions (id_tx, txid, min_observed_height, zip318_kind)
             VALUES (1, X'01', 0, 3);
             INSERT INTO orchard_ironwood_migrations (
                 id, account_id, status, note_split_fee_buffer, note_split_change,
                 note_split_prep_fees, note_split_total_input, note_split_total_migratable
             )
             VALUES (1, 1, 'planned', 100, NULL, 200, 300, 400);
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        assert!(has_column(conn, "v_transactions", "zip318_kind"));

        WalletMigrator::new()
            .with_seed(Secret::new(seed.to_vec()))
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[MIGRATION_ID])
            .unwrap();

        let conn = &db_data.conn;
        for table in [
            "orchard_ironwood_migrations",
            "orchard_ironwood_migration_crossing_values",
            "orchard_ironwood_migration_prep_inputs",
            "orchard_ironwood_migration_prep_outputs",
            "orchard_ironwood_migration_prep_direct_funding",
            "orchard_ironwood_migration_transactions",
            "orchard_ironwood_migration_transaction_deps",
            "orchard_ironwood_migration_spend_nullifiers",
            "idx_orchard_ironwood_migration_tx_due",
            "idx_orchard_ironwood_migrations_account",
        ] {
            assert!(!table_exists(conn, table), "{table} was not dropped");
        }
        assert!(!has_column(conn, "transactions", "zip318_kind"));
        assert!(!has_column(conn, "v_transactions", "zip318_kind"));
        assert!(has_column(conn, "v_transactions", "pool_crossing_value"));

        let txid: Vec<u8> = conn
            .query_row("SELECT txid FROM transactions WHERE id_tx = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(txid, vec![1]);
    }

    /// A view that depends on the removed schema is named, and the wallet is left exactly as it
    /// was, instead of SQLite failing the upgrade with an anonymous error.
    #[test]
    fn dependent_views_are_named_and_the_upgrade_rolls_back() {
        use crate::wallet::init::WalletMigrationError;
        use schemerz::MigratorError;

        let data_file = NamedTempFile::new().unwrap();
        let mut db_data = WalletDb::for_path(
            data_file.path(),
            Network::TestNetwork,
            test_clock(),
            test_rng(),
        )
        .unwrap();
        let seed = [0xab; 32];
        WalletMigrator::new()
            .with_seed(Secret::new(seed.to_vec()))
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, DEPENDENCIES)
            .unwrap();
        db_data
            .conn
            .execute_batch(
                "CREATE VIEW ext_app_history AS SELECT txid FROM v_transactions;
                 CREATE VIEW ext_app_migrations AS SELECT * FROM orchard_ironwood_migrations;",
            )
            .unwrap();

        let result = WalletMigrator::new()
            .with_seed(Secret::new(seed.to_vec()))
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[MIGRATION_ID]);
        assert!(
            matches!(
                &result,
                Err(MigratorError::Migration {
                    error: WalletMigrationError::CorruptedData(reason),
                    ..
                }) if reason.contains("ext_app_history") && reason.contains("ext_app_migrations")
            ),
            "{result:?}"
        );
        let conn = &db_data.conn;
        assert!(has_column(conn, "transactions", "zip318_kind"));
        assert!(has_column(conn, "v_transactions", "zip318_kind"));
        assert!(table_exists(conn, "orchard_ironwood_migrations"));

        db_data
            .conn
            .execute_batch("DROP VIEW ext_app_history; DROP VIEW ext_app_migrations;")
            .unwrap();
        WalletMigrator::new()
            .with_seed(Secret::new(seed.to_vec()))
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[MIGRATION_ID])
            .unwrap();
        assert!(!has_column(&db_data.conn, "transactions", "zip318_kind"));
    }
}
