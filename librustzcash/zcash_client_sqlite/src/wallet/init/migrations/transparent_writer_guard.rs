//! Seedless compatibility barrier for chain and address-ownership mutations.
use super::transparent_activation_schema;
use crate::wallet::init::WalletMigrationError;
use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;

pub const MIGRATION_ID: Uuid = Uuid::from_u128(0xa5335e29_3abc_43e9_8bc3_287956d8ec01);
const DEPENDENCIES: &[Uuid] = &[transparent_activation_schema::MIGRATION_ID];
pub(super) struct Migration;
impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }
    fn description(&self) -> &'static str {
        "Requires compatible writers for transparent recovery lifecycle."
    }
}
impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Rewind mutates transaction placements, scan ranges and blocks, including rescans
        // with a retained higher checkpoint. Address/account changes also invalidate recovery.
        // Guard inserts as well: INSERT OR REPLACE may avoid DELETE triggers in SQLite.
        for table in [
            "blocks",
            "scan_queue",
            "transactions",
            "addresses",
            "accounts",
        ] {
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                conn.execute_batch(&format!(
                    "CREATE TRIGGER tpir_guard_{table}_{operation} BEFORE {operation} ON {table}
                     BEGIN
                       SELECT CASE WHEN tpir_writer_version() < (SELECT min_reader_version FROM tpir_meta WHERE id = 0)
                         THEN RAISE(ABORT, 'incompatible transparent ledger writer') END;
                     END;"
                ))?;
            }
        }
        conn.execute(
            "UPDATE tpir_meta SET min_reader_version = MAX(min_reader_version, 7) WHERE id = 0",
            [],
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
