//! Retires suspended Ironwood outgoing candidates that value accounting proves are dummies.
//!
//! Before this migration, an outgoing candidate whose OVK decryption failed stayed in
//! `ironwood_enhance_outgoing_queue` with `not_recoverable = 1` indefinitely. For a
//! wallet-funded transaction padded with dummy outputs, that kept the transaction reported as
//! suspended work and kept its `tx_retrieval_queue` enhancement request, even though every
//! real output had been recovered. The store now retires such rows when a PIR response closes
//! the transaction's value balance; this migration applies the same rule once to rows written
//! before that, since no further response will arrive for them.
//!
//! The rule and its exclusions are documented on `enhance_pir::VALUE_BALANCED_DUMMIES`. The
//! SQL below is a frozen copy with its constants inlined: route `0` is private protection,
//! pool `4` is Ironwood, and query type `1` is enhancement.

use std::collections::HashSet;

use rusqlite::named_params;
use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::{ironwood_enhance, status_inclusion_evidence};
use crate::wallet::init::WalletMigrationError;

/// Retires suspended Ironwood outgoing candidates that no held account funded.
pub const MIGRATION_ID: Uuid = Uuid::from_u128(0x8b32fc31_ac5d_4008_9954_791c7c2c5d71);

const DEPENDENCIES: &[Uuid] = &[
    ironwood_enhance::MIGRATION_ID,
    status_inclusion_evidence::MIGRATION_ID,
];

pub(super) struct Migration;

impl schemerz::Migration<Uuid> for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Retires suspended Ironwood outgoing candidates that value accounting proves are dummies."
    }
}

