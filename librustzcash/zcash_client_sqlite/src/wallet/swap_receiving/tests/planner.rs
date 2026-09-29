use super::*;
use rusqlite::params;
use std::num::NonZeroU32;
use zakura_swap_receiving::lifecycle::{ChainAnchor, OperationStatus, ReceiptExpectation};
use zcash_client_backend::data_api::WalletRead;
fn anchor(st: &TestState<crate::testing::BlockCache, TestDb, LocalNetwork>) -> ChainAnchor {
    use zcash_client_backend::data_api::WalletRead;
    let tip = st.wallet().db().block_fully_scanned().unwrap().unwrap();
    ChainAnchor {
        height: tip.block_height(),
        hash: tip.block_hash().0,
    }
}

#[test]
fn bounded_metadata_batches_do_not_derive_ten_thousand_historical_keys() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let (owner, _) = account_key(db.conn.borrow(), &db.params, account).unwrap();
    db.transactionally::<_,_,Error>(|db| {
        let mut insert=db.conn.0.prepare("INSERT INTO ironwood_receiving_keys(account_id,purpose,derivation_version,key_index,receiver,scan_from,advances_allocation)
            VALUES(?1,0,1,?2,zeroblob(43),?3,1)")?;
        for i in 0u64..10_000 {insert.execute(params![owner.0,i.to_be_bytes(),u32::from(through.height)])?;}
        Ok(())
    }).unwrap();
    let limit = NonZeroU32::new(64).unwrap();
    let batch = db
        .prepare_swap_discovery_batch(account, through, 1000, limit)
        .unwrap();
    assert_eq!(batch.work.len(), 64);
    assert_eq!(batch.remaining_lookups, 10_000);
    // Only the first attempted record is leased. A crash before the tail starts
    // leaves all 63 unstarted records immediately eligible.
    let first = batch.work[0].key;
    db.begin_swap_discovery_attempt(account, first, 1000)
        .unwrap();
    let batch = db
        .prepare_swap_discovery_batch(account, through, 1000, limit)
        .unwrap();
    assert_eq!(batch.remaining_lookups, 9999);
    assert_eq!(batch.work[0].key.index(), 1);
    assert!(batch.work.iter().all(|w| w.key != first));
    let conn = Connection::open(st.wallet().data_file_path()).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT next_attempt_at FROM ironwood_swap_discovery ORDER BY receiving_key_id LIMIT 1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1060
    );
    // Restored registry rows never become trial-decryption keys.
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(through.height)
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn forty_active_keys_have_no_count_cap_and_terminal_waits_for_fresh_context() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let mut keys = Vec::new();
    for _ in 0..40 {
        let key = db
            .reserve_swap_receiving_key(account, Purpose::Refund, through.height)
            .unwrap()
            .key_id();
        db.record_swap_observation(account, key, "local", OperationStatus::Active, 1000, true)
            .unwrap();
        keys.push(key);
    }
    assert_eq!(db.get_swap_scan_window(through.height).unwrap().0.len(), 40);
    let terminal = OperationStatus::Terminal(ReceiptExpectation::None);
    db.record_swap_observation(account, keys[0], "local", terminal, 1100, true)
        .unwrap();
    db.anchor_swap_observations(through, 1099).unwrap();
    assert_eq!(
        db.get_swap_scan_window(through.height + 11)
            .unwrap()
            .0
            .len(),
        40
    );
    db.anchor_swap_observations(through, 1101).unwrap();
    assert_eq!(
        db.get_swap_scan_window(through.height + 11)
            .unwrap()
            .0
            .len(),
        39
    );
    // Repeated status preserves the first observation; stale active status cannot reopen it.
    db.record_swap_observation(account, keys[0], "local", terminal, 1200, true)
        .unwrap();
    db.record_swap_observation(
        account,
        keys[0],
        "local",
        OperationStatus::Active,
        1001,
        true,
    )
    .unwrap();
    assert_eq!(
        db.get_swap_scan_window(through.height + 11)
            .unwrap()
            .0
            .len(),
        39
    );
}

#[test]
fn directory_coverage_survives_status_changes_and_positive_receipt_cannot_close_empty() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let key = db
        .recover_swap_receiving_key(account, KeyId::new(Purpose::Refund, 9), through.height)
        .unwrap()
        .key_id();
    db.record_swap_observation(
        account,
        key,
        "restored",
        OperationStatus::Active,
        1000,
        false,
    )
    .unwrap();
    db.queue_swap_lookup(account, key, through, &[]).unwrap();
    assert!(
        !db.finish_swap_discovery_attempt(account, key, through, 1000)
            .unwrap()
    );
    db.record_swap_observation(
        account,
        key,
        "restored",
        OperationStatus::Terminal(ReceiptExpectation::Positive(None)),
        1100,
        false,
    )
    .unwrap();
    assert_eq!(
        db.swap_directory_check(account, key).unwrap(),
        Some(through)
    );
    db.anchor_swap_observations(through, 1101).unwrap();
    assert!(
        !db.finish_swap_discovery_attempt(account, key, through, 50_000)
            .unwrap()
    );
    assert!(
        db.get_swap_scan_window(through.height)
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn delayed_check_closes_once_and_reorg_reopens_it() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let initial = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let key = db
        .recover_swap_receiving_key(account, KeyId::new(Purpose::Refund, 2), initial.height)
        .unwrap()
        .key_id();
    db.record_swap_observation(
        account,
        key,
        "restored",
        OperationStatus::Terminal(ReceiptExpectation::None),
        1000,
        false,
    )
    .unwrap();
    db.anchor_swap_observations(initial, 1001).unwrap();
    db.queue_swap_lookup(account, key, initial, &[]).unwrap();
    assert!(
        !db.finish_swap_discovery_attempt(account, key, initial, 1001)
            .unwrap()
    );
    st.generate_and_scan_empty_blocks(11);
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    let limit = NonZeroU32::new(64).unwrap();
    assert!(
        db.prepare_swap_discovery_batch(account, through, 44199, limit)
            .unwrap()
            .work
            .is_empty()
    );
    let batch = db
        .prepare_swap_discovery_batch(account, through, 44200, limit)
        .unwrap();
    assert_eq!(batch.work.len(), 1);
    assert_eq!(batch.remaining_lookups, 1);
    db.queue_swap_lookup(account, key, through, &[]).unwrap();
    assert!(
        db.finish_swap_discovery_attempt(account, key, through, 44200)
            .unwrap()
    );
    assert!(
        db.prepare_swap_discovery_batch(account, through, 90000, limit)
            .unwrap()
            .work
            .is_empty()
    );
    st.truncate_to_height_retaining_cache(initial.height);
    let db = st.wallet_mut().db_mut();
    assert!(db.swap_lookup_coverage(account, key).unwrap().is_none());
    assert_eq!(
        db.prepare_swap_discovery_batch(account, initial, 90000, limit)
            .unwrap()
            .work
            .len(),
        1
    );
}

