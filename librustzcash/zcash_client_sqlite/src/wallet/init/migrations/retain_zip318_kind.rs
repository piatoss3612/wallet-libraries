//! Keeps `transactions.zip318_kind` for older builds and records when one of them writes.
//!
//! Published zakura-client-sqlite 0.1.0-rc5 and 0.1.0-rc7 run
//! `UPDATE transactions SET zip318_kind = ...` every time they store a decrypted transaction, and
//! read `v_transactions.zip318_kind`. A wallet this build upgraded must keep working when the user
//! reinstalls a build that uses them, without any step this build would have to take in advance,
//! so the column and its view field stay as unused legacy schema. This build never reads them; new
//! rows hold the default, `0` (not classified).
//!
//! This migration:
//!
//! - adds the column back, with the published definition, to a wallet that applied the
//!   unpublished revision of `drop_zip318_pool_migration` that dropped it;
//! - adds the `zip318_kind` field back to `v_transactions`, after `trust_status` as in rc5/rc7;
//! - creates `tpir_legacy_writes` and the `tpir_legacy_zip318_write` trigger. Only an older build
//!   writes `zip318_kind`, so the trigger marks exactly the wallets an older build stored a
//!   transaction in. Initialization then reconciles that build's writes before this build uses the
//!   wallet (see `wallet::init::legacy_writers`).
//!
//! TODO(zakura-core/wallet-libraries#85): drop the column, its view field, the marker table and
//! its trigger once no supported build writes the column.

use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::transparent_shared_derivations;
use crate::wallet::init::WalletMigrationError;

/// Restores `transactions.zip318_kind` where needed and installs the legacy-writer marker.
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
        "Keeps transactions.zip318_kind for older builds and marks wallets they write to."
    }
}

/// The `v_transactions` select-list entry after which rc5/rc7 place `zip318_kind`.
const TRUST_STATUS_VIEW_COLUMN: &str = "transactions.trust_status";

fn column_exists(
    conn: &rusqlite::Connection,
    relation: &str,
    column: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [relation, column],
        |row| row.get(0),
    )
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        if !column_exists(transaction, "transactions", "zip318_kind")? {
            transaction.execute_batch(
                "ALTER TABLE transactions ADD COLUMN zip318_kind INTEGER NOT NULL DEFAULT 0",
            )?;
        }

        if !column_exists(transaction, "v_transactions", "zip318_kind")? {
            let view: String = transaction.query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'v_transactions'",
                [],
                |row| row.get(0),
            )?;
            if view.matches(TRUST_STATUS_VIEW_COLUMN).count() != 1 {
                return Err(WalletMigrationError::CorruptedData(
                    "unexpected v_transactions trust_status column".into(),
                ));
            }
            let updated = view.replacen(
                TRUST_STATUS_VIEW_COLUMN,
                "transactions.trust_status,\n       transactions.zip318_kind",
                1,
            );
            transaction.execute_batch("DROP VIEW v_transactions")?;
            transaction.execute_batch(&updated)?;
        }

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

    use super::{DEPENDENCIES, MIGRATION_ID, column_exists};

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
        migrate_to(&mut db, &[MIGRATION_ID]);

        let conn = &db.conn;
        assert!(column_exists(conn, "v_transactions", "zip318_kind").unwrap());
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

    /// A wallet that applied the dropping revision gets the column and view field back with the
    /// published definition, and is not marked.
    #[test]
    fn restores_the_column_after_the_dropping_revision() {
        let (_file, mut db) = db();
        migrate_to(&mut db, DEPENDENCIES);
        // The dropping revision removed the view field, then the column.
        let view: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'v_transactions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let without_field = regex::Regex::new(r",\s*transactions\.zip318_kind")
            .unwrap()
            .replace(&view, "")
            .into_owned();
        assert_ne!(without_field, view);
        db.conn
            .execute_batch(&format!(
                "INSERT INTO transactions (id_tx, txid, min_observed_height) VALUES (1, X'01', 0);
                 DROP VIEW v_transactions;
                 {without_field};
                 ALTER TABLE transactions DROP COLUMN zip318_kind;"
            ))
            .unwrap();
        migrate_to(&mut db, &[MIGRATION_ID]);

        let conn = &db.conn;
        let (kind, not_null, default): (String, bool, String) = conn
            .query_row(
                "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('transactions')
                 WHERE name = 'zip318_kind'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), not_null, default.as_str()),
            ("INTEGER", true, "0")
        );
        assert!(column_exists(conn, "v_transactions", "zip318_kind").unwrap());
        assert_eq!(
            conn.query_row("SELECT zip318_kind FROM transactions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(!marked(conn));
    }
}
