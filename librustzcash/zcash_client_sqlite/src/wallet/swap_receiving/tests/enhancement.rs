use super::*;
use std::convert::Infallible;
use zcash_client_backend::{
    data_api::{
        WalletRead,
        testing::{AddressType, IronwoodFvk},
        wallet::{
            ConfirmationsPolicy, decrypt_and_store_transaction,
            input_selection::GreedyInputSelector,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard},
    wallet::OvkPolicy,
};
use zcash_keys::address::{Address, UnifiedAddress};
use zcash_protocol::{ShieldedPool, memo::MemoBytes, value::Zatoshis};
use zip321::{Payment, TransactionRequest};

fn full_transaction_roundtrip(full_first: bool) {
    let activation = BlockHeight::from_u32(100_000);
    let network = LocalNetwork {
        nu6: Some(activation),
        nu6_1: Some(activation),
        nu6_2: Some(activation),
        nu6_3: Some(activation),
        ..TestBuilder::<(), ()>::DEFAULT_NETWORK
    };
    let mut st = TestBuilder::new()
        .with_network(network)
        .with_data_store_factory(TestDbFactory::file_backed())
        .with_block_cache(crate::testing::BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    let account = st.test_account().cloned().unwrap();
    let parent = orchard::keys::FullViewingKey::from(account.usk().orchard());
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(parent),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(1_000_000),
    );
    st.scan_cached_blocks(h, 1);
    for _ in 0..5 {
        let (h, _) = st.generate_empty_block();
        st.scan_cached_blocks(h, 1);
    }

    let scan_from = st.wallet().chain_height().unwrap().unwrap() + 1;
    let keys: Vec<_> = [Purpose::Refund, Purpose::Receive]
        .into_iter()
        .map(|purpose| {
            st.wallet_mut()
                .db_mut()
                .reserve_swap_receiving_key(account.id(), purpose, scan_from)
                .unwrap()
        })
        .collect();
    let memo = MemoBytes::from_bytes(b"swap payment memo").unwrap();
    let payments = keys
        .iter()
        .map(|key| {
            let address = Address::Unified(
                UnifiedAddress::from_receivers(Some(key.receiver()), None, None).unwrap(),
            );
            Payment::new(
                address.to_zcash_address(&network),
                Some(Zatoshis::const_from_u64(50_000)),
                Some(memo.clone()),
                None,
                None,
                vec![],
            )
            .unwrap()
        })
        .collect();
    let strategy = standard::SingleOutputChangeStrategy::<TestDb>::new(
        StandardFeeRule::Zip317,
        None,
        ShieldedPool::Orchard,
        DustOutputPolicy::default(),
    );
    let proposal = st
        .propose_transfer(
            account.id(),
            &GreedyInputSelector::new(),
            &strategy,
            TransactionRequest::new(payments).unwrap(),
            ConfirmationsPolicy::MIN,
        )
        .unwrap();
    let created = st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();
    let tx = st.wallet().get_transaction(created[0]).unwrap().unwrap();

    // Ordinary OVK recovery finds these self-payments as outgoing. Swap decryption
    // must replace those records with incoming records, not duplicate them.
    let ufvks = st.wallet().get_unified_full_viewing_keys().unwrap();
    let decoded = zcash_client_backend::decrypt_transaction(&network, None, Some(h), &tx, &ufvks)
        .with_swap_receiving_keys(st.wallet().get_swap_scanning_keys().unwrap());
    for key in &keys {
        let outputs: Vec<_> = decoded
            .ironwood_outputs()
            .iter()
            .filter(|o| o.note().0.recipient() == key.receiver())
            .collect();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].swap_key_id(), Some(key.key_id()));
        assert_eq!(
            outputs[0].transfer_type(),
            zcash_client_backend::TransferType::Incoming
        );
        assert_eq!(outputs[0].memo(), &memo);
    }
    if full_first {
        decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, None).unwrap();
    }
    let (mined, _) = st.generate_next_block_including(created[0]);
    st.scan_cached_blocks(mined, 1);
    // Close/reopen before fetching the full transaction, as on a resumed sync.
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        network,
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    for _ in 0..2 {
        decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, Some(mined)).unwrap();
    }
    st.scan_cached_blocks(mined, 1);
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account.id(), mined)
        .unwrap();
    for key in keys {
        let note = notes
            .iter()
            .find(|n| n.swap_key_id() == Some(key.key_id()))
            .unwrap();
        assert_eq!(note.note().recipient(), key.receiver());
        let stored: Vec<u8> = st.wallet().conn().query_row(
            "SELECT rn.memo FROM ironwood_received_notes rn JOIN transactions t ON t.id_tx = rn.transaction_id
             WHERE t.txid = ?1 AND rn.action_index = ?2",
            rusqlite::params![tx.txid().as_ref(), note.output_index()], |r| r.get(0)).unwrap();
        assert_eq!(stored, memo.as_slice());
    }
    assert_eq!(notes.len(), 3); // Both swap payments and ordinary internal change.
}