#[test]
fn incomplete_lookup_is_atomic_and_pending_ciphertext_survives_restart() {
    let (mut st, key, candidate, through, _) = super::apply::fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.conn
        .borrow()
        .execute("DELETE FROM ironwood_swap_payment_recovery", [])
        .unwrap();
    let mut bad = candidate.clone();
    bad.position += 1; // same output identity, conflicting location
    assert!(
        db.queue_swap_lookup(account, key.key_id(), through, &[candidate.clone(), bad])
            .is_err()
    );
    assert!(
        db.pending_swap_payments(account, key.key_id())
            .unwrap()
            .is_empty()
    );
    assert!(
        db.swap_lookup_coverage(account, key.key_id())
            .unwrap()
            .is_none()
    );
    db.queue_swap_lookup(account, key.key_id(), through, &[candidate.clone()])
        .unwrap();
    assert_eq!(
        db.swap_lookup_coverage(account, key.key_id()).unwrap(),
        Some(through)
    );
    assert!(
        db.finish_swap_discovery_attempt(account, key.key_id(), through, 1000)
            .is_err()
    );
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .pending_swap_payments(account, key.key_id())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .swap_lookup_coverage(account, key.key_id())
            .unwrap(),
        Some(through)
    );
}

#[test]
fn status_outage_moves_only_stale_local_operations_to_directory() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let old = db
        .reserve_swap_receiving_key(account, Purpose::Refund, through.height)
        .unwrap()
        .key_id();
    let active = db
        .reserve_swap_receiving_key(account, Purpose::Refund, through.height)
        .unwrap()
        .key_id();
    for key in [old, active] {
        db.record_swap_observation(account, key, "local", OperationStatus::Active, 1000, true)
            .unwrap();
    }
    db.record_swap_observation(
        account,
        active,
        "local",
        OperationStatus::Active,
        200000,
        false,
    )
    .unwrap();
    db.anchor_swap_observations(through, 200001).unwrap();
    let keys = db.get_swap_scan_window(through.height).unwrap().0;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_id(), active);
    let batch = db
        .prepare_swap_discovery_batch(account, through, 200001, NonZeroU32::new(64).unwrap())
        .unwrap();
    assert_eq!(batch.work.len(), 1);
    assert_eq!(batch.work[0].key, old);
    assert!(db.swap_history_pending(account, through.height).unwrap());
    db.begin_swap_discovery_attempt(account, old, 200001)
        .unwrap();
    assert!(
        db.prepare_swap_discovery_batch(account, through, 200001, NonZeroU32::new(64).unwrap())
            .unwrap()
            .work
            .is_empty()
    );
    assert!(db.swap_history_pending(account, through.height).unwrap());
}

#[test]
fn newly_reported_restored_refund_does_not_wait_for_delayed_closeout() {
    let mut st = reservations::fixture();
    let account = st.test_account().unwrap().id();
    let initial = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let key = db
        .recover_swap_receiving_key(account, KeyId::new(Purpose::Refund, 3), initial.height)
        .unwrap()
        .key_id();
    db.record_swap_observation(
        account,
        key,
        "restored",
        OperationStatus::Active,
        1000,
        false,
    )
    .unwrap();
    db.queue_swap_lookup(account, key, initial, &[]).unwrap();
    db.finish_swap_discovery_attempt(account, key, initial, 1000)
        .unwrap();
    st.generate_and_scan_empty_blocks(1);
    let through = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.record_swap_observation(
        account,
        key,
        "restored",
        OperationStatus::Terminal(ReceiptExpectation::Positive(None)),
        1010,
        false,
    )
    .unwrap();
    let work = db
        .prepare_swap_discovery_batch(account, through, 1010, NonZeroU32::new(64).unwrap())
        .unwrap();
    assert_eq!(work.work.len(), 1);
    assert_eq!(work.remaining_lookups, 1);
    db.queue_swap_lookup(account, key, through, &[]).unwrap();
    assert!(
        !db.finish_swap_discovery_attempt(account, key, through, 1010)
            .unwrap()
    );
    assert_eq!(
        db.prepare_swap_discovery_batch(account, through, 4610, NonZeroU32::new(64).unwrap())
            .unwrap()
            .work
            .len(),
        1
    );
}
