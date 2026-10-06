//! Records the transparent shape that Enhance PIR reports for a transaction, and requeues the
//! privately recoverable details of transactions whose transparent details are unsupported
//! (route 2).
//!
//! Each Enhance PIR record asserts, as trusted service metadata, whether its transaction has
//! transparent inputs and whether it has transparent outputs. Storage now keeps those two flags,
//! with the mined height they were validated at, in `ironwood_enhance_routing`; NULL means
//! unknown, never "no transparent data". A shielding classification needs the explicit absence
//! of transparent outputs, so wallets that recovered a route-2 transaction's memo and fee before
//! this migration have no shape for it yet.
//!
//! [`super::ironwood_unsupported_memo_retry`] requeued only unknown memos. Once a memo is known,
//! the metadata record can still be retrieved by querying the same authenticated note position,
//! so this migration also queues metadata work for every route-2 transaction whose fee or shape
//! is unknown, bound to its first received version-3 note. Notes, spend links, fees, routes and
//! public retrieval intents are untouched; the work is dispatched only privately, only while
//! public authority is absent. The memo requeue is repeated so that a wallet that skipped the
//! earlier migration's effects is repaired the same way; both statements are idempotent.
use std::collections::HashSet;

use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::ironwood_unsupported_memo_retry;
use crate::wallet::init::WalletMigrationError;

/// Adds the recorded transparent shape and requeues route-2 memo and metadata work.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x6f2e9a14_c3d8_4b57_8e01_a9d4c7b25f36);

const DEPENDENCIES: &[Uuid] = &[ironwood_unsupported_memo_retry::MIGRATION_ID];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Records Enhance PIR transparent shapes and requeues private details of route-2 transactions."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        // Flags use the record's wire bits: transparent inputs 1, transparent outputs 2. Route 2
        // is `PRIVATE_DETAILS_UNSUPPORTED`. A memo position already claimed by another note keeps
        // its claim.
        conn.execute_batch(
            "ALTER TABLE ironwood_enhance_routing
                 ADD COLUMN transparent_flags INTEGER CHECK (transparent_flags IN (0, 1, 2, 3));
             ALTER TABLE ironwood_enhance_routing
                 ADD COLUMN transparent_flags_height INTEGER
                     CHECK (transparent_flags_height >= 0
                            AND (transparent_flags IS NULL) = (transparent_flags_height IS NULL));
             INSERT INTO ironwood_memo_retrieval_queue (received_note_id, commitment_tree_position)
             SELECT rn.id, rn.commitment_tree_position
             FROM ironwood_received_notes rn
             JOIN transactions t ON t.id_tx = rn.transaction_id
             JOIN ironwood_enhance_routing r ON r.transaction_id = t.id_tx
             WHERE r.route = 2 AND t.raw IS NULL AND t.mined_height IS NOT NULL
               AND rn.memo IS NULL AND rn.note_version = 3
               AND rn.commitment_tree_position IS NOT NULL
             ON CONFLICT DO NOTHING;
             INSERT INTO ironwood_enhance_metadata_queue (
                 transaction_id, commitment_tree_position, output_index, compact_bound
             )
             SELECT t.id_tx, rn.commitment_tree_position, rn.action_index, 0
             FROM transactions t
             JOIN ironwood_enhance_routing r ON r.transaction_id = t.id_tx
             JOIN ironwood_received_notes rn ON rn.transaction_id = t.id_tx
             WHERE r.route = 2 AND t.raw IS NULL AND t.mined_height IS NOT NULL
               AND rn.note_version = 3 AND rn.commitment_tree_position IS NOT NULL
               AND rn.action_index = (
                   SELECT MIN(n.action_index) FROM ironwood_received_notes n
                   WHERE n.transaction_id = t.id_tx AND n.note_version = 3
                     AND n.commitment_tree_position IS NOT NULL)
             ON CONFLICT(transaction_id) DO UPDATE SET
                 commitment_tree_position = excluded.commitment_tree_position,
                 output_index = excluded.output_index,
                 compact_bound = 0;",
        )?;
        Ok(())
    }

    fn down(&self, _conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrate() {
        super::super::tests::test_migrate(&[super::MIGRATION_ID]);
    }

    /// The shape columns accept only the record's two flags, and a shape only with the height
    /// it was validated at.
    #[test]
    fn shape_columns_are_constrained() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ironwood_enhance_routing (transaction_id INTEGER PRIMARY KEY);
             INSERT INTO ironwood_enhance_routing VALUES (1);
             ALTER TABLE ironwood_enhance_routing
                 ADD COLUMN transparent_flags INTEGER CHECK (transparent_flags IN (0, 1, 2, 3));
             ALTER TABLE ironwood_enhance_routing
                 ADD COLUMN transparent_flags_height INTEGER
                     CHECK (transparent_flags_height >= 0
                            AND (transparent_flags IS NULL) = (transparent_flags_height IS NULL));",
        )
        .unwrap();
        for (flags, height) in [("1", "10"), ("3", "0"), ("NULL", "NULL")] {
            conn.execute_batch(&format!(
                "UPDATE ironwood_enhance_routing
                 SET transparent_flags = {flags}, transparent_flags_height = {height}"
            ))
            .unwrap();
        }
        for (flags, height) in [("4", "10"), ("1", "NULL"), ("NULL", "10"), ("1", "-1")] {
            assert!(
                conn.execute_batch(&format!(
                    "UPDATE ironwood_enhance_routing
                     SET transparent_flags = {flags}, transparent_flags_height = {height}"
                ))
                .is_err(),
                "accepted {flags}, {height}"
            );
        }
    }
}
