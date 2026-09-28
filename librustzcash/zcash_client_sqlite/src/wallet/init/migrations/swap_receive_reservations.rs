//! Keep incoming swap reservations separate from payments and UI activity.
use super::swap_scan_lifecycle;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;

pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x5166ed6d_6ceb_41ee_b225_dd3fd94afc7b);
pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_scan_lifecycle::MIGRATION_ID].into_iter().collect()
    }
    fn description(&self) -> &'static str {
        "Persist incoming swap reservations and quote reconciliation."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        tx.execute_batch("CREATE TABLE ironwood_swap_receive_used (
            receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE
        );
        INSERT INTO ironwood_swap_receive_used
            SELECT DISTINCT receiving_key_id FROM ironwood_received_notes WHERE receiving_key_id IS NOT NULL;
        CREATE TRIGGER remember_swap_receive_insert AFTER INSERT ON ironwood_received_notes
            WHEN NEW.receiving_key_id IS NOT NULL BEGIN
            INSERT OR IGNORE INTO ironwood_swap_receive_used VALUES (NEW.receiving_key_id);
        END;
        CREATE TRIGGER remember_swap_receive_update AFTER UPDATE OF receiving_key_id ON ironwood_received_notes
            WHEN NEW.receiving_key_id IS NOT NULL BEGIN
            INSERT OR IGNORE INTO ironwood_swap_receive_used VALUES (NEW.receiving_key_id);
        END;
        CREATE TABLE ironwood_swap_receive_reservations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            created_at INTEGER NOT NULL,
            started INTEGER NOT NULL DEFAULT 0 CHECK(started IN (0,1)),
            legacy_unknown INTEGER NOT NULL DEFAULT 0 CHECK(legacy_unknown IN (0,1)),
            closed_at INTEGER
        );
        CREATE UNIQUE INDEX one_open_swap_receive_reservation ON ironwood_swap_receive_reservations(receiving_key_id)
            WHERE closed_at IS NULL;
        CREATE TABLE ironwood_swap_receive_quotes (
            request_id TEXT PRIMARY KEY,
            reservation_id INTEGER NOT NULL REFERENCES ironwood_swap_receive_reservations(id) ON DELETE CASCADE,
            requested_at INTEGER NOT NULL,
            operation_id TEXT,
            deposit_memo TEXT,
            deadline INTEGER,
            status TEXT,
            funded INTEGER NOT NULL DEFAULT 0 CHECK(funded IN (0,1)),
            checked_at INTEGER,
            rejected INTEGER NOT NULL DEFAULT 0 CHECK(rejected IN (0,1))
        );
        CREATE INDEX swap_receive_quote_operation ON ironwood_swap_receive_quotes(operation_id);
        INSERT INTO ironwood_swap_receive_reservations(receiving_key_id,created_at,started,legacy_unknown)
            SELECT k.id, unixepoch(), 1, 1 FROM ironwood_receiving_keys k
            WHERE k.purpose=1 AND k.advances_allocation=1
            AND NOT EXISTS(SELECT 1 FROM ironwood_swap_receive_used u WHERE u.receiving_key_id=k.id);")?;
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