#[test]
fn swap_receiving_full_transaction_after_compact_scan() {
    full_transaction_roundtrip(false);
}

#[test]
fn swap_receiving_full_transaction_before_compact_scan() {
    full_transaction_roundtrip(true);
}

#[test]
fn refund_funding_memo_recovers_from_seed_with_zero_change() {
    use zakura_swap_receiving::RefundMemo;
    let activation = BlockHeight::from_u32(100_000);
    let network = LocalNetwork {
        nu6: Some(activation),
        nu6_1: Some(activation),
        nu6_2: Some(activation),
        nu6_3: Some(activation),
        ..TestBuilder::<(), ()>::DEFAULT_NETWORK
    };
    let mut st = TestBuilder::new()
        .with_network(network)
        .with_data_store_factory(TestDbFactory::file_backed())
        .with_block_cache(crate::testing::BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    let account = st.test_account().cloned().unwrap();
    let parent = orchard::keys::FullViewingKey::from(account.usk().orchard());
    let (first, _, _) = st.generate_next_block(
        &IronwoodFvk(parent),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(1_000_000),
    );
    st.scan_cached_blocks(first, 1);
    for _ in 0..5 {
        let (h, _) = st.generate_empty_block();
        st.scan_cached_blocks(h, 1);
    }
    let receiver = orchard::keys::FullViewingKey::from(
        &orchard::keys::SpendingKey::from_bytes([0xf5; 32]).unwrap(),
    )
    .address_at(0u32, Scope::External);
    let address =
        Address::Unified(UnifiedAddress::from_receivers(Some(receiver), None, None).unwrap())
            .to_zcash_address(&network);
    let memo = RefundMemo::new(network.network_type(), 7, &address.to_string()).unwrap();
    let memo_bytes = MemoBytes::from_bytes(&memo.encode()).unwrap();
    let strategy = standard::SingleOutputChangeStrategy::<TestDb>::new(
        StandardFeeRule::Zip317,
        Some(memo_bytes.clone()),
        ShieldedPool::Ironwood,
        DustOutputPolicy::default(),
    );
    let proposal = st
        .propose_transfer(
            account.id(),
            &GreedyInputSelector::new(),
            &strategy,
            TransactionRequest::new(vec![Payment::without_memo(
                address,
                Zatoshis::const_from_u64(990_000),
            )])
            .unwrap(),
            ConfirmationsPolicy::MIN,
        )
        .unwrap();
    let change = proposal.steps()[0].balance().proposed_change();
    assert_eq!(change.len(), 1);
    assert_eq!(change[0].value(), Zatoshis::ZERO);
    assert_eq!(change[0].memo(), Some(&memo_bytes));
    let created = st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();
    let tx = st.wallet().get_transaction(created[0]).unwrap().unwrap();
    let (mined, _) = st.generate_next_block_including(created[0]);
    st.scan_cached_blocks(mined, 1);

    let refund_fvk = KeyId::new(Purpose::Refund, 7)
        .derive(&orchard::keys::FullViewingKey::from(
            account.usk().orchard(),
        ))
        .unwrap();
    let (refunded, _, _) = st.generate_next_block(
        &IronwoodFvk(refund_fvk),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(980_000),
    );

    // Replace the wallet with a seed restore. No reservations or sent-transaction
    // records survive, so recovery must authenticate chain inputs and the memo.
    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    let _old_wallet = st.reset();
    let (restored, _) = st
        .wallet_mut()
        .create_account("restored", &seed, account.birthday(), None)
        .unwrap();
    st.wallet_mut().update_chain_tip(refunded).unwrap();
    st.scan_cached_blocks(first, (u32::from(refunded) - u32::from(first) + 1) as usize);
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
    decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, Some(mined)).unwrap();
    let records = st
        .wallet_mut()
        .db_mut()
        .recover_swap_refund_memos(restored)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].index, 7);
    assert_eq!(records[0].deposit_address, memo.deposit_address());
    let keys = st.wallet().db().get_swap_receiving_keys(restored).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_id(), KeyId::new(Purpose::Refund, 7));
    assert_eq!(keys[0].scan_from(), mined);
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap(),
        records
    );

    // The first pass already crossed the refund without its key. Registration
    // queues that missing history, and replay makes the note reconstructible.
    st.scan_cached_blocks(mined, 2);
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(restored, refunded)
        .unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note.swap_key_id() == Some(KeyId::new(Purpose::Refund, 7)))
    );

    // Restoration rebuilds provider polling without a local swap activity record.
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(restored)
        .unwrap();
    let key = KeyId::new(Purpose::Refund, 7);
    let initial = zakura_swap_receiving::lifecycle::ChainAnchor {
        height: refunded,
        hash: st
            .wallet()
            .db()
            .get_block_hash(refunded)
            .unwrap()
            .unwrap()
            .0,
    };
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(restored, key, initial)
            .unwrap(),
        Some(initial)
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_000)
            .unwrap(),
        vec![(key, memo.deposit_address().to_owned())]
    );
    let directory = tempfile::tempdir().unwrap();
    let restored_path = directory.path().join("restored.sqlite");
    st.wallet()
        .conn()
        .execute("VACUUM INTO ?1", [restored_path.to_str().unwrap()])
        .unwrap();
    let reopened = WalletDb::for_path(&restored_path, *st.network(), test_clock(), test_rng())
        .unwrap()
        .with_transparent_ledger_mode(
            zcash_client_backend::data_api::transparent_ledger::TransparentLedgerMode::Public,
        );
    *st.wallet_mut().db_mut() = reopened;
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_059)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_060)
            .unwrap()
            .len(),
        1
    );
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(restored, key, initial)
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(restored, key, initial)
            .unwrap(),
        None
    );
    st.truncate_to_height_retaining_cache(refunded);
    let (late, _, _) = st.generate_next_block(
        &IronwoodFvk(keys[0].full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(50_000),
    );
    st.scan_cached_blocks(late, 1);
    assert_eq!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(restored, late)
            .unwrap()
            .iter()
            .filter(|n| n.swap_key_id() == Some(key))
            .count(),
        2
    );
    st.wallet_mut()
        .db_mut()
        .observe_swap_operation(restored, key, memo.deposit_address(), true, late)
        .unwrap();
    // Repeated memo recovery cannot reopen terminal status or move its deadline.
    st.wallet_mut()
        .db_mut()
        .recover_swap_refund_memos(restored)
        .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 2_000)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        st.wallet()
            .db()
            .get_swap_scan_window(late + 10)
            .unwrap()
            .0
            .len(),
        1
    );
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(late + 11)
            .unwrap()
            .0
            .is_empty()
    );

    // Terminal status before catch-up cannot skip the unscanned grace blocks.
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(restored, key, initial)
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(restored, key, initial)
            .unwrap(),
        None
    );
    for _ in 0..10 {
        st.generate_empty_block();
    }
    st.scan_cached_blocks(late + 1, 10);
    let closed = zakura_swap_receiving::lifecycle::ChainAnchor {
        height: late + 10,
        hash: st
            .wallet()
            .db()
            .get_block_hash(late + 10)
            .unwrap()
            .unwrap()
            .0,
    };
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_recovery_target(restored, key, closed)
            .unwrap(),
        Some(closed)
    );
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(restored, key, closed)
        .unwrap();
    st.truncate_to_height_retaining_cache(late + 9);
    assert_eq!(
        st.wallet()
            .db()
            .swap_recovery_target(restored, key)
            .unwrap(),
        None
    );
    assert!(
        !st.wallet()
            .db()
            .get_swap_scan_window(late + 10)
            .unwrap()
            .0
            .is_empty()
    );

    // Losing own-send evidence, or changing the authenticated scope, must make
    // the same memo ineligible. Restoring the evidence lets a later pass retry.
    let conn = st.wallet().conn();
    conn.execute(
        "UPDATE ironwood_received_notes SET recipient_key_scope = 0 WHERE memo = ?1",
        [memo_bytes.as_slice()],
    )
    .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_received_notes SET recipient_key_scope = 1 WHERE memo = ?1",
            [memo_bytes.as_slice()],
        )
        .unwrap();
    st.wallet()
        .conn()
        .execute("DELETE FROM ironwood_received_note_spends", [])
        .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
}
