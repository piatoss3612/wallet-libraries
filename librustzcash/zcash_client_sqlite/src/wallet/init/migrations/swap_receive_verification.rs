//! Keep address-exposure evidence separate from mutable recovery checkpoints.
use super::swap_receive_reservations;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x8391ee73_d4a6_491d_9a94_4c2a36e91c05);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_receive_reservations::MIGRATION_ID]
            .into_iter()
            .collect()
    }
    fn description(&self) -> &'static str {
        "Persist canonical empty incoming-address verification."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Older recovery checkpoints can include payments. Do not migrate them as empty checks.
        tx.execute_batch(
            "CREATE TABLE ironwood_swap_receive_checks (
   receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
   height INTEGER NOT NULL CHECK(height BETWEEN 0 AND 4294967295),
   block_hash BLOB NOT NULL CHECK(length(block_hash)=32)
  );",
        )?;
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
