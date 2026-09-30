use super::*;

#[test]
fn historical_writer_statements_fail_without_partial_wallet_changes() {
    let (mut st, account, _, _) = active_and_candidate();
    let before = production_dump(conn(&st));
    let evidence = recovery(&st, account);
    let legacy = rusqlite::Connection::open(st.wallet().data_file_path()).unwrap();
    for statement in [
        "DELETE FROM blocks WHERE height > 0",
        "UPDATE blocks SET hash = zeroblob(32)",
        "INSERT OR REPLACE INTO blocks SELECT * FROM blocks LIMIT 1",
        "DELETE FROM scan_queue",
        "UPDATE transactions SET mined_height = NULL",
        "UPDATE addresses SET account_id = account_id",
        "UPDATE accounts SET birthday_height = birthday_height",
    ] {
        let error = legacy.execute_batch(statement).unwrap_err();
        assert!(
            error.to_string().contains("tpir_writer_version"),
            "{statement}: {error}"
        );
        assert_eq!(production_dump(conn(&st)), before);
        assert_eq!(recovery(&st, account), evidence);
    }
    // Explicit transactions roll back earlier writes when the guarded statement fails.
    legacy
        .execute_batch("BEGIN; UPDATE tpir_meta SET policy_generation = policy_generation + 1;")
        .unwrap();
    assert!(legacy.execute_batch("DELETE FROM blocks").is_err());
    legacy.execute_batch("ROLLBACK").unwrap();
    assert_eq!(recovery(&st, account), evidence);
    // A current rewind retains the supported lifecycle behavior.
    let floor = watch(&st, account).target.unwrap().height - 2;
    st.wallet_mut().truncate_to_height(floor).unwrap();
    assert!(
        recovery(&st, account)
            .receives
            .iter()
            .all(|r| r.mined_height <= floor)
    );
    if let Ok(path) = std::env::var("TPIR_OLD_WRITER_FIXTURE") {
        std::fs::copy(st.wallet().data_file_path(), path).unwrap();
    }
}

#[test]
fn damaged_coverage_and_page_anchors_are_not_recovery_authority() {
    let (mut st, account) = shadow_wallet();
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    conn(&st)
        .execute("UPDATE tpir_coverage SET anchor_hash = zeroblob(32)", [])
        .unwrap();
    assert!(
        st.wallet()
            .db()
            .transparent_candidate_recovery(account)
            .is_err()
    );
    assert!(
        st.wallet()
            .db()
            .transparent_recovery_work(account, NonZeroUsize::new(1).unwrap())
            .is_err()
    );
    conn(&st).execute("DELETE FROM tpir_coverage", []).unwrap();
    let mut c = commit(&ws);
    c.opened_pages = vec![PageRequest {
        page: b"bad-anchor".to_vec(),
        addresses: vec![external(&ws)],
        from: ws.target.unwrap().height,
        through: ws.target.unwrap().height,
    }];
    apply(&mut st, c).unwrap();
    conn(&st)
        .execute(
            "UPDATE tpir_pending_pages SET target_hash = zeroblob(32)",
            [],
        )
        .unwrap();
    assert!(st.wallet().db().transparent_watch_set(account).is_err());
}

#[test]
fn supplied_and_reopened_connections_register_current_writer_capability() {
    let (st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let mut db = crate::WalletDb::from_connection(
        rusqlite::Connection::open(st.wallet().data_file_path()).unwrap(),
        *st.network(),
        crate::testing::db::test_clock(),
        crate::testing::db::test_rng(),
    );
    db.set_transparent_ledger_mode(PrivateShadow);
    db.transactionally::<_, _, SqliteClientError>(|tx| {
        tx.conn.0.execute("UPDATE blocks SET hash = hash", [])?;
        tx.get_wallet_summary(ConfirmationsPolicy::MIN)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        db.transparent_watch_set(account).unwrap(),
        watch(&st, account)
    );
    let reopened = crate::WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        crate::testing::db::test_clock(),
        crate::testing::db::test_rng(),
    )
    .unwrap();
    reopened
        .conn
        .execute("UPDATE blocks SET hash = hash", [])
        .unwrap();
}
