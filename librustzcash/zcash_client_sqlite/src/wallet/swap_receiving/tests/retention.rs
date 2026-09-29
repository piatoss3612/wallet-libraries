use super::apply::fixture;
use super::*;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::WalletRead;

fn check_keys<CL, R>(
    db: &mut WalletDb<Connection, LocalNetwork, CL, R>,
    account: AccountUuid,
    through: ChainAnchor,
) {
    for key in db.get_swap_receiving_keys(account).unwrap() {
        db.finish_swap_discovery_attempt(account, key.key_id(), through, 1_000_000)
            .unwrap();
    }
}

#[test]
fn retention_completion_waits_for_candidates_and_extended_lookahead() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    assert!(
        !db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    assert_eq!(
        db.apply_pending_swap_payment(
            account,
            key.key_id(),
            &candidate,
            through,
            Some((through, &path))
        )
        .unwrap(),
        PaymentApplication::Applied
    );
    // Import advances allocation. Finishing must create and wait for the new window.
    assert!(
        !db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    check_keys(db, account, through);
    assert!(
        db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        Some(through.height + 1)
    );
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        Some(through.height + 1)
    );
    let wrong = ChainAnchor {
        hash: [99; 32],
        ..through
    };
    assert!(
        !st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, wrong, 1)
            .unwrap()
    );
}

#[test]
fn retention_prunes_other_pools_and_respects_the_oldest_account() {
    use zcash_protocol::PoolType;
    let mut st = wallet(false);
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    let birthday = st.test_account().unwrap().birthday().clone();
    let (other, _) = st
        .wallet_mut()
        .db_mut()
        .create_account("other", &seed, &birthday, None)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(other)
        .unwrap();
    let conn = st.wallet_mut().conn_mut();
    // Use small synthetic heights to isolate shared pruning from network activation.
    conn.execute(
        "UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=0",
        [],
    )
    .unwrap();
    conn.execute("UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=200 WHERE account_id=(SELECT id FROM accounts WHERE uuid=?1)", [account.0]).unwrap();
    for (pool, nf) in [
        (PoolType::SAPLING, 1u8),
        (PoolType::ORCHARD, 2),
        (PoolType::IRONWOOD, 3),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO tx_locator_map VALUES(100,0,?1)",
            [[9u8; 32]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nullifier_map VALUES(?1,?2,100,0)",
            rusqlite::params![crate::wallet::encoding::pool_code(pool), [nf; 32]],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO ironwood_nullifier_scan_blocks VALUES(100)", [])
        .unwrap();
    let tx = conn.transaction().unwrap();
    crate::wallet::prune_nullifier_map(&tx, 300.into()).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM nullifier_map", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM tx_locator_map", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    tx.commit().unwrap();
    conn.execute(
        "UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=250",
        [],
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    crate::wallet::prune_nullifier_map(&tx, 300.into()).unwrap();
    for table in [
        "nullifier_map",
        "tx_locator_map",
        "ironwood_nullifier_scan_blocks",
    ] {
        assert_eq!(
            tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }
    tx.commit().unwrap();
}

#[test]
fn missing_spend_history_queues_replay_and_recovers_after_restart() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    st.wallet()
        .conn()
        .execute("DELETE FROM ironwood_nullifier_scan_blocks", [])
        .unwrap();
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_swap_private_recovery SET nullifier_retention_height=?1",
            [u32::from(through.height + 1)],
        )
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .apply_pending_swap_payment(
                account,
                key.key_id(),
                &candidate,
                through,
                Some((through, &path))
            )
            .unwrap(),
        PaymentApplication::AwaitingSpendHistory
    );
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    let ranges = st.wallet().db().suggest_scan_ranges().unwrap();
    assert!(
        ranges
            .iter()
            .any(|r| r.block_range().contains(&candidate.height))
    );
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, through.height)
            .unwrap()
            .is_empty()
    );
    st.scan_cached_blocks(candidate.height, 1);
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .apply_pending_swap_payment(
                account,
                key.key_id(),
                &candidate,
                through,
                Some((through, &path))
            )
            .unwrap(),
        PaymentApplication::Applied
    );
}

