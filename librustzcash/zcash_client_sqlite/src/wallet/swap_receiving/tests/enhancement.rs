use super::*;
use std::convert::Infallible;
use transparent::address::TransparentAddress;
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
use zcash_protocol::{ShieldedPool, TxId, memo::MemoBytes, value::Zatoshis};
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
                .reserve_swap_receiving_key_from(account.id(), purpose, scan_from)
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

/// Builds a wallet whose test account holds one confirmed 1,000,000 zatoshi
/// Ironwood note, returning the height of the block that paid it.
fn ironwood_funded_wallet() -> (
    TestState<crate::testing::BlockCache, TestDb, LocalNetwork>,
    BlockHeight,
) {
    let activation = BlockHeight::from_u32(100_000);
    let mut st = TestBuilder::new()
        .with_network(LocalNetwork {
            nu6: Some(activation),
            nu6_1: Some(activation),
            nu6_2: Some(activation),
            nu6_3: Some(activation),
            ..TestBuilder::<(), ()>::DEFAULT_NETWORK
        })
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
    (st, first)
}

/// Sends `payments` from the test account with `memo` on Ironwood change, then
/// mines and scans the transaction. Returns its proposal, txid and height.
fn send_with_change_memo(
    st: &mut TestState<crate::testing::BlockCache, TestDb, LocalNetwork>,
    payments: Vec<Payment>,
    memo: &MemoBytes,
) -> (
    zcash_client_backend::proposal::Proposal<StandardFeeRule, crate::ReceivedNoteId>,
    TxId,
    BlockHeight,
) {
    let account = st.test_account().cloned().unwrap();
    let strategy = standard::SingleOutputChangeStrategy::<TestDb>::new(
        StandardFeeRule::Zip317,
        Some(memo.clone()),
        ShieldedPool::Ironwood,
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
    let (mined, _) = st.generate_next_block_including(created[0]);
    st.scan_cached_blocks(mined, 1);
    (proposal, created[0], mined)
}

#[test]
fn refund_funding_memo_recovers_from_seed_with_zero_change() {
    use zakura_swap_receiving::{
        RefundMemo,
        lifecycle::{ChainAnchor, ProviderStatus, near_observation},
    };
    let (mut st, first) = ironwood_funded_wallet();
    let network = *st.network();
    let account = st.test_account().cloned().unwrap();
    let deposit =
        Address::Transparent(TransparentAddress::PublicKeyHash([7; 20])).to_zcash_address(&network);
    let memo = RefundMemo::new(7);
    let memo_bytes = MemoBytes::from_bytes(&memo.encode()).unwrap();
    let (proposal, txid, mined) = send_with_change_memo(
        &mut st,
        vec![Payment::without_memo(
            deposit.clone(),
            Zatoshis::const_from_u64(985_000),
        )],
        &memo_bytes,
    );
    let change = proposal.steps()[0].balance().proposed_change();
    assert_eq!(change.len(), 1);
    assert_eq!(change[0].value(), Zatoshis::ZERO);
    assert_eq!(change[0].memo(), Some(&memo_bytes));
    let tx = st.wallet().get_transaction(txid).unwrap().unwrap();

    // Replace the wallet with a seed restore. No reservations or sent-transaction
    // records survive, so recovery must authenticate chain inputs and the memo.
    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    let _old_wallet = st.reset();
    let (restored, _) = st
        .wallet_mut()
        .create_account("restored", &seed, account.birthday(), None)
        .unwrap();
    st.wallet_mut().update_chain_tip(mined).unwrap();
    st.scan_cached_blocks(first, (u32::from(mined) - u32::from(first) + 1) as usize);
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
    decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, Some(mined)).unwrap();
    // A failed enclosing transaction must leave both the registration and memo
    // progress eligible for retry.
    let aborted: Result<(), Error> = st.wallet_mut().db_mut().transactionally(|db| {
        assert_eq!(db.recover_swap_refund_memos(restored)?.len(), 1);
        Err(corrupt("test rollback"))
    });
    assert!(aborted.is_err());
    assert!(
        st.wallet()
            .db()
            .get_swap_receiving_keys(restored)
            .unwrap()
            .is_empty()
    );
    let records = st
        .wallet_mut()
        .db_mut()
        .recover_swap_refund_memos(restored)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].index, 7);
    assert_eq!(records[0].deposit_address, Some(deposit.to_string()));
    let keys = st.wallet().db().get_swap_receiving_keys(restored).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_id(), KeyId::new(Purpose::Refund, 7));
    assert_eq!(keys[0].scan_from(), mined);
    // The wallet never scanned this key, so its history comes from a receiver-directory
    // sweep rather than trial decryption.
    assert!(
        st.wallet()
            .db()
            .get_swap_scanning_keys()
            .unwrap()
            .is_empty()
    );
    assert!(
        st.wallet()
            .db()
            .swap_history_pending(restored, mined)
            .unwrap()
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
    // Identical re-enhancement preserves completion rather than reviving work.
    decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, Some(mined)).unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );

    // Restoration rebuilds provider polling without a local swap activity record.
    let key = KeyId::new(Purpose::Refund, 7);
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_000, std::num::NonZeroU32::new(8).unwrap())
            .unwrap(),
        vec![(key, deposit.to_string())]
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
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_059, std::num::NonZeroU32::new(8).unwrap())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 1_060, std::num::NonZeroU32::new(8).unwrap())
            .unwrap()
            .len(),
        1
    );

    // A completed refund sweep scans the key from the next block.
    let swept = ChainAnchor {
        height: mined,
        hash: st.wallet().db().get_block_hash(mined).unwrap().unwrap().0,
    };
    st.wallet_mut()
        .db_mut()
        .finish_sweep(restored, key, swept)
        .unwrap();
    // `reset` forgot the cached tip that the next generated block extends.
    st.truncate_to_height_retaining_cache(mined);
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
        1
    );
    let refunded = near_observation(
        Purpose::Refund,
        &ProviderStatus {
            status: "REFUNDED",
            refunded_amount: Some(Zatoshis::const_from_u64(50_000)),
            ..Default::default()
        },
    )
    .unwrap();
    st.wallet_mut()
        .db_mut()
        .record_swap_observation(restored, key, &deposit.to_string(), refunded, 1_100)
        .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 2_000, std::num::NonZeroU32::new(8).unwrap())
            .unwrap()
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
    let spends: Vec<(i64, i64)> = st
        .wallet()
        .conn()
        .prepare(
            "SELECT ironwood_received_note_id,transaction_id FROM ironwood_received_note_spends",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
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

    for (note, transaction) in spends {
        st.wallet()
            .conn()
            .execute(
                "INSERT INTO ironwood_received_note_spends VALUES(?1,?2)",
                rusqlite::params![note, transaction],
            )
            .unwrap();
    }
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap(),
        records
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap()
            .is_empty()
    );

    // A progress record for a different funding height cannot suppress recovery.
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_swap_refund_memo_progress
        SET funding_height=funding_height+1",
            [],
        )
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap(),
        records
    );

    // Changing the memo invalidates only that note's progress. Unsupported data
    // stays unprocessed without failing recovery, and holds back refund issuance.
    let mut invalid = memo.encode();
    invalid[5] = 0xff;
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_received_notes SET memo=?1 WHERE memo=?2",
            rusqlite::params![invalid.as_slice(), memo_bytes.as_slice()],
        )
        .unwrap();
    let db = st.wallet_mut().db_mut();
    assert!(db.recover_swap_refund_memos(restored).unwrap().is_empty());
    assert!(db.swap_refund_memos_pending(restored).unwrap());
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_received_notes SET memo=?1 WHERE memo=?2",
            rusqlite::params![memo_bytes.as_slice(), invalid.as_slice()],
        )
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .recover_swap_refund_memos(restored)
            .unwrap(),
        records
    );
    // Reprocessed memos cannot reopen the provider's terminal status.
    assert!(
        st.wallet_mut()
            .db_mut()
            .take_swap_refund_status_checks(restored, 3_000, std::num::NonZeroU32::new(8).unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn funded_refund_quote_waits_for_its_outcome_and_abandoned_ones_do_not() {
    use zakura_swap_receiving::lifecycle::{
        CompletionPolicy, OperationStatus::Terminal, ReceiptExpectation,
    };
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let tip = st.wallet().chain_height().unwrap().unwrap();
    let now = unix_now(&test_clock());
    let key = st
        .wallet_mut()
        .db_mut()
        .reserve_swap_receiving_key_from(account, Purpose::Refund, tip + 1)
        .unwrap()
        .key_id();
    let deposit = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]));
    let funded = deposit.encode(&network);
    let abandoned =
        Address::Transparent(TransparentAddress::PublicKeyHash([8; 20])).encode(&network);
    let db = st.wallet_mut().db_mut();
    assert!(db.swap_funding_memo(account, key.index(), &funded).is_err());
    for deposit in [&abandoned, &funded] {
        db.record_swap_refund_quote(account, key.index(), deposit, now + 3_600, now)
            .unwrap();
    }
    let memo = db.swap_funding_memo(account, key.index(), &funded).unwrap();
    let (proposal, _, mined) = send_with_change_memo(
        &mut st,
        vec![Payment::without_memo(
            deposit.to_zcash_address(&network),
            Zatoshis::const_from_u64(100_000),
        )],
        &memo,
    );
    verify_swap_funding_proposal(&proposal, &memo, &funded).unwrap();
    assert!(verify_swap_funding_proposal(&proposal, &memo, &abandoned).is_err());

    let db = st.wallet_mut().db_mut();
    let records = db.recover_swap_refund_memos(account).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].index, key.index());
    assert!(db.recover_swap_refund_memos(account).unwrap().is_empty());
    assert!(!db.swap_history_pending(account, mined).unwrap());
    // Only restored refunds are polled by the library.
    assert!(
        db.take_swap_refund_status_checks(account, 0, std::num::NonZeroU32::new(8).unwrap())
            .unwrap()
            .is_empty()
    );
    let row = |conn: &Connection, deposit: &str| -> (i64, Option<i64>, u8) {
        conn.query_row(
            "SELECT observed_at, terminal_at, expectation FROM ironwood_swap_operations
                 WHERE operation_id = ?1",
            [deposit],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(row(&db.conn, &abandoned), (0, Some(now), 1));
    // The mined funding opens its quote until the provider reports an outcome.
    assert_eq!(row(&db.conn, &funded), (0, None, 0));
    let grace = CompletionPolicy::default().grace_secs;
    assert_eq!(
        db.close_finished_swap_keys(account, now + grace, mined)
            .unwrap(),
        0
    );
    let finished = now + 7_200;
    db.observe_swap_operation(
        account,
        key,
        &funded,
        Terminal(ReceiptExpectation::None),
        finished,
    )
    .unwrap();
    assert_eq!(row(&db.conn, &funded), (finished, Some(finished), 1));
    assert_eq!(
        db.close_finished_swap_keys(account, finished + grace - 1, mined)
            .unwrap(),
        0
    );
    assert_eq!(
        db.close_finished_swap_keys(account, finished + grace, mined)
            .unwrap(),
        1
    );
}

#[test]
fn reissued_refund_key_sweeps_history_before_its_scan_start() {
    use zakura_swap_receiving::RefundMemo;
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let memo = MemoBytes::from_bytes(&RefundMemo::new(0).encode()).unwrap();
    let deposit = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]));
    let (_, _, mined) = send_with_change_memo(
        &mut st,
        vec![Payment::without_memo(
            deposit.to_zcash_address(&network),
            Zatoshis::const_from_u64(100_000),
        )],
        &memo,
    );
    // A restored wallet can issue the memo's index again before recovering the memo.
    let db = st.wallet_mut().db_mut();
    let key = db
        .reserve_swap_receiving_key_from(account, Purpose::Refund, mined + 1)
        .unwrap();
    assert_eq!(key.key_id().index(), 0);
    assert_eq!(db.recover_swap_refund_memos(account).unwrap().len(), 1);
    assert!(db.swap_history_pending(account, mined).unwrap());
}

