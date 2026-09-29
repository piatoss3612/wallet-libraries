//! Blocks Phase 1 status/enhancement queue inserts under durable `PrivateRequired`.
//!
//! Older readers never check `min_reader_version` and insert query types `0`/`1`. After this
//! migration, SQLite rejects those inserts while `tpir_meta.applied_mode` is private-required,
//! and any such rows already present are relocated to the withheld codes (+10).
use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::transparent_policy_generation;
use crate::wallet::init::WalletMigrationError;

/// Keep in sync with `transparent_ledger::policy::LEGACY_RETRIEVAL_GATE_SQL`.
const LEGACY_RETRIEVAL_GATE_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS tpir_forbid_legacy_retrieval_insert
BEFORE INSERT ON tx_retrieval_queue
FOR EACH ROW
WHEN NEW.query_type IN (0, 1)
 AND EXISTS (SELECT 1 FROM tpir_meta WHERE id = 0 AND applied_mode = 2)
BEGIN
  SELECT RAISE(ABORT, 'transparent ledger requires a newer reader');
END;

CREATE TRIGGER IF NOT EXISTS tpir_forbid_legacy_retrieval_update
BEFORE UPDATE OF query_type ON tx_retrieval_queue
FOR EACH ROW
WHEN NEW.query_type IN (0, 1)
 AND EXISTS (SELECT 1 FROM tpir_meta WHERE id = 0 AND applied_mode = 2)
BEGIN
  SELECT RAISE(ABORT, 'transparent ledger requires a newer reader');
END;
"#;

/// Identifier for the migration that forbids Phase 1 retrieval-queue inserts under PrivateRequired.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0xc8d5f3b2_ae40_4f9c_b7d6_2e1f9e8d7c6b);

const DEPENDENCIES: &[Uuid] = &[transparent_policy_generation::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Forbids Phase 1 retrieval-queue inserts under durable PrivateRequired and relocates any that remain."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Relocate ordinary codes left behind by a Phase 1 writer after PrivateRequired.
        conn.execute(
            "UPDATE tx_retrieval_queue
             SET query_type = query_type + 10
             WHERE query_type IN (0, 1)
               AND EXISTS (
                   SELECT 1 FROM tpir_meta WHERE id = 0 AND applied_mode = 2
               )",
            [],
        )?;
        conn.execute_batch(LEGACY_RETRIEVAL_GATE_SQL)?;
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