#[test]
fn retention_waits_for_internal_memos_and_own_send_evidence() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    db.apply_pending_swap_payment(
        account,
        key.key_id(),
        &candidate,
        through,
        Some((through, &path)),
    )
    .unwrap();
    assert!(
        !db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    check_keys(db, account, through);
    // Model a normal internal note whose memo enhancement has not finished.
    st.wallet().conn().execute("UPDATE ironwood_received_notes SET receiving_key_id=NULL,recipient_key_scope=1,memo=NULL", []).unwrap();
    assert!(
        !st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        Some(through.height)
    );
    // Even an enhanced marker waits if its funding-account evidence is unresolved.
    st.wallet()
        .conn()
        .execute("UPDATE ironwood_received_notes SET memo=X'FF5A535750'", [])
        .unwrap();
    assert!(
        !st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    st.wallet()
        .conn()
        .execute("UPDATE ironwood_received_notes SET memo=X'F6'", [])
        .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
}

#[test]
fn retention_rewinds_and_resumes_for_new_blocks() {
    let (mut st, key, candidate, original, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    db.apply_pending_swap_payment(
        account,
        key.key_id(),
        &candidate,
        original,
        Some((original, &path)),
    )
    .unwrap();
    assert!(
        !db.finish_swap_nullifier_recovery(account, original, 1)
            .unwrap()
    );
    check_keys(db, account, original);
    assert!(
        db.finish_swap_nullifier_recovery(account, original, 1)
            .unwrap()
    );
    let (height, _) = st.generate_empty_block();
    st.scan_cached_blocks(height, 1);
    let tip = ChainAnchor {
        height,
        hash: st.wallet().get_block_hash(height).unwrap().unwrap().0,
    };
    check_keys(st.wallet_mut().db_mut(), account, tip);
    assert!(
        st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, tip, 1)
            .unwrap()
    );
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        Some(height + 1)
    );
    st.truncate_to_height_retaining_cache(original.height);
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        Some(original.height + 1)
    );
    st.scan_cached_blocks(height, 1);
    assert_eq!(
        st.wallet()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ironwood_nullifier_scan_blocks WHERE height=?1",
                [u32::from(height)],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn completed_recovery_retains_the_next_large_batch_until_reconciled() {
    let (mut st, key, candidate, original, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    db.apply_pending_swap_payment(
        account,
        key.key_id(),
        &candidate,
        original,
        Some((original, &path)),
    )
    .unwrap();
    assert!(
        !db.finish_swap_nullifier_recovery(account, original, 1)
            .unwrap()
    );
    check_keys(db, account, original);
    assert!(
        db.finish_swap_nullifier_recovery(account, original, 1)
            .unwrap()
    );
    for _ in 0..201 {
        st.generate_empty_block();
    }
    st.scan_cached_blocks(original.height + 1, 201);
    let height = original.height + 201;
    let through = ChainAnchor {
        height,
        hash: st.wallet().get_block_hash(height).unwrap().unwrap().0,
    };
    let count = |conn: &Connection| {
        conn.query_row(
            "SELECT COUNT(*) FROM ironwood_nullifier_scan_blocks WHERE height>?1",
            [u32::from(original.height)],
            |r| r.get::<_, u32>(0),
        )
        .unwrap()
    };
    assert_eq!(count(st.wallet().conn()), 201);
    check_keys(st.wallet_mut().db_mut(), account, through);
    assert!(
        st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    assert_eq!(count(st.wallet().conn()), crate::PRUNING_DEPTH + 1);
    assert_eq!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, height)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn completion_does_not_enable_discovery_for_an_unregistered_account() {
    let (mut st, _, _, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    let before = st
        .wallet()
        .db()
        .get_swap_receiving_keys(account)
        .unwrap()
        .len();
    assert!(
        !st.wallet_mut()
            .db_mut()
            .finish_swap_nullifier_recovery(account, through, 50)
            .unwrap()
    );
    assert_eq!(
        st.wallet()
            .db()
            .get_swap_receiving_keys(account)
            .unwrap()
            .len(),
        before
    );
    assert_eq!(
        crate::wallet::ironwood_nullifier_retention_height(st.wallet().conn()).unwrap(),
        None
    );
}

#[test]
fn covered_history_releases_while_provider_outcome_is_pending() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    db.record_swap_observation(
        account,
        key.key_id(),
        "restored",
        zakura_swap_receiving::lifecycle::OperationStatus::Active,
        1000,
        false,
    )
    .unwrap();
    db.apply_pending_swap_payment(
        account,
        key.key_id(),
        &candidate,
        through,
        Some((through, &path)),
    )
    .unwrap();
    assert!(
        !db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
    // Close the new lookahead and record processed coverage for the unresolved swap.
    for k in db.get_swap_receiving_keys(account).unwrap() {
        db.finish_swap_discovery_attempt(account, k.key_id(), through, 1000)
            .unwrap();
    }
    assert!(
        db.finish_swap_nullifier_recovery(account, through, 1)
            .unwrap()
    );
}

#[test]
fn repeated_missing_history_does_not_restart_the_public_replay() {
    let (mut st, _, _, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.transactionally::<_, _, Error>(|tx| tx.queue_swap_spend_history(account, through.height))
        .unwrap();
    db.conn.borrow().execute_batch("CREATE TEMP TABLE replay_queue_writes(n INTEGER); INSERT INTO replay_queue_writes VALUES(0);
        CREATE TEMP TRIGGER replay_insert AFTER INSERT ON scan_queue BEGIN UPDATE replay_queue_writes SET n=n+1; END;
        CREATE TEMP TRIGGER replay_delete AFTER DELETE ON scan_queue BEGIN UPDATE replay_queue_writes SET n=n+1; END;
        CREATE TEMP TRIGGER replay_update AFTER UPDATE ON scan_queue BEGIN UPDATE replay_queue_writes SET n=n+1; END;").unwrap();
    db.transactionally::<_, _, Error>(|tx| tx.queue_swap_spend_history(account, through.height))
        .unwrap();
    assert_eq!(
        db.conn
            .borrow()
            .query_row("SELECT n FROM replay_queue_writes", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn note_before_public_restore_bound_is_blocked_without_queuing_replay() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    let (owner, _) = account_key(db.conn.borrow(), &db.params, account).unwrap();
    db.conn
        .borrow()
        .execute(
            "UPDATE accounts SET birthday_height=?2 WHERE id=?1",
            rusqlite::params![owner.0, u32::from(candidate.height) + 1],
        )
        .unwrap();
    assert_eq!(
        db.apply_pending_swap_payment(
            account,
            key.key_id(),
            &candidate,
            through,
            Some((through, &path))
        )
        .unwrap(),
        PaymentApplication::OutsideRecoveryRange
    );
    assert_eq!(
        db.pending_swap_payments(account, key.key_id())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.conn
            .borrow()
            .query_row("SELECT COUNT(*) FROM ironwood_swap_spend_replay", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}
