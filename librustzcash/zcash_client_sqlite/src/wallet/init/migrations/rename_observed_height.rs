//! Renames `transactions.min_observed_height` so Phase 1 status lookups fail closed.
//!
//! Phase 1 `transaction_status_work_for` selects `min_observed_height` and returns a public
//! status request for any caller-supplied txid. Queue write triggers cannot stop that path.
//! After this rename, that prepare fails on a PrivateRequired (or any Phase-2-migrated) wallet,
//! while this build uses `observed_height`.
use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::withhold_legacy_retrieval;
use crate::wallet::init::WalletMigrationError;

/// Identifier for the `observed_height` rename that fail-closes Phase 1 status lookups.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0xd9e6a4c3_bf51_40ad_c8e7_3f2a0f9e8d7c);

const DEPENDENCIES: &[Uuid] = &[withhold_legacy_retrieval::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Renames transactions.min_observed_height to observed_height so Phase 1 direct status lookups fail closed."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        conn.execute_batch(
            "ALTER TABLE transactions RENAME COLUMN min_observed_height TO observed_height;",
        )?;
        Ok(())
    }

    fn down(&self, _transaction: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    use super::MIGRATION_ID;
    use crate::wallet::init::migrations::tests::test_migrate;

    #[test]
    fn migrate() {
        test_migrate(&[MIGRATION_ID]);
    }
}
