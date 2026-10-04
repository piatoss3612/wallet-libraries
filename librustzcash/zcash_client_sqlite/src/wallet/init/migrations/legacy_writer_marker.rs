//! Installs the marker for transaction stores by published rc5/rc7 builds.
//!
//! The unreleased `drop_zip318_pool_migration` retains `transactions.zip318_kind` and its
//! `v_transactions` field throughout upgrades from published schemas. This migration only
//! installs `tpir_legacy_writes` and the trigger on classification updates, which current
//! builds do not perform. Initialization reconciles older writes before using the ledger.
//! UTXO-only writes do not update the classification, so reconciliation also validates
//! private projections when the marker is empty.
//!
//! Databases that applied the earlier development revision that dropped the column are
//! outside the supported upgrade path; this migration does not repair their schema.
//!
//! TODO(zakura-core/wallet-libraries#85): remove with the legacy classification column.

use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::transparent_shared_derivations;
use crate::wallet::init::WalletMigrationError;

/// Installs the legacy-writer marker after the transparent ledger schema.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0xca0b6106_3efa_4a89_ad92_e57327f78df2);

const DEPENDENCIES: &[Uuid] = &[transparent_shared_derivations::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Marks transaction stores by older wallet builds."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        transaction.execute_batch(
            "CREATE TABLE tpir_legacy_writes (
                 id INTEGER PRIMARY KEY CHECK (id = 0)
             );
             CREATE TRIGGER tpir_legacy_zip318_write
             AFTER UPDATE OF zip318_kind ON transactions
             BEGIN
                 INSERT OR IGNORE INTO tpir_legacy_writes (id) VALUES (0);
             END;",
        )?;
        Ok(())
    }

    fn down(&self, _transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use secrecy::Secret;
    use tempfile::NamedTempFile;
    use zcash_protocol::consensus::Network;

    use zcash_client_backend::data_api::testing::TestRng;

    use crate::{
        WalletDb,
        testing::db::{test_clock, test_rng},
        util::testing::FixedClock,
        wallet::init::{WalletMigrator, migrations::tests::test_migrate},
    };

    use super::{DEPENDENCIES, MIGRATION_ID};

    type TestDb = WalletDb<Connection, Network, FixedClock, TestRng>;

    #[test]
    fn migrate() {
        test_migrate(&[MIGRATION_ID]);
    }

    fn marked(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM tpir_legacy_writes)",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn migrate_to(db: &mut TestDb, target: &[uuid::Uuid]) {
        WalletMigrator::new()
            .with_seed(Secret::new(vec![0xab; 32]))
            .ignore_seed_relevance()
            .init_or_migrate_to(db, target)
            .unwrap();
    }

    fn db() -> (NamedTempFile, TestDb) {
        let file = NamedTempFile::new().unwrap();
        let db = WalletDb::for_path(file.path(), Network::TestNetwork, test_clock(), test_rng())
            .unwrap();
        (file, db)
    }

    /// The column and its view field survive from the published schema, keep their values, and
    /// only a write to the column marks the wallet.
    #[test]
    fn keeps_the_published_column_and_marks_only_its_writes() {
        let (_file, mut db) = db();
        migrate_to(&mut db, DEPENDENCIES);
        db.conn
            .execute(
                "INSERT INTO transactions (id_tx, txid, min_observed_height, zip318_kind)
                 VALUES (1, X'01', 0, 3)",
                [],
            )
            .unwrap();
        let view_before: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'v_transactions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        migrate_to(&mut db, &[MIGRATION_ID]);
        // Reopening a supported upgraded database does not reinstall the marker or change data.
        migrate_to(&mut db, &[MIGRATION_ID]);

        let conn = &db.conn;
        conn.prepare("SELECT zip318_kind FROM v_transactions")
            .unwrap();
        let view_after: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'v_transactions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_before, view_after);
        let definition: (String, bool, String) = conn.query_row(
            "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('transactions') WHERE name = 'zip318_kind'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).unwrap();
        assert_eq!(definition, ("INTEGER".into(), true, "0".into()));
        let kind: i64 = conn
            .query_row("SELECT zip318_kind FROM transactions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, 3);
        assert!(!marked(conn));

        // Writes this build makes leave the marker alone.
        conn.execute(
            "UPDATE transactions SET mined_height = NULL, trust_status = 1 WHERE id_tx = 1",
            [],
        )
        .unwrap();
        assert!(!marked(conn));

        // The statement every published rc5/rc7 transaction store runs.
        conn.execute(
            "UPDATE transactions SET zip318_kind = 2 WHERE id_tx = 1",
            [],
        )
        .unwrap();
        assert!(marked(conn));
    }

    /// Partial initialization before the marker skips reconciliation safely, even on reopen.
    #[test]
    fn initialization_before_marker_installation_is_safe() {
        let (_file, mut db) = db();
        migrate_to(&mut db, DEPENDENCIES);
        migrate_to(&mut db, DEPENDENCIES);
        let installed: bool = db
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'tpir_legacy_writes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!installed);
        assert!(!crate::wallet::init::legacy_writers::pending(&db.conn).unwrap());
    }
}
