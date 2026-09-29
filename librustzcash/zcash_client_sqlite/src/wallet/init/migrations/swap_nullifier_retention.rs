//! Replace indefinite shared history retention with a per-account Ironwood floor.
use super::swap_receive_verification;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x36463314_9e3c_4544_8d2b_65bcc2601da2);
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_receive_verification::MIGRATION_ID]
            .into_iter()
            .collect()
    }
    fn description(&self) -> &'static str {
        "Bound temporary swap nullifier retention."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Existing wallets keep their evidence until recovery explicitly releases it.
        tx.execute_batch(
            "ALTER TABLE ironwood_swap_private_recovery ADD COLUMN
            nullifier_retention_height INTEGER NOT NULL DEFAULT 0
            CHECK(nullifier_retention_height BETWEEN 0 AND 4294967295);",
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