#[test]
fn refund_status_checks_skip_closed_keys() {
    use zakura_swap_receiving::{
        RefundMemo,
        lifecycle::{ChainAnchor, CompletionPolicy},
    };
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let memo = MemoBytes::from_bytes(&RefundMemo::new(7).encode()).unwrap();
    let deposit =
        Address::Transparent(TransparentAddress::PublicKeyHash([7; 20])).to_zcash_address(&network);
    let (_, _, mined) = send_with_change_memo(
        &mut st,
        vec![Payment::without_memo(
            deposit.clone(),
            Zatoshis::const_from_u64(100_000),
        )],
        &memo,
    );
    let swept = ChainAnchor {
        height: mined,
        hash: st.wallet().get_block_hash(mined).unwrap().unwrap().0,
    };
    let key = KeyId::new(Purpose::Refund, 7);
    let db = st.wallet_mut().db_mut();
    assert_eq!(db.recover_swap_refund_memos(account).unwrap().len(), 1);
    db.finish_sweep(account, key, swept).unwrap();
    // The provider never reports an outcome, so only the scanning limit closes the key.
    let limit = unix_now(&test_clock()) + CompletionPolicy::default().limit_secs;
    let batch = std::num::NonZeroU32::new(8).unwrap();
    assert_eq!(
        db.take_swap_refund_status_checks(account, limit, batch)
            .unwrap(),
        vec![(key, deposit.to_string())]
    );
    assert_eq!(
        db.close_finished_swap_keys(account, limit, mined).unwrap(),
        1
    );
    assert!(
        db.take_swap_refund_status_checks(account, limit + 60, batch)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn funding_without_one_transparent_output_restores_its_key_without_a_watch() {
    use zakura_swap_receiving::RefundMemo;
    let memo = MemoBytes::from_bytes(&RefundMemo::new(7).encode()).unwrap();
    let transparent = |byte| Address::Transparent(TransparentAddress::PublicKeyHash([byte; 20]));
    let receiver = orchard::keys::FullViewingKey::from(
        &orchard::keys::SpendingKey::from_bytes([0xf5; 32]).unwrap(),
    )
    .address_at(0u32, Scope::External);
    let shielded =
        Address::Unified(UnifiedAddress::from_receivers(Some(receiver), None, None).unwrap());
    let payment = |address: &Address, network: &LocalNetwork| {
        Payment::without_memo(
            address.to_zcash_address(network),
            Zatoshis::const_from_u64(100_000),
        )
    };

    for recipients in [vec![shielded], vec![transparent(7), transparent(8)]] {
        let (mut st, _) = ironwood_funded_wallet();
        let account = st.test_account().unwrap().id();
        let network = *st.network();
        let payments = recipients.iter().map(|a| payment(a, &network)).collect();
        send_with_change_memo(&mut st, payments, &memo);
        let db = st.wallet_mut().db_mut();
        let records = db.recover_swap_refund_memos(account).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].index, 7);
        assert_eq!(records[0].deposit_address, None);
        assert!(
            db.get_swap_receiving_key(account, KeyId::new(Purpose::Refund, 7))
                .unwrap()
                .is_some()
        );
        // Without a deposit address there is nothing to ask the provider about.
        assert!(
            db.take_swap_refund_status_checks(account, 0, std::num::NonZeroU32::new(8).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(!db.swap_refund_memos_pending(account).unwrap());
    }

    // A record whose raw transaction is not stored yet waits for it, without
    // failing recovery, then recovers the exact address it paid.
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let deposit = transparent(7);
    let (_, txid, _) = send_with_change_memo(&mut st, vec![payment(&deposit, &network)], &memo);
    let raw: Vec<u8> = st
        .wallet()
        .conn()
        .query_row(
            "SELECT raw FROM transactions WHERE txid=?1",
            [txid.as_ref()],
            |r| r.get(0),
        )
        .unwrap();
    st.wallet()
        .conn()
        .execute(
            "UPDATE transactions SET raw=NULL WHERE txid=?1",
            [txid.as_ref()],
        )
        .unwrap();
    let db = st.wallet_mut().db_mut();
    assert!(db.recover_swap_refund_memos(account).unwrap().is_empty());
    assert!(db.swap_refund_memos_pending(account).unwrap());
    st.wallet()
        .conn()
        .execute(
            "UPDATE transactions SET raw=?2 WHERE txid=?1",
            rusqlite::params![txid.as_ref(), raw],
        )
        .unwrap();
    let records = st
        .wallet_mut()
        .db_mut()
        .recover_swap_refund_memos(account)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].index, 7);
    assert_eq!(
        records[0].deposit_address,
        Some(deposit.to_zcash_address(&network).to_string())
    );
}

