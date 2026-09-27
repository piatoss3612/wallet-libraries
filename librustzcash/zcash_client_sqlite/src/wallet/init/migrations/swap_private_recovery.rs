//! Keep authenticated discovery candidates separate from credited wallet notes.
use super::swap_payment_recovery;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
/// Identifier for private recovery policy and directory checks.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x75ea2907_2b95_4ba9_af1e_c7a80b4d2136);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_payment_recovery::MIGRATION_ID].into_iter().collect()
    }
    fn description(&self) -> &'static str {
        "Persists private recovery policy and directory checks."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        tx.execute_batch("CREATE TABLE ironwood_swap_private_recovery (
            account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE
        );
        CREATE TABLE ironwood_swap_directory_checks (
            receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            height INTEGER NOT NULL CHECK (height BETWEEN 0 AND 4294967295),
            block_hash BLOB NOT NULL CHECK (length(block_hash) = 32)
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
