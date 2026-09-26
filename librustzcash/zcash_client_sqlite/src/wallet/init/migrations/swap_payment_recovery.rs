//! Keep authenticated discovery candidates separate from credited wallet notes.
use super::swap_receiving_coverage;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
/// Identifier for pending swap payments and retained nullifier coverage.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x7aec2ef0_6b82_40f0_a1a4_47c1330b6813);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_receiving_coverage::MIGRATION_ID]
            .into_iter()
            .collect()
    }
    fn description(&self) -> &'static str {
        "Persists pending swap payments and retained nullifier coverage."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // No coverage is inferred for old scans: their nullifiers may already have been pruned.
        tx.execute_batch("CREATE TABLE ironwood_nullifier_scan_blocks (
            height INTEGER PRIMARY KEY CHECK (height >= 0 AND height <= 4294967295)
        );
        CREATE TABLE ironwood_swap_payment_recovery (
            receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            txid BLOB NOT NULL CHECK (length(txid) = 32),
            action_index INTEGER NOT NULL CHECK (action_index BETWEEN 0 AND 4294967295),
            height INTEGER NOT NULL CHECK (height BETWEEN 0 AND 4294967295),
            block_hash BLOB NOT NULL CHECK (length(block_hash) = 32),
            tx_index INTEGER NOT NULL CHECK (tx_index BETWEEN 0 AND 65535),
            position INTEGER NOT NULL CHECK (position BETWEEN 0 AND 4294967295),
            encrypted_note BLOB NOT NULL CHECK (length(encrypted_note) = 676),
            PRIMARY KEY (txid, action_index)
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
