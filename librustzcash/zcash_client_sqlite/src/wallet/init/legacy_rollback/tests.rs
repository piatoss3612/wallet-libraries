use super::*;
use crate::testing::db::{test_clock, test_rng};
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

#[test]
fn legacy_rollback_round_trip_keeps_data_and_journal() {
    let mut db = db();
    db.conn.execute_batch("INSERT INTO transactions(txid, min_observed_height, raw, created, target_height) VALUES (X'0102', 42, X'0304', 123, 43);
        CREATE VIEW application_history AS SELECT txid FROM v_transactions;").unwrap();
    let before = journal(&db.conn);
    prepare_legacy_rollback(&mut db).unwrap();
    assert!(column_restored(&db.conn).unwrap());
    assert_eq!(
        db.conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.conn
            .query_row("SELECT zip318_kind FROM transactions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // The statement the published Orchard ingestion path always executes.
    db.conn
        .execute(
            "UPDATE transactions SET zip318_kind = ?1 WHERE txid = X'0102'",
            [1],
        )
        .unwrap();
    assert_eq!(journal(&db.conn), before);
    assert!(matches!(
        crate::wallet::transparent_ledger::durable_policy(&db.conn),
        Err(crate::error::SqliteClientError::LegacyRollbackPrepared)
    ));
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert!(!column_restored(&db.conn).unwrap());
    assert_eq!(journal(&db.conn), before);
    assert_eq!(
        db.conn
            .query_row(
                "SELECT min_observed_height, raw, created, target_height FROM transactions",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?
                ))
            )
            .unwrap(),
        (42, vec![3, 4], "123".to_string(), 43)
    );
    db.conn
        .prepare("SELECT * FROM application_history")
        .unwrap();
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    // Repeated preparation is safe and never removes/replays journal IDs.
    prepare_legacy_rollback(&mut db).unwrap();
    prepare_legacy_rollback(&mut db).unwrap();
    assert_eq!(journal(&db.conn), before);
}

#[test]
fn legacy_rollback_refuses_changed_or_private_policy_without_changing_schema() {
    for sql in [
        "UPDATE tpir_meta SET applied_mode = 1",
        "UPDATE tpir_meta SET policy_generation = 1",
        "UPDATE tpir_meta SET min_reader_version = 2",
    ] {
        let mut db = db();
        db.conn.execute(sql, []).unwrap();
        assert!(prepare_legacy_rollback(&mut db).is_err());
        assert!(!column_restored(&db.conn).unwrap());
    }
}

#[test]
fn legacy_rollback_schema_failure_is_atomic() {
    let mut db = db();
    db.conn.execute_batch("DROP VIEW v_transactions; CREATE VIEW v_transactions AS SELECT transactions.trust_status AS a, transactions.trust_status AS b FROM transactions").unwrap();
    // An unexpected canonical view refuses preparation; no compatibility column leaks.
    assert!(prepare_legacy_rollback(&mut db).is_err());
    assert!(!column_restored(&db.conn).unwrap());
}

#[test]
fn legacy_rollback_resume_failure_preserves_old_writes() {
    let mut db = db();
    prepare_legacy_rollback(&mut db).unwrap();
    db.conn
        .execute_batch(
            "INSERT INTO transactions(txid, min_observed_height, zip318_kind) VALUES (X'02', 7, 2);
        CREATE VIEW legacy_classifications AS SELECT zip318_kind FROM transactions;",
        )
        .unwrap();
    assert!(WalletMigrator::new().init_or_migrate(&mut db).is_err());
    assert!(column_restored(&db.conn).unwrap());
    assert_eq!(
        db.conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.conn
            .query_row("SELECT zip318_kind FROM transactions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    db.conn
        .execute_batch("DROP VIEW legacy_classifications")
        .unwrap();
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
}

#[test]
fn legacy_rollback_refuses_unknown_migrations_before_resume() {
    let mut db = db();
    prepare_legacy_rollback(&mut db).unwrap();
    db.conn
        .execute(
            "INSERT INTO schemer_migrations(id) VALUES (?1)",
            [Uuid::from_u128(123).as_bytes().to_vec()],
        )
        .unwrap();
    assert!(prepare_legacy_rollback(&mut db).is_err());
    assert!(column_restored(&db.conn).unwrap());
}

#[test]
fn legacy_rollback_refuses_recovery_state_even_with_public_metadata() {
    let mut db = db();
    db.conn.execute_batch("INSERT INTO tpir_revisions(source,revision,lineage,sealed,publication_height,publication_hash)
        VALUES (X'01', X'02', 1, 0, 42, zeroblob(32))").unwrap();
    assert!(prepare_legacy_rollback(&mut db).is_err());
    assert!(!column_restored(&db.conn).unwrap());
}

#[cfg(feature = "transparent-inputs")]
#[test]
fn legacy_rollback_reconciles_old_public_outputs_and_spends_without_authority() {
    use secrecy::SecretVec;
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite, chain::ChainState};
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::BlockHeight;
    let mut db = db();
    db.create_account(
        "rollback fixture",
        &SecretVec::new(vec![7; 32]),
        &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(1_200_000), BlockHash([0; 32])),
            None,
        ),
        None,
    )
    .unwrap();
    prepare_legacy_rollback(&mut db).unwrap();
    db.conn.execute_batch("INSERT INTO transactions(id_tx,txid,min_observed_height) VALUES (1,zeroblob(32),42);
        INSERT INTO transactions(id_tx,txid,min_observed_height,created,target_height) VALUES (2, X'02',43,123,44);
        INSERT INTO transparent_received_outputs(id,transaction_id,output_index,account_id,address,address_id,script,value_zat)
            SELECT 1,1,0,account_id,cached_transparent_receiver_address,id,X'00',1000 FROM addresses WHERE cached_transparent_receiver_address IS NOT NULL LIMIT 1;
        INSERT INTO transparent_received_outputs(id,transaction_id,output_index,account_id,address,address_id,script,value_zat)
            SELECT 2,2,0,account_id,cached_transparent_receiver_address,id,X'00',2000 FROM addresses WHERE cached_transparent_receiver_address IS NOT NULL LIMIT 1;
        INSERT INTO transparent_received_output_spends(transparent_received_output_id,transaction_id) VALUES (1,2);
        INSERT INTO transparent_spend_map(spending_transaction_id,prevout_txid,prevout_output_index) VALUES (2, X'03', 4);").unwrap();
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM tpir_output_origins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    let outputs: Vec<(i64, i64)> = db
        .conn
        .prepare("SELECT output_id,origin FROM tpir_output_origins ORDER BY output_id,origin")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(outputs, vec![(1, 0), (2, 0), (2, 1)]);
    assert_eq!(
        db.conn
            .query_row(
                "SELECT count(*) FROM tpir_spend_origins WHERE origin = 0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    assert_eq!(
        db.conn
            .query_row(
                "SELECT count(*) FROM tpir_spend_origins WHERE origin = 1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    for table in ["tpir_coverage", "tpir_active_accounts"] {
        assert_eq!(
            db.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    WalletMigrator::new().init_or_migrate(&mut db).unwrap();
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM tpir_output_origins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
}
