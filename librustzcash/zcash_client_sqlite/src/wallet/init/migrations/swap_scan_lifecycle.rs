//! Bound local swap scanning independently of durable PIR closeout.
use super::swap_private_recovery;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x5af803d8_d1b5_4637_8909_66a49b7f081a);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_private_recovery::MIGRATION_ID].into_iter().collect()
    }
    fn description(&self) -> &'static str {
        "Bounds swap scanning and saves PIR recovery targets."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        tx.execute_batch("CREATE TABLE ironwood_swap_scan_uses (
            receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            operation_id TEXT NOT NULL,
            scan_from INTEGER NOT NULL CHECK(scan_from BETWEEN 0 AND 4294967295),
            scan_through INTEGER CHECK(scan_through BETWEEN 0 AND 4294967295),
            PRIMARY KEY(receiving_key_id, operation_id)
        );
        CREATE TABLE ironwood_swap_recovery_targets (
            receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            height INTEGER NOT NULL CHECK(height BETWEEN 0 AND 4294967295),
            block_hash BLOB NOT NULL CHECK(length(block_hash) = 32)
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
