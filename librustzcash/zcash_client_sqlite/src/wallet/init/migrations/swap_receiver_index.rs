//! Index receiver lookup without changing existing reservations or key identities.

use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;

use super::swap_refund_watches;
use crate::wallet::init::WalletMigrationError;

pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x38d6e9d9_18e1_4950_9bc2_8c9dbd746da4);
pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_refund_watches::MIGRATION_ID].into_iter().collect()
    }
    fn description(&self) -> &'static str {
        "Index account-scoped swap receiver lookup."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        tx.execute_batch(
            "CREATE INDEX ironwood_receiving_keys_account_receiver
            ON ironwood_receiving_keys(account_id, receiver);",
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
