use super::*;
use crate::{
    WalletDb,
    testing::db::{test_clock, test_rng},
    wallet::init::WalletMigrator,
};
use zcash_protocol::consensus::Network;

fn db() -> WalletDb<
    Connection,
    Network,
    crate::util::testing::FixedClock,
    zcash_client_backend::data_api::testing::TestRng,
> {
    let mut db = WalletDb::from_connection(
        Connection::open_in_memory().unwrap(),
        Network::TestNetwork,
        test_clock(),
        test_rng(),
    );
    rusqlite::vtab::array::load_module(&db.conn).unwrap();
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    db
}

fn journal(conn: &Connection) -> Vec<Vec<u8>> {
    conn.prepare("SELECT id FROM schemer_migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(conn: &Connection, query: &str) -> i64 {
    conn.query_row(query, [], |r| r.get(0)).unwrap()
}

fn output_origins(conn: &Connection) -> Vec<(i64, i64)> {
    conn.prepare("SELECT output_id, origin FROM tpir_output_origins ORDER BY output_id, origin")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The statement every published rc5/rc7 transaction store runs marks the wallet, the ledger
/// refuses it until initialization reconciles, and reconciliation keeps the data, the journal and
/// the legacy column.
#[test]
fn an_old_store_is_reconciled_on_the_next_initialization() {
    let mut db = db();
    db.conn
        .execute_batch(
            "INSERT INTO transactions(txid, min_observed_height, raw, created, target_height)
             VALUES (X'0102', 42, X'0304', 123, 43);
             CREATE VIEW application_history AS SELECT txid, zip318_kind FROM v_transactions;",
        )
        .unwrap();
    let before = journal(&db.conn);
    assert!(!pending(&db.conn).unwrap());
    crate::wallet::transparent_ledger::durable_policy(&db.conn).unwrap();

    db.conn
        .execute(
            "UPDATE transactions SET zip318_kind = ?1 WHERE txid = X'0102'",
            [1],
        )
        .unwrap();
    assert!(pending(&db.conn).unwrap());
    assert!(matches!(
        crate::wallet::transparent_ledger::durable_policy(&db.conn),
        Err(crate::error::SqliteClientError::LegacyWritesUnreconciled)
    ));

    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert!(!pending(&db.conn).unwrap());
    crate::wallet::transparent_ledger::durable_policy(&db.conn).unwrap();
    assert_eq!(journal(&db.conn), before);
    assert_eq!(
        db.conn
            .query_row(
                "SELECT min_observed_height, raw, created, target_height, zip318_kind
                 FROM transactions",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            )
            .unwrap(),
        (42, vec![3, 4], "123".to_string(), 43, 1)
    );
    db.conn
        .prepare("SELECT * FROM application_history")
        .unwrap();
    assert_eq!(
        count(&db.conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

/// A rollback to the explicit handover's state needs no handover: a wallet whose policy changed,
/// or that holds private recovery state, is reconciled as well, without gaining authority.
#[test]
fn reconciliation_does_not_depend_on_the_policy() {
    for sql in [
        "UPDATE tpir_meta SET applied_mode = 1",
        "UPDATE tpir_meta SET policy_generation = 1",
        "INSERT INTO tpir_revisions(source,revision,lineage,sealed,publication_height,publication_hash)
         VALUES (X'01', X'02', 1, 0, 42, zeroblob(32))",
    ] {
        let mut db = db();
        db.conn.execute(sql, []).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO transactions(txid, min_observed_height) VALUES (X'01', 42);
                 UPDATE transactions SET zip318_kind = 2;",
            )
            .unwrap();
        WalletMigrator::new().init_or_migrate(&mut db).unwrap();
        assert!(!pending(&db.conn).unwrap(), "{sql}");
        assert_eq!(count(&db.conn, "SELECT count(*) FROM tpir_coverage"), 0);
    }
}

/// A failed reconciliation leaves the older build's writes and the marker in place.
#[test]
fn a_failed_reconciliation_is_atomic() {
    let mut db = db();
    db.conn
        .execute_batch(
            "INSERT INTO transactions(txid, min_observed_height) VALUES (X'02', 7);
             UPDATE transactions SET zip318_kind = 2;
             DROP TABLE tpir_spend_origins;",
        )
        .unwrap();
    assert!(WalletMigrator::new().init_or_migrate(&mut db).is_err());
    assert!(pending(&db.conn).unwrap());
    assert_eq!(count(&db.conn, "SELECT zip318_kind FROM transactions"), 2);
    assert_eq!(
        db.conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[cfg(feature = "transparent-inputs")]
fn with_account() -> WalletDb<
    Connection,
    Network,
    crate::util::testing::FixedClock,
    zcash_client_backend::data_api::testing::TestRng,
> {
    use secrecy::SecretVec;
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite, chain::ChainState};
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::BlockHeight;
    let mut db = db();
    db.create_account(
        "legacy writer fixture",
        &SecretVec::new(vec![7; 32]),
        &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(1_200_000), BlockHash([0; 32])),
            None,
        ),
        None,
    )
    .unwrap();
    db
}

/// Rows an older build wrote, as rc5/rc7 record them: a received output, a locally created
/// transaction with its own output spending the first, and a spend of an unknown output.
#[cfg(feature = "transparent-inputs")]
const OLD_WRITES: &str = "
    INSERT INTO transactions(id_tx,txid,min_observed_height) VALUES (1,zeroblob(32),42);
    INSERT INTO transactions(id_tx,txid,min_observed_height,created,target_height) VALUES (2, X'02',43,123,44);
    INSERT INTO transparent_received_outputs(id,transaction_id,output_index,account_id,address,address_id,script,value_zat)
        SELECT 1,1,0,account_id,cached_transparent_receiver_address,id,X'00',1000 FROM addresses WHERE cached_transparent_receiver_address IS NOT NULL LIMIT 1;
    INSERT INTO transparent_received_outputs(id,transaction_id,output_index,account_id,address,address_id,script,value_zat)
        SELECT 2,2,0,account_id,cached_transparent_receiver_address,id,X'00',2000 FROM addresses WHERE cached_transparent_receiver_address IS NOT NULL LIMIT 1;
    INSERT INTO transparent_received_output_spends(transparent_received_output_id,transaction_id) VALUES (1,2);
    INSERT INTO transparent_spend_map(spending_transaction_id,prevout_txid,prevout_output_index) VALUES (2, X'03', 4);";

/// An older build's outputs and spends get legacy public provenance, plus local construction
/// where the wallet created the transaction, and nothing else.
#[cfg(feature = "transparent-inputs")]
#[test]
fn old_outputs_and_spends_get_legacy_provenance_without_authority() {
    let mut db = with_account();
    db.conn.execute_batch(OLD_WRITES).unwrap();
    db.conn
        .execute(
            "UPDATE transactions SET zip318_kind = 0 WHERE id_tx = 2",
            [],
        )
        .unwrap();
    assert_eq!(
        count(&db.conn, "SELECT count(*) FROM tpir_output_origins"),
        0
    );
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert_eq!(output_origins(&db.conn), vec![(1, 0), (2, 0), (2, 1)]);
    assert_eq!(
        count(
            &db.conn,
            "SELECT count(*) FROM tpir_spend_origins WHERE origin = 0"
        ),
        2
    );
    assert_eq!(
        count(
            &db.conn,
            "SELECT count(*) FROM tpir_spend_origins WHERE origin = 1"
        ),
        2
    );
    for table in ["tpir_coverage", "tpir_active_accounts"] {
        assert_eq!(count(&db.conn, &format!("SELECT count(*) FROM {table}")), 0);
    }
    assert_eq!(
        count(&db.conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );

    // Reconciliation is idempotent.
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert_eq!(
        count(&db.conn, "SELECT count(*) FROM tpir_output_origins"),
        3
    );
    assert_eq!(
        count(&db.conn, "SELECT count(*) FROM tpir_spend_origins"),
        4
    );
}

/// Transparent outputs an older build discovered without storing a transaction leave no marker,
/// and still get provenance; records that already have an origin keep exactly the origins they had.
#[cfg(feature = "transparent-inputs")]
#[test]
fn unmarked_old_outputs_are_classified_and_existing_origins_kept() {
    let mut db = with_account();
    db.conn.execute_batch(OLD_WRITES).unwrap();
    db.conn
        .execute_batch(
            "INSERT INTO tpir_output_origins(output_id, origin) VALUES (2, 2);
             INSERT INTO tpir_spend_origins(spending_transaction_id, prevout_txid, prevout_output_index, origin)
                 VALUES (2, X'03', 4, 3);",
        )
        .unwrap();
    assert!(!pending(&db.conn).unwrap());
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert_eq!(output_origins(&db.conn), vec![(1, 0), (2, 2)]);
    assert_eq!(
        db.conn
            .prepare(
                "SELECT prevout_output_index, origin FROM tpir_spend_origins
                 ORDER BY prevout_output_index, origin",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![(0, 0), (0, 1), (4, 3)]
    );
}
