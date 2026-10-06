//! Swap receiving keys, their operations, incoming reservations, and restore sweeps.

use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::ironwood_enhance;
use crate::wallet::init::WalletMigrationError;

/// Identifier for the swap receiving migration.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x75bb24bd_c390_4791_9694_368a878a7b76);

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        [ironwood_enhance::MIGRATION_ID].into_iter().collect()
    }

    fn description(&self) -> &'static str {
        "Adds swap receiving keys, operations, reservations and restore sweeps."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, tx: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Schema is independent of build features. Reopening without swap support
        // must preserve reservations rather than permit address reuse.
        tx.execute_batch(
            "CREATE TABLE ironwood_receiving_keys (
                id INTEGER PRIMARY KEY,
                account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                purpose INTEGER NOT NULL CHECK (purpose IN (0, 1)),
                derivation_version INTEGER NOT NULL CHECK (derivation_version = 1),
                key_index BLOB NOT NULL CHECK (typeof(key_index) = 'blob' AND length(key_index) = 8),
                receiver BLOB NOT NULL CHECK (typeof(receiver) = 'blob' AND length(receiver) = 43),
                scan_from INTEGER NOT NULL CHECK (scan_from BETWEEN 0 AND 4294967295),
                advances_allocation INTEGER NOT NULL CHECK (advances_allocation IN (0, 1)),
                registered_at INTEGER NOT NULL DEFAULT 0,
                active_from INTEGER CHECK (active_from BETWEEN 0 AND 4294967295),
                closed_at INTEGER,
                UNIQUE (account_id, purpose, derivation_version, key_index)
            );
            CREATE INDEX ironwood_receiving_keys_account_receiver
                ON ironwood_receiving_keys(account_id, receiver);
            CREATE INDEX ironwood_receiving_keys_scanning
                ON ironwood_receiving_keys(closed_at, active_from);
            ALTER TABLE ironwood_received_notes ADD COLUMN receiving_key_id INTEGER
                REFERENCES ironwood_receiving_keys(id);
            CREATE TABLE ironwood_swap_operations (
                receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
                operation_id TEXT NOT NULL,
                observed_at INTEGER NOT NULL DEFAULT 0,
                terminal_at INTEGER,
                expectation INTEGER NOT NULL DEFAULT 0 CHECK (expectation IN (0, 1, 2)),
                expected_value INTEGER CHECK (expected_value > 0),
                deadline INTEGER,
                PRIMARY KEY (receiving_key_id, operation_id)
            );
            CREATE TABLE ironwood_swap_sweeps (
                receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
                target_height INTEGER CHECK (target_height BETWEEN 0 AND 4294967295),
                target_hash BLOB CHECK (length(target_hash) = 32),
                lookup_height INTEGER CHECK (lookup_height BETWEEN 0 AND 4294967295),
                lookup_hash BLOB CHECK (length(lookup_hash) = 32),
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_at INTEGER NOT NULL DEFAULT 0,
                done_height INTEGER CHECK (done_height BETWEEN 0 AND 4294967295)
            );
            CREATE INDEX ironwood_swap_sweeps_due ON ironwood_swap_sweeps(done_height, next_attempt_at);
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
            );
            CREATE TABLE ironwood_nullifier_scan_blocks (
                height INTEGER PRIMARY KEY CHECK (height BETWEEN 0 AND 4294967295)
            );
            CREATE TABLE ironwood_swap_spend_retention (
                account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                nullifier_retention_height INTEGER NOT NULL DEFAULT 0
                    CHECK (nullifier_retention_height BETWEEN 0 AND 4294967295)
            );
            CREATE TABLE ironwood_swap_spend_replay (
                account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                through_height INTEGER NOT NULL
            );
            CREATE TABLE ironwood_swap_refund_watches (
                receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
                operation_id TEXT NOT NULL,
                next_check_at INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER,
                retain_from INTEGER CHECK (retain_from BETWEEN 0 AND 4294967295),
                PRIMARY KEY (receiving_key_id, operation_id)
            );
            CREATE TABLE ironwood_swap_refund_memo_progress (
                note_id INTEGER PRIMARY KEY REFERENCES ironwood_received_notes(id) ON DELETE CASCADE,
                receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
                funding_height INTEGER NOT NULL CHECK (funding_height BETWEEN 0 AND 4294967295)
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
                DELETE FROM ironwood_swap_refund_memo_progress WHERE note_id = NEW.id;
            END;
            CREATE TABLE ironwood_swap_receive_used (
                receiving_key_id INTEGER PRIMARY KEY REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE
            );
            CREATE TRIGGER remember_swap_receive_insert AFTER INSERT ON ironwood_received_notes
                WHEN NEW.receiving_key_id IS NOT NULL BEGIN
                INSERT OR IGNORE INTO ironwood_swap_receive_used VALUES (NEW.receiving_key_id);
            END;
            CREATE TRIGGER remember_swap_receive_update AFTER UPDATE OF receiving_key_id ON ironwood_received_notes
                WHEN NEW.receiving_key_id IS NOT NULL BEGIN
                INSERT OR IGNORE INTO ironwood_swap_receive_used VALUES (NEW.receiving_key_id);
            END;
            CREATE TABLE ironwood_swap_receive_reservations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                receiving_key_id INTEGER NOT NULL REFERENCES ironwood_receiving_keys(id) ON DELETE CASCADE,
                created_at INTEGER NOT NULL,
                started INTEGER NOT NULL DEFAULT 0 CHECK (started IN (0, 1)),
                closed_at INTEGER
            );
            CREATE UNIQUE INDEX one_open_swap_receive_reservation
                ON ironwood_swap_receive_reservations(receiving_key_id) WHERE closed_at IS NULL;
            CREATE TABLE ironwood_swap_receive_quotes (
                request_id TEXT PRIMARY KEY,
                reservation_id INTEGER NOT NULL REFERENCES ironwood_swap_receive_reservations(id) ON DELETE CASCADE,
                requested_at INTEGER NOT NULL,
                operation_id TEXT,
                deposit_memo TEXT,
                deadline INTEGER,
                status TEXT,
                funded INTEGER NOT NULL DEFAULT 0 CHECK (funded IN (0, 1)),
                checked_at INTEGER,
                rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected IN (0, 1))
            );
            CREATE INDEX swap_receive_quote_operation ON ironwood_swap_receive_quotes(operation_id);",
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
