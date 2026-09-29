//! Separate network scheduling, lookup coverage, and live operation provenance.
use super::swap_refund_memo_progress;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x6ad7c165_4581_4ac7_92b2_5fb9fe30e821);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_refund_memo_progress::MIGRATION_ID]
            .into_iter()
            .collect()
    }
    fn description(&self) -> &'static str {
        "Persist swap discovery scheduling and operation provenance."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Legacy height deadlines did not preserve the provider outcome. Poll
        // again to establish receipt expectations instead of inferring no refund.
        tx.execute_batch("ALTER TABLE ironwood_swap_scan_uses ADD COLUMN local INTEGER NOT NULL DEFAULT 0 CHECK(local IN(0,1));
        ALTER TABLE ironwood_swap_scan_uses ADD COLUMN observed_at INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE ironwood_swap_scan_uses ADD COLUMN terminal_at INTEGER;
        ALTER TABLE ironwood_swap_scan_uses ADD COLUMN expectation INTEGER NOT NULL DEFAULT 0 CHECK(expectation IN(0,1,2));
        ALTER TABLE ironwood_swap_scan_uses ADD COLUMN anchor_height INTEGER;
        ALTER TABLE ironwood_swap_scan_uses ADD COLUMN expected_value INTEGER;
        UPDATE ironwood_swap_scan_uses SET observed_at=unixepoch();
        UPDATE ironwood_swap_scan_uses SET local=1 WHERE
          EXISTS(SELECT 1 FROM ironwood_swap_receive_quotes q JOIN ironwood_swap_receive_reservations r ON r.id=q.reservation_id WHERE r.receiving_key_id=ironwood_swap_scan_uses.receiving_key_id AND ironwood_swap_scan_uses.operation_id='receive-quote:'||q.request_id) OR EXISTS (
            SELECT 1 FROM ironwood_swap_refund_watches w WHERE
            w.receiving_key_id=ironwood_swap_scan_uses.receiving_key_id
            AND w.operation_id=ironwood_swap_scan_uses.operation_id AND w.initial_height IS NULL);
        CREATE INDEX ironwood_swap_scan_local ON ironwood_swap_scan_uses(local,scan_through,receiving_key_id);
        CREATE TABLE ironwood_swap_discovery (
          receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
          next_attempt_at INTEGER NOT NULL DEFAULT 0,
          attempts INTEGER NOT NULL DEFAULT 0,
          lookup_height INTEGER,
          lookup_hash BLOB,
          completed_at INTEGER,
          closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN(0,1)),
          CHECK((lookup_height IS NULL)=(lookup_hash IS NULL))
        );
        INSERT INTO ironwood_swap_discovery(receiving_key_id) SELECT id FROM ironwood_receiving_keys;
        CREATE INDEX ironwood_swap_discovery_due ON ironwood_swap_discovery(closed,next_attempt_at,receiving_key_id);
        CREATE TRIGGER ironwood_swap_discovery_register AFTER INSERT ON ironwood_receiving_keys BEGIN
          INSERT INTO ironwood_swap_discovery(receiving_key_id) VALUES(NEW.id);
        END;
        CREATE TABLE ironwood_swap_spend_replay (
          account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
          through_height INTEGER NOT NULL
        );")?;
        Ok(())
    }
    fn down(&self, _: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn migrate() {
        super::super::tests::test_migrate(&[super::MIGRATION_ID]);
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    #[test]
    fn only_proven_local_uses_survive_migration_as_scanning_watches() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE accounts(id INTEGER PRIMARY KEY);
            CREATE TABLE ironwood_receiving_keys(id INTEGER PRIMARY KEY);
            CREATE TABLE ironwood_swap_scan_uses(receiving_key_id INTEGER,operation_id TEXT,scan_through INTEGER);
            CREATE TABLE ironwood_swap_refund_watches(receiving_key_id INTEGER,operation_id TEXT,initial_height INTEGER);
            CREATE TABLE ironwood_swap_receive_reservations(id INTEGER PRIMARY KEY,receiving_key_id INTEGER);
            CREATE TABLE ironwood_swap_receive_quotes(request_id TEXT,reservation_id INTEGER);
            INSERT INTO ironwood_receiving_keys VALUES(1),(2),(3),(4),(5);
            INSERT INTO ironwood_swap_scan_uses VALUES(1,'local',110),(2,'restored',NULL),(3,'ambiguous',NULL),(4,'receive-quote:known',NULL),(5,'receive-quote:unproven',NULL);
            INSERT INTO ironwood_swap_refund_watches VALUES(1,'local',NULL),(2,'restored',100);
            INSERT INTO ironwood_swap_receive_reservations VALUES(1,4);
            INSERT INTO ironwood_swap_receive_quotes VALUES('known',1);").unwrap();
        let tx = conn.transaction().unwrap();
        Migration.up(&tx).unwrap();
        tx.commit().unwrap();
        let local:Vec<i64>=conn.prepare("SELECT receiving_key_id FROM ironwood_swap_scan_uses WHERE local=1 ORDER BY receiving_key_id").unwrap()
            .query_map([],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(local, vec![1, 4]);
        assert!(
            conn.query_row(
                "SELECT terminal_at IS NULL FROM ironwood_swap_scan_uses WHERE receiving_key_id=1",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM ironwood_swap_discovery", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            5
        );
    }
}
