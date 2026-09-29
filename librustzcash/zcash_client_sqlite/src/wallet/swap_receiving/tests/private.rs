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
fn private_recovery_keeps_early_spends_and_empty_coverage_in_large_batches() {
    use orchard::keys::SpendingKey;
    use zakura_swap_receiving::lifecycle::ChainAnchor;
    use zcash_client_backend::data_api::testing::IronwoodFvk;
    use zcash_keys::address::{Address, UnifiedAddress};
    use zcash_protocol::value::Zatoshis;

    for enabled in [false, true] {
        for spend in [false, true] {
            let (mut st, key, candidate, original, _) = fixture();
            let account = st.test_account().unwrap().id();
            if enabled {
                st.wallet_mut()
                    .db_mut()
                    .enable_private_swap_recovery(account)
                    .unwrap();
            }
            let first = original.height + 1;
            if spend {
                let note = candidate
                    .encrypted_note
                    .decrypt(
                        &FullViewingKey::from(st.test_account().unwrap().usk().orchard()),
                        key.key_id(),
                    )
                    .unwrap();
                let noise = FullViewingKey::from(&SpendingKey::from_bytes([7; 32]).unwrap());
                st.generate_next_block_spending(
                    &IronwoodFvk(key.full_viewing_key().clone()),
                    (
                        note.note().nullifier(key.full_viewing_key()),
                        Zatoshis::const_from_u64(100_000),
                    ),
                    Address::Unified(
                        UnifiedAddress::from_receivers(
                            Some(noise.address_at(0u32, Scope::External)),
                            None,
                            None,
                        )
                        .unwrap(),
                    ),
                    Zatoshis::const_from_u64(100_000),
                );
            } else {
                st.generate_empty_block();
            }
            for _ in 0..200 {
                st.generate_empty_block();
            }
            st.scan_cached_blocks(first, 201);
            let last = first + 200;
            let through = ChainAnchor {
                height: last,
                hash: st.wallet().db().get_block_hash(last).unwrap().unwrap().0,
            };
            let status = st
                .wallet_mut()
                .db_mut()
                .swap_payment_spend_status(account, key.key_id(), &candidate, through)
                .unwrap();
            if enabled {
                assert!(matches!(status, SpendStatus::Spent { .. }) == spend);
                if !spend {
                    assert_eq!(status, SpendStatus::Unspent);
                }
                let covered: u32 = st.wallet().conn().query_row(
                    "SELECT COUNT(*) FROM ironwood_nullifier_scan_blocks WHERE height BETWEEN ?1 AND ?2",
                    rusqlite::params![u32::from(first), u32::from(last)], |row| row.get(0),
                ).unwrap();
                assert_eq!(covered, 201);
            } else {
                // The ordinary optimization remains enabled when late discovery is off.
                assert_eq!(status, SpendStatus::Unknown);
            }
        }
    }
}

#[test]
fn restored_refund_status_batches_prioritize_unchecked_operations() {
    let (mut st, _, _, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    let key = st
        .wallet_mut()
        .db_mut()
        .reserve_swap_receiving_key(account, Purpose::Refund, through.height)
        .unwrap();
    for operation in ["a", "b"] {
        st.wallet_mut()
            .db_mut()
            .observe_swap_operation(account, key.key_id(), operation, false, through.height)
            .unwrap();
        let id =
            super::super::payments::key_ref(st.wallet().conn(), account, key.key_id()).unwrap();
        st.wallet().conn().execute("INSERT INTO ironwood_swap_refund_watches(receiving_key_id,operation_id) VALUES(?1,?2)",
            rusqlite::params![id,operation]).unwrap();
    }
    let one = std::num::NonZeroU32::new(1).unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(account, 100, one)
            .unwrap(),
        vec![(key.key_id(), "a".into())]
    );
    // The first request's failure cannot prevent trying the next restored swap.
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(account, 101, one)
            .unwrap(),
        vec![(key.key_id(), "b".into())]
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(account, 159, one)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(account, 160, one)
            .unwrap(),
        vec![(key.key_id(), "a".into())]
    );
}

#[test]
fn discovery_work_retries_candidates_but_leaves_completed_and_pending_operations_alone() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.enable_private_swap_recovery(account).unwrap();
    db.watch_swap_receive_key(account, key.key_id().index(), through.height)
        .unwrap();
    let active = db
        .reserve_swap_receiving_key(account, Purpose::Refund, through.height)
        .unwrap();
    db.observe_swap_operation(account, active.key_id(), "pending", false, through.height)
        .unwrap();
    let work = db
        .prepare_swap_discovery_batch(
            account,
            through,
            1000,
            std::num::NonZeroU32::new(64).unwrap(),
        )
        .unwrap()
        .work;
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].key, key.key_id());
    assert_eq!(work[0].receiver, key.receiver().to_raw_address_bytes());
    assert_eq!(
        db.swap_recovery_target(account, key.key_id()).unwrap(),
        Some(through)
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
    db.mark_swap_directory_checked(account, key.key_id(), through)
        .unwrap();
    assert!(
        db.prepare_swap_discovery_batch(
            account,
            through,
            1000,
            std::num::NonZeroU32::new(64).unwrap()
        )
        .unwrap()
        .work
        .is_empty()
    );

    // A candidate queued after a completed check must still be applied.
    db.queue_swap_payment(account, key.key_id(), &candidate)
        .unwrap();
    let work = db
        .prepare_swap_discovery_batch(
            account,
            through,
            1000,
            std::num::NonZeroU32::new(64).unwrap(),
        )
        .unwrap()
        .work;
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].key, key.key_id());
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
    assert!(
        db.prepare_swap_discovery_batch(
            account,
            through,
            1000,
            std::num::NonZeroU32::new(64).unwrap()
        )
        .unwrap()
        .work
        .is_empty()
    );

    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    let (height, _) = st.generate_empty_block();
    st.scan_cached_blocks(height, 1);
    let tip = zakura_swap_receiving::lifecycle::ChainAnchor {
        height,
        hash: st.wallet().db().get_block_hash(height).unwrap().unwrap().0,
    };
    assert!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_discovery_batch(
                account,
                tip,
                1000,
                std::num::NonZeroU32::new(64).unwrap()
            )
            .unwrap()
            .work
            .is_empty()
    );
}