const VALUE_BALANCED_DUMMIES: &str = "
    SELECT commitment_tree_position FROM ironwood_enhance_outgoing_queue
    WHERE transaction_id = :tx AND not_recoverable = 1
      AND EXISTS (
          SELECT 1 FROM transactions t
          JOIN ironwood_enhance_routing r ON r.transaction_id = t.id_tx
          WHERE r.route = 0 AND t.raw IS NULL AND t.mined_height IS NOT NULL
            AND t.id_tx = :tx AND t.fee IS NOT NULL)
      AND NOT EXISTS (
          SELECT 1 FROM ironwood_enhance_outgoing_queue o
          WHERE o.transaction_id = :tx
            AND (o.not_recoverable = 0 OR NOT EXISTS (
                SELECT 1 FROM ironwood_enhance_outgoing_accounts a
                WHERE a.commitment_tree_position = o.commitment_tree_position)))
      AND NOT EXISTS (
          SELECT 1 FROM ironwood_memo_retrieval_queue q
          JOIN ironwood_received_notes rn ON rn.id = q.received_note_id
          WHERE rn.transaction_id = :tx)
      AND NOT EXISTS (SELECT 1 FROM ironwood_enhance_metadata_queue WHERE transaction_id = :tx)
      AND NOT EXISTS (SELECT 1 FROM ironwood_enhance_discovery_queue WHERE transaction_id = :tx)
      AND EXISTS (SELECT 1 FROM ironwood_received_note_spends WHERE transaction_id = :tx)
      AND (SELECT COALESCE(SUM(rn.value), 0)
           FROM ironwood_received_note_spends s
           JOIN ironwood_received_notes rn ON rn.id = s.ironwood_received_note_id
           WHERE s.transaction_id = :tx)
        = (SELECT fee FROM transactions WHERE id_tx = :tx)
        + (SELECT COALESCE(SUM(value), 0) FROM (
               SELECT value FROM sent_notes WHERE transaction_id = :tx AND output_pool = 4
               UNION ALL
               SELECT rn.value FROM ironwood_received_notes rn
               WHERE rn.transaction_id = :tx
                 AND NOT EXISTS (
                     SELECT 1 FROM sent_notes sn
                     WHERE sn.transaction_id = :tx AND sn.output_pool = 4
                       AND sn.output_index = rn.action_index)))";

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, conn: &rusqlite::Transaction) -> Result<(), Self::Error> {
        let transactions = conn
            .prepare(
                "SELECT DISTINCT transaction_id FROM ironwood_enhance_outgoing_queue
                 WHERE not_recoverable = 1",
            )?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for tx in transactions {
            let positions = conn
                .prepare(VALUE_BALANCED_DUMMIES)?
                .query_map(named_params![":tx": tx], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if positions.is_empty() {
                continue;
            }
            // Foreign keys are off during migrations, so no cascade applies.
            for position in positions {
                conn.execute(
                    "DELETE FROM ironwood_enhance_outgoing_accounts
                     WHERE commitment_tree_position = ?",
                    [position],
                )?;
                conn.execute(
                    "DELETE FROM ironwood_enhance_outgoing_queue WHERE commitment_tree_position = ?",
                    [position],
                )?;
            }
            // Release the txid request once no private work remains, as completion does.
            conn.execute(
                "DELETE FROM tx_retrieval_queue
                 WHERE txid = (SELECT txid FROM transactions WHERE id_tx = :tx)
                   AND query_type = 1
                   AND EXISTS (SELECT 1 FROM ironwood_enhance_routing
                               WHERE transaction_id = :tx AND route = 0)
                   AND NOT EXISTS (
                       SELECT 1 FROM ironwood_memo_retrieval_queue q
                       JOIN ironwood_received_notes rn ON rn.id = q.received_note_id
                       WHERE rn.transaction_id = :tx)
                   AND NOT EXISTS (
                       SELECT 1 FROM ironwood_enhance_outgoing_queue WHERE transaction_id = :tx)
                   AND NOT EXISTS (
                       SELECT 1 FROM ironwood_enhance_metadata_queue WHERE transaction_id = :tx)
                   AND NOT EXISTS (
                       SELECT 1 FROM ironwood_enhance_discovery_queue WHERE transaction_id = :tx)",
                named_params![":tx": tx],
            )?;
        }
        Ok(())
    }

    fn down(&self, _: &rusqlite::Transaction) -> Result<(), Self::Error> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    use schemerz_rusqlite::RusqliteMigration;

    #[test]
    fn migrate() {
        crate::wallet::init::migrations::tests::test_migrate(&[super::MIGRATION_ID]);
    }

    const TABLES: &str = "
        CREATE TABLE transactions (
            id_tx INTEGER PRIMARY KEY, txid BLOB, raw BLOB, mined_height INTEGER, fee INTEGER);
        CREATE TABLE ironwood_enhance_routing (transaction_id INTEGER PRIMARY KEY, route INTEGER);
        CREATE TABLE ironwood_enhance_outgoing_queue (
            commitment_tree_position INTEGER PRIMARY KEY, transaction_id INTEGER,
            not_recoverable INTEGER);
        CREATE TABLE ironwood_enhance_outgoing_accounts (
            commitment_tree_position INTEGER, account_id INTEGER);
        CREATE TABLE ironwood_received_notes (
            id INTEGER PRIMARY KEY, transaction_id INTEGER, action_index INTEGER, value INTEGER);
        CREATE TABLE ironwood_received_note_spends (
            ironwood_received_note_id INTEGER, transaction_id INTEGER);
        CREATE TABLE ironwood_memo_retrieval_queue (
            received_note_id INTEGER, commitment_tree_position INTEGER);
        CREATE TABLE ironwood_enhance_metadata_queue (transaction_id INTEGER);
        CREATE TABLE ironwood_enhance_discovery_queue (transaction_id INTEGER);
        CREATE TABLE sent_notes (
            transaction_id INTEGER, output_pool INTEGER, output_index INTEGER, value INTEGER);
        CREATE TABLE tx_retrieval_queue (txid BLOB, query_type INTEGER);";

    /// Transaction `tx` spends a 100 note, pays 60 to a recipient (sent note at index 1) and
    /// 30 in change (received note at index 0), and has two undecryptable candidates at
    /// positions `base` and `base + 1`. `fee` decides whether the balance closes.
    fn send(conn: &rusqlite::Connection, tx: i64, fee: i64, base: i64, accounts: bool) {
        let funding = tx * 10;
        conn.execute_batch(&format!(
            "INSERT INTO transactions VALUES ({funding}, X'00', NULL, 1, 0);
             INSERT INTO ironwood_received_notes VALUES ({funding}, {funding}, 0, 100);
             INSERT INTO transactions VALUES ({tx}, X'{tx:02x}', NULL, 2, {fee});
             INSERT INTO ironwood_enhance_routing VALUES ({tx}, 0);
             INSERT INTO ironwood_received_note_spends VALUES ({funding}, {tx});
             INSERT INTO ironwood_received_notes VALUES ({tx}, {tx}, 0, 30);
             INSERT INTO sent_notes VALUES ({tx}, 4, 1, 60);
             INSERT INTO ironwood_enhance_outgoing_queue VALUES ({base}, {tx}, 1);
             INSERT INTO ironwood_enhance_outgoing_queue VALUES ({}, {tx}, 1);
             INSERT INTO tx_retrieval_queue VALUES (X'{tx:02x}', 1);",
            base + 1
        ))
        .unwrap();
        if accounts {
            conn.execute_batch(&format!(
                "INSERT INTO ironwood_enhance_outgoing_accounts VALUES ({base}, 1);
                 INSERT INTO ironwood_enhance_outgoing_accounts VALUES ({}, 1);",
                base + 1
            ))
            .unwrap();
        }
    }

    fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn retires_only_value_balanced_decryption_failures() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        send(&conn, 1, 10, 100, true); // 100 = 60 + 30 + 10: dummies
        send(&conn, 2, 5, 200, true); // 5 unaccounted: a real output may be hidden
        send(&conn, 3, 10, 300, false); // orphaned by account deletion
        // A mined zero-fee transaction spending a zero-valued note: linked, but summing to 0.
        conn.execute_batch(
            "INSERT INTO transactions VALUES (40, X'00', NULL, 1, 0);
             INSERT INTO ironwood_received_notes VALUES (40, 40, 0, 0);
             INSERT INTO transactions VALUES (4, X'04', NULL, 2, 0);
             INSERT INTO ironwood_enhance_routing VALUES (4, 0);
             INSERT INTO ironwood_received_note_spends VALUES (40, 4);
             INSERT INTO ironwood_enhance_outgoing_queue VALUES (400, 4, 1);
             INSERT INTO ironwood_enhance_outgoing_accounts VALUES (400, 1);
             INSERT INTO tx_retrieval_queue VALUES (X'04', 1);",
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        super::Migration.up(&tx).unwrap();
        tx.commit().unwrap();

        let outgoing = |tx: i64| {
            count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM ironwood_enhance_outgoing_queue WHERE transaction_id = {tx}"
                ),
            )
        };
        let queued = |tx: i64| {
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM tx_retrieval_queue WHERE txid = X'{tx:02x}'"),
            )
        };
        assert_eq!((outgoing(1), queued(1)), (0, 0));
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM ironwood_enhance_outgoing_accounts
                 WHERE commitment_tree_position IN (100, 101)"
            ),
            0
        );
        assert_eq!((outgoing(2), queued(2)), (2, 1));
        assert_eq!((outgoing(3), queued(3)), (2, 1));
        assert_eq!((outgoing(4), queued(4)), (0, 0));
    }
}