/// Enhance PIR's transparent flags are unauthenticated. Whatever they claim, a
/// privately retrieved refund record, even of an unsupported version, must wait
/// for its raw funding transaction.
#[test]
fn refund_memo_over_pir_waits_for_raw_funding_transaction() {
    use zakura_swap_receiving::RefundMemo;
    use zcash_client_backend::data_api::enhance_pir::{
        EnhancePirRead, EnhancePirWork, EnhancePirWrite, EnhanceRecord, EnhanceRecordParts,
        EnhanceTransactionMetadata, EnhancementMode, TransactionEnhancementWork,
    };

    for (version, flagged) in [(1, true), (1, false), (2, false)] {
        let (mut st, first) = ironwood_funded_wallet();
        let network = *st.network();
        let account = st.test_account().cloned().unwrap();
        let deposit = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]))
            .to_zcash_address(&network);
        let mut record = RefundMemo::new(7).encode();
        record[5] = version;
        let memo = MemoBytes::from_bytes(&record).unwrap();
        let (_, txid, mined) = send_with_change_memo(
            &mut st,
            vec![Payment::without_memo(
                deposit.clone(),
                Zatoshis::const_from_u64(985_000),
            )],
            &memo,
        );
        let tx = st.wallet().get_transaction(txid).unwrap().unwrap();
        let fee: u64 = st
            .wallet()
            .conn()
            .query_row(
                "SELECT fee FROM transactions WHERE txid=?1",
                [txid.as_ref()],
                |r| r.get(0),
            )
            .unwrap();

        // Restore privately. Compact blocks carry no transparent data.
        let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
        let _old_wallet = st.reset();
        let (restored, _) = st
            .wallet_mut()
            .create_account("restored", &seed, account.birthday(), None)
            .unwrap();
        st.wallet_mut()
            .db_mut()
            .set_enhancement_mode(EnhancementMode::PrivateIronwood);
        st.wallet_mut().update_chain_tip(mined).unwrap();
        st.scan_cached_blocks(first, (u32::from(mined) - u32::from(first) + 1) as usize);

        let requests: Vec<_> = st
            .wallet()
            .db()
            .transaction_enhancement_work()
            .unwrap()
            .into_iter()
            .filter_map(|work| match work {
                TransactionEnhancementWork::Private(EnhancePirWork::Query(r))
                    if r.request_id().txid() == txid =>
                {
                    Some(r)
                }
                _ => None,
            })
            .collect();
        assert!(!requests.is_empty());

        // Genuine ciphertexts; only the server's transparent flag varies.
        let bundle = tx.ironwood_bundle().unwrap();
        let metadata =
            EnhanceTransactionMetadata::new(u32::from(tx.expiry_height()), Some(fee)).unwrap();
        let batch: Vec<_> = requests
            .iter()
            .map(|r| {
                let action = &bundle.actions()[r.request_id().output_index() as usize];
                let note = action.encrypted_note();
                (
                    *r,
                    EnhanceRecord::from_parts(EnhanceRecordParts {
                        enc_ciphertext_suffix: note.enc_ciphertext[52..].try_into().unwrap(),
                        cv_net: action.cv_net().to_bytes(),
                        out_ciphertext: note.out_ciphertext,
                        has_transparent_inputs: false,
                        has_transparent_outputs: flagged,
                        metadata,
                    }),
                )
            })
            .collect();
        st.wallet_mut()
            .db_mut()
            .apply_ironwood_enhance_records(&batch)
            .unwrap();

        let memo_stored: bool = st
            .wallet()
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes n
                 JOIN transactions t ON t.id_tx = n.transaction_id
                 WHERE t.txid = ?1 AND n.memo IS NOT NULL)",
                [txid.as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!memo_stored, "version={version} flagged={flagged}");
        assert!(
            st.wallet()
                .db()
                .transaction_enhancement_work()
                .unwrap()
                .iter()
                .any(|w| matches!(w, TransactionEnhancementWork::Public(p) if p.txid() == txid)),
            "version={version} flagged={flagged}"
        );
        assert!(
            st.wallet_mut()
                .db_mut()
                .recover_swap_refund_memos(restored)
                .unwrap()
                .is_empty()
        );

        decrypt_and_store_transaction(&network, st.wallet_mut(), &tx, Some(mined)).unwrap();
        let db = st.wallet_mut().db_mut();
        let records = db.recover_swap_refund_memos(restored).unwrap();
        if version == 1 {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].deposit_address, Some(deposit.to_string()));
        } else {
            // A newer record cannot be read here. It may hold a refund index, so
            // refund issuance waits for an upgrade instead of failing sync.
            assert!(records.is_empty());
            assert!(matches!(
                db.reserve_swap_receiving_key(restored, Purpose::Refund, mined),
                Err(Error::ReservationPolicy(ReservationPolicy::Unreadable))
            ));
        }
    }
}

