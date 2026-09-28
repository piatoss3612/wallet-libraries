use super::*;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::{
    WalletRead,
    testing::{AddressType, IronwoodFvk},
};
use zcash_protocol::value::Zatoshis;

#[test]
fn local_restore_extends_lookahead_then_stops_at_fixed_target() {
    let height = BlockHeight::from_u32(100_000);
    let mut st = TestBuilder::new()
        .with_network(LocalNetwork {
            nu6: Some(height),
            nu6_1: Some(height),
            nu6_2: Some(height),
            nu6_3: Some(height),
            ..TestBuilder::<(), ()>::DEFAULT_NETWORK
        })
        .with_data_store_factory(TestDbFactory::file_backed())
        .with_block_cache(crate::testing::BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    let account = st.test_account().unwrap().id();
    let parent = FullViewingKey::from(st.test_account().unwrap().usk().orchard());
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    // The payment beyond the initial window arrives first. Extending the window
    // must replay old history, not just start watching the next block.
    for index in [50, 49] {
        let fvk = KeyId::new(Purpose::Receive, index).derive(&parent).unwrap();
        st.generate_next_block(
            &IronwoodFvk(fvk),
            AddressType::DefaultExternal,
            Zatoshis::const_from_u64(100_000),
        );
    }
    st.scan_cached_blocks(height, 2);
    let through = ChainAnchor {
        height: height + 1,
        hash: st
            .wallet()
            .db()
            .get_block_hash(height + 1)
            .unwrap()
            .unwrap()
            .0,
    };
    for expected_keys in [50, 100, 101] {
        let db = st.wallet_mut().db_mut();
        db.maintain_swap_receive_lookahead(account, 50, height)
            .unwrap();
        let keys = db.get_swap_receiving_keys(account).unwrap();
        assert_eq!(keys.len(), expected_keys);
        for key in keys {
            db.queue_swap_recovery_scan(account, key.key_id(), through)
                .unwrap();
        }
        assert!(
            db.get_swap_scan_window(through.height + 1)
                .unwrap()
                .0
                .is_empty()
        );
        st.scan_cached_blocks(height, 2);
    }
    let db = st.wallet_mut().db_mut();
    let notes = db
        .get_unspent_ironwood_notes_at_historical_height(account, through.height)
        .unwrap();
    assert_eq!(notes.len(), 2);
    for key in db.get_swap_receiving_keys(account).unwrap() {
        assert!(
            !db.swap_recovery_needs_directory(account, key.key_id(), through.height)
                .unwrap()
        );
        db.queue_swap_recovery_scan(account, key.key_id(), through)
            .unwrap();
    }
    assert_eq!(
        db.block_fully_scanned().unwrap().unwrap().block_height(),
        through.height
    );

    // A restart and tip advance do not turn the empty window into perpetual work.
    let mut reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    let key = KeyId::new(Purpose::Receive, 100);
    assert_eq!(
        reopened.swap_recovery_target(account, key).unwrap(),
        Some(through)
    );
    let (next, _, _) = st.generate_next_block(
        &IronwoodFvk(parent),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(10_000),
    );
    st.scan_cached_blocks(next, 1);
    let new_tip = ChainAnchor {
        height: next,
        hash: st.wallet().db().get_block_hash(next).unwrap().unwrap().0,
    };
    reopened
        .queue_swap_recovery_scan(account, key, new_tip)
        .unwrap();
    assert_eq!(
        reopened.swap_recovery_target(account, key).unwrap(),
        Some(through)
    );
    assert!(reopened.get_swap_scan_window(next).unwrap().0.is_empty());
    assert_eq!(
        reopened
            .block_fully_scanned()
            .unwrap()
            .unwrap()
            .block_height(),
        next
    );

    let invalid = ChainAnchor {
        hash: [42; 32],
        ..new_tip
    };
    assert!(
        reopened
            .queue_swap_recovery_scan(account, key, invalid)
            .is_err()
    );
    drop(reopened);
    st.truncate_to_height_retaining_cache(height);
    assert!(
        st.wallet()
            .db()
            .swap_recovery_target(account, key)
            .unwrap()
            .is_none()
    );
    st.scan_cached_blocks(height + 1, 2);
    st.wallet_mut()
        .db_mut()
        .queue_swap_recovery_scan(account, key, new_tip)
        .unwrap();
    assert!(
        st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, next)
            .unwrap()
    );
    st.scan_cached_blocks(height + 1, 2);
    assert!(
        !st.wallet()
            .db()
            .swap_receiving_needs_discovery(account, key, next)
            .unwrap()
    );
}

#[test]
fn local_coverage_does_not_cancel_a_known_operations_directory_closeout() {
    let (mut st, _, candidate, through, _) = super::apply::fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    let key = db
        .watch_swap_receive_key(account, 9, candidate.height)
        .unwrap()
        .key_id();
    db.observe_swap_operation(account, key, "pending", false, through.height)
        .unwrap();
    db.queue_swap_recovery_scan(account, key, through).unwrap();
    st.scan_cached_blocks(candidate.height, 1);
    let db = st.wallet_mut().db_mut();
    assert!(
        !db.swap_receiving_needs_discovery(account, key, through.height)
            .unwrap()
    );
    assert!(
        db.swap_recovery_needs_directory(account, key, through.height)
            .unwrap()
    );
    db.mark_swap_directory_checked(account, key, through)
        .unwrap();
    assert!(
        !db.swap_recovery_needs_directory(account, key, through.height)
            .unwrap()
    );
    assert!(
        db.get_swap_scan_window(through.height + 1)
            .unwrap()
            .0
            .iter()
            .any(|k| k.key_id() == key)
    );
}
