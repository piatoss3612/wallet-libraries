//! Adds early restore of one-time IVK payments (see `one_time_ivk::restore::early`) and
//! the spend journal (see `one_time_ivk::journal`).
//!
//! Additive: existing queued payments have no witness and are no hints, existing sweeps
//! are not provisional, no account has an early restore, and no block is covered by the
//! journal.
use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::one_time_ivk;
use crate::wallet::init::WalletMigrationError;

/// Identifier for the early restore migration.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0xc5b63b3b_8190_4d0f_b217_3921d7aac225);

const DEPENDENCIES: &[Uuid] = &[one_time_ivk::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Adds early restore of one-time IVK payments and the spend journal."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        conn.execute_batch(
            "ALTER TABLE ironwood_one_time_payment_recovery
                ADD COLUMN witness BLOB CHECK (length(witness) = 1024);
            ALTER TABLE ironwood_one_time_payment_recovery
                ADD COLUMN scan_hint INTEGER NOT NULL DEFAULT 0 CHECK (scan_hint IN (0, 1));
            ALTER TABLE ironwood_one_time_sweeps
                ADD COLUMN provisional INTEGER NOT NULL DEFAULT 0 CHECK (provisional IN (0, 1));
            CREATE TABLE ironwood_one_time_restores (
                account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                restore_from INTEGER NOT NULL CHECK (restore_from BETWEEN 0 AND 4294967295),
                anchor_height INTEGER NOT NULL CHECK (anchor_height BETWEEN 0 AND 4294967295),
                anchor_hash BLOB NOT NULL CHECK (length(anchor_hash) = 32),
                anchor_tree_size INTEGER NOT NULL CHECK (anchor_tree_size >= 0),
                state INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2))
            );
            CREATE TABLE ironwood_spend_journal (
                block_height INTEGER NOT NULL CHECK (block_height BETWEEN 0 AND 4294967295),
                tx_index INTEGER NOT NULL CHECK (tx_index BETWEEN 0 AND 65535),
                block_hash BLOB NOT NULL CHECK (length(block_hash) = 32),
                txid BLOB NOT NULL CHECK (length(txid) = 32),
                ironwood_start_position INTEGER NOT NULL CHECK (ironwood_start_position >= 0),
                compact_tx BLOB NOT NULL,
                PRIMARY KEY (block_height, tx_index)
            );
            CREATE TABLE ironwood_spend_journal_nullifiers (
                block_height INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                action_index INTEGER NOT NULL CHECK (action_index >= 0),
                nf BLOB NOT NULL CHECK (length(nf) = 32),
                PRIMARY KEY (block_height, tx_index, action_index),
                FOREIGN KEY (block_height, tx_index)
                    REFERENCES ironwood_spend_journal(block_height, tx_index) ON DELETE CASCADE
            );
            CREATE INDEX idx_ironwood_spend_journal_nullifiers_nf
                ON ironwood_spend_journal_nullifiers(nf);
            CREATE TABLE ironwood_journal_coverage (
                account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                start_height INTEGER NOT NULL CHECK (start_height BETWEEN 0 AND 4294967295),
                end_height INTEGER NOT NULL
                    CHECK (end_height > start_height AND end_height <= 4294967296),
                PRIMARY KEY (account_id, start_height)
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