#[test]
fn missing_funding_memos_block_refund_issuance_and_settling() {
    use zakura_swap_receiving::lifecycle::CompletionPolicy;
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let tip = st.wallet().chain_height().unwrap().unwrap();
    let now = unix_now(&test_clock());
    let key = st
        .wallet_mut()
        .db_mut()
        .reserve_swap_receiving_key_from(account, Purpose::Refund, tip + 1)
        .unwrap()
        .key_id();
    let deposit = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]));
    let encoded = deposit.encode(&network);
    let db = st.wallet_mut().db_mut();
    db.record_swap_refund_quote(account, key.index(), &encoded, now + 3_600, now)
        .unwrap();
    let memo = db
        .swap_funding_memo(account, key.index(), &encoded)
        .unwrap();
    let (_, _, mined) = send_with_change_memo(
        &mut st,
        vec![Payment::without_memo(
            deposit.to_zcash_address(&network),
            Zatoshis::const_from_u64(100_000),
        )],
        &memo,
    );
    // Compact scanning stores the change before enhancement retrieves its memo.
    let (note, stored): (i64, Vec<u8>) = st
        .wallet()
        .conn()
        .query_row(
            "SELECT id, memo FROM ironwood_received_notes
             WHERE recipient_key_scope = 1 AND substr(memo, 1, 5) = X'FF5A535750'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let set_memo = |st: &mut TestState<_, TestDb, _>, memo: Option<&[u8]>| {
        st.wallet()
            .conn()
            .execute(
                "UPDATE ironwood_received_notes SET memo = ?2 WHERE id = ?1",
                rusqlite::params![note, memo],
            )
            .unwrap();
    };
    set_memo(&mut st, None);
    // Without its memo, the funded quote still looks abandoned once the grace passes.
    let settled = now + CompletionPolicy::default().grace_secs;
    let db = st.wallet_mut().db_mut();
    assert!(matches!(
        db.reserve_swap_receiving_key(account, Purpose::Refund, mined),
        Err(Error::ReservationPolicy(ReservationPolicy::Coverage))
    ));
    assert_eq!(
        db.close_finished_swap_keys(account, settled, mined)
            .unwrap(),
        0
    );
    set_memo(&mut st, Some(&stored));
    let db = st.wallet_mut().db_mut();
    assert_eq!(
        db.close_finished_swap_keys(account, settled, mined)
            .unwrap(),
        0
    );
    let funded: (i64, Option<i64>, u8) = db
        .conn
        .query_row(
            "SELECT observed_at, terminal_at, expectation FROM ironwood_swap_operations
             WHERE operation_id = ?1",
            [&encoded],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(funded, (0, None, 0));
    assert_eq!(
        db.reserve_swap_receiving_key(account, Purpose::Refund, mined)
            .unwrap()
            .key_id(),
        KeyId::new(Purpose::Refund, key.index() + 1)
    );
}

#[test]
fn refund_quote_needs_a_scanning_refund_key_and_a_canonical_transparent_deposit() {
    let (mut st, _) = ironwood_funded_wallet();
    let account = st.test_account().unwrap().id();
    let network = *st.network();
    let tip = st.wallet().chain_height().unwrap().unwrap();
    let now = unix_now(&test_clock());
    let p2pkh = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20])).encode(&network);
    let parent = FullViewingKey::from(st.test_account().unwrap().usk().orchard());
    let unified = Address::Unified(
        UnifiedAddress::from_receivers(Some(parent.address_at(0u32, Scope::External)), None, None)
            .unwrap(),
    )
    .encode(&network);
    let mainnet = Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]))
        .encode(&zcash_protocol::consensus::MainNetwork);
    let tex = Address::Tex([7; 20]).encode(&network);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.record_swap_refund_quote(account, 0, &p2pkh, now + 60, now)
            .is_err()
    );
    for _ in 0..2 {
        db.reserve_swap_receiving_key_from(account, Purpose::Refund, tip + 1)
            .unwrap();
    }
    let padded = format!(" {p2pkh}");
    for bad in [&unified, &tex, &mainnet, &padded] {
        assert!(
            db.record_swap_refund_quote(account, 0, bad, now + 60, now)
                .is_err()
        );
    }
    assert!(
        db.record_swap_refund_quote(account, 0, &p2pkh, now, now)
            .is_err()
    );
    for _ in 0..2 {
        db.record_swap_refund_quote(account, 0, &p2pkh, now + 60, now)
            .unwrap();
    }
    assert!(
        db.record_swap_refund_quote(account, 1, &p2pkh, now + 60, now)
            .is_err()
    );
    assert!(db.swap_funding_memo(account, 0, &p2pkh).is_ok());
    // A closed key would miss the refund.
    db.conn
        .execute(
            "UPDATE ironwood_receiving_keys SET closed_at = ?1 WHERE purpose = 0",
            [now],
        )
        .unwrap();
    assert!(db.swap_funding_memo(account, 0, &p2pkh).is_err());
}
