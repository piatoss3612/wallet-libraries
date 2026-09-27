use super::apply::fixture;
use super::*;
use zcash_client_backend::data_api::WalletRead;

#[test]
fn private_recovery_retains_spend_history_and_does_not_queue_key_replay() {
    let (mut st, key, candidate, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .watch_swap_receive_key(account, 9, candidate.height)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .block_fully_scanned()
            .unwrap()
            .unwrap()
            .block_height(),
        through.height
    );
    st.wallet_mut()
        .db_mut()
        .transactionally(|db| -> Result<(), SqliteClientError> {
            crate::wallet::prune_nullifier_map(db.conn.0, through.height + 101)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .swap_payment_spend_status(account, key.key_id(), &candidate, through)
            .unwrap(),
        SpendStatus::Unspent
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .mark_swap_directory_checked(account, key.key_id(), through)
            .is_err()
    );
    let other = KeyId::new(Purpose::Receive, 9);
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(account, other, through)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .swap_directory_check(account, other)
            .unwrap(),
        Some(through)
    );
}

#[test]
fn completed_directory_coverage_requires_continuous_scan_coverage() {
    let (mut st, _, candidate, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    let key = KeyId::new(Purpose::Receive, 9);
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .watch_swap_receive_key(account, 9, candidate.height)
        .unwrap();
    assert!(
        st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, through.height)
            .unwrap()
    );
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(account, key, "local-pending", false, through.height)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(account, key, through)
        .unwrap();
    assert!(
        !st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, through.height)
            .unwrap()
    );
    // A height beyond both forms of coverage remains a gap, never a completed lookup.
    assert!(
        st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, through.height + 1)
            .unwrap()
    );
    let registered = st
        .wallet()
        .db()
        .get_swap_receiving_keys(account)
        .unwrap()
        .into_iter()
        .find(|k| k.key_id() == key)
        .unwrap();
    let (height, _, _) = st.generate_next_block(
        &zcash_client_backend::data_api::testing::IronwoodFvk(
            registered.full_viewing_key().clone(),
        ),
        zcash_client_backend::data_api::testing::AddressType::DefaultExternal,
        zcash_protocol::value::Zatoshis::const_from_u64(10_000),
    );
    st.scan_cached_blocks(height, 1);
    assert!(
        !st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, height)
            .unwrap()
    );
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account, height)
        .unwrap();
    assert!(notes.iter().any(|note| note.swap_key_id() == Some(key)));
}

#[test]
fn private_scan_deadline_survives_restart_and_delayed_directory_then_reorgs() {
    use zcash_client_backend::data_api::testing::{AddressType, IronwoodFvk};
    use zcash_protocol::value::Zatoshis;
    let (mut st, _, _, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    let key = st
        .wallet_mut()
        .db_mut()
        .reserve_swap_receiving_key(account, Purpose::Refund, through.height + 1)
        .unwrap();
    let lookahead = st
        .wallet_mut()
        .db_mut()
        .watch_swap_receive_key(account, 10, through.height)
        .unwrap();
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(through.height + 1)
            .unwrap()
            .0
            .is_empty()
    );
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(account, key.key_id(), "swap", false, through.height)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .get_swap_scan_window(through.height + 1)
            .unwrap()
            .0
            .len(),
        1
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(account, key.key_id(), through)
            .unwrap(),
        None
    );
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(account, key.key_id(), "swap", true, through.height)
        .unwrap();
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    // Re-observation after reopening must not move the saved deadline.
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(account, key.key_id(), "swap", true, through.height + 5)
        .unwrap();
    let end = through.height + 10;
    for _ in 0..12 {
        st.generate_next_block(
            &IronwoodFvk(key.full_viewing_key().clone()),
            AddressType::DefaultExternal,
            Zatoshis::const_from_u64(20_000),
        );
    }
    // Even a batch crossing the deadline is split before an inactive key is tried.
    let summary = st.scan_cached_blocks(through.height + 1, 12);
    assert_eq!(summary.scanned_range().end, end + 1);
    st.scan_cached_blocks(end + 1, 2);
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(end + 1)
            .unwrap()
            .0
            .is_empty()
    );
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account, end + 2)
        .unwrap();
    assert_eq!(
        notes
            .iter()
            .filter(|n| n.swap_key_id() == Some(key.key_id()))
            .count(),
        10
    );
    let tip = zakura_swap_receiving::lifecycle::ChainAnchor {
        height: end + 2,
        hash: st.wallet().db().get_block_hash(end + 2).unwrap().unwrap().0,
    };
    let target = st
        .wallet_mut()
        .db_mut()
        .prepare_swap_recovery_target(account, key.key_id(), tip)
        .unwrap()
        .unwrap();
    assert_eq!(target.height, end);
    assert!(
        st.wallet()
            .db()
            .swap_directory_check(account, key.key_id())
            .unwrap()
            .is_none()
    );
    // A completed empty lookahead query is fixed too, despite later tip movement.
    let restored = st
        .wallet_mut()
        .db_mut()
        .prepare_swap_recovery_target(account, lookahead.key_id(), tip)
        .unwrap()
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(account, lookahead.key_id(), tip)
        .unwrap();
    let (next, _) = st.generate_empty_block();
    st.scan_cached_blocks(next, 1);
    let newer = zakura_swap_receiving::lifecycle::ChainAnchor {
        height: next,
        hash: st.wallet().db().get_block_hash(next).unwrap().unwrap().0,
    };
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(account, key.key_id(), newer)
            .unwrap(),
        Some(target)
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(account, lookahead.key_id(), newer)
            .unwrap(),
        Some(restored)
    );
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(account, key.key_id(), tip)
        .unwrap();
    // Canonical rewind invalidates the PIR anchors and reactivates only the bounded window.
    st.truncate_to_height_retaining_cache(end - 1);
    assert_eq!(
        st.wallet()
            .db()
            .swap_recovery_target(account, key.key_id())
            .unwrap(),
        None
    );
    assert_eq!(
        st.wallet()
            .db()
            .swap_directory_check(account, key.key_id())
            .unwrap(),
        None
    );
    assert_eq!(
        st.wallet().db().get_swap_scan_window(end).unwrap().0.len(),
        1
    );
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(end + 1)
            .unwrap()
            .0
            .is_empty()
    );
    st.scan_cached_blocks(end, 1);
    let anchor = zakura_swap_receiving::lifecycle::ChainAnchor {
        height: end,
        hash: st.wallet().db().get_block_hash(end).unwrap().unwrap().0,
    };
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(account, key.key_id(), anchor)
            .unwrap(),
        Some(anchor)
    );
    // A distinct pending use sharing the address must keep it active.
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(account, key.key_id(), "another-swap", false, end)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .get_swap_scan_window(end + 1)
            .unwrap()
            .0
            .len(),
        1
    );
    assert_eq!(
        st.wallet()
            .db()
            .swap_recovery_target(account, key.key_id())
            .unwrap(),
        None
    );
}
