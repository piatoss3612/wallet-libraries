//! Durable, per-note progress for authenticated funding-memo recovery.

use schemerz_rusqlite::RusqliteMigration;
use std::collections::HashSet;
use uuid::Uuid;

use super::swap_receiver_index;
use crate::wallet::init::WalletMigrationError;

pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x87081bfd_f7a9_4830_a6df_6d19681b2135);
pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }
    fn dependencies(&self) -> HashSet<Uuid> {
        [swap_receiver_index::MIGRATION_ID].into_iter().collect()
    }
    fn description(&self) -> &'static str {
        "Persist completed refund memo recovery per note."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;
    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Start empty so upgrading a wallet retries existing authenticated records.
        // Height changes make a record eligible again after a reorg. A repeated
        // enhancement with identical memo bytes leaves its progress intact.
        tx.execute_batch("CREATE TABLE ironwood_swap_refund_memo_progress (
            note_id INTEGER PRIMARY KEY REFERENCES ironwood_received_notes(id) ON DELETE CASCADE,
            receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
            funding_height INTEGER NOT NULL CHECK(funding_height BETWEEN 0 AND 4294967295)
        );
        CREATE INDEX ironwood_swap_refund_memo_progress_key
            ON ironwood_swap_refund_memo_progress(receiving_key_id);
        CREATE TRIGGER ironwood_swap_refund_memo_changed
        AFTER UPDATE OF memo, account_id, recipient_key_scope, receiving_key_id, transaction_id
        ON ironwood_received_notes
        WHEN OLD.memo IS NOT NEW.memo OR OLD.account_id IS NOT NEW.account_id
          OR OLD.recipient_key_scope IS NOT NEW.recipient_key_scope
          OR OLD.receiving_key_id IS NOT NEW.receiving_key_id
          OR OLD.transaction_id IS NOT NEW.transaction_id
        BEGIN
            DELETE FROM ironwood_swap_refund_memo_progress WHERE note_id=NEW.id;
        END;")?;
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
