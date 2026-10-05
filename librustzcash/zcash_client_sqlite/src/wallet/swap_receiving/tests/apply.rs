use super::*;
use incrementalmerkletree::Hashable;
use orchard::{
    note_encryption::{CompactAction, IronwoodDomain, IronwoodNoteEncryption},
    tree::{MerkleHashOrchard, MerklePath},
};
use prost::Message;
use std::collections::BTreeMap;
use zakura_swap_receiving::{lifecycle::ChainAnchor, recovery::EncryptedNote};
use zcash_client_backend::{
    data_api::{
        WalletRead,
        testing::{AddressType, IronwoodFvk},
    },
    proto::compact_formats::CompactBlock,
};
use zcash_note_encryption::{Domain, try_compact_note_decryption};
use zcash_protocol::value::Zatoshis;

pub(super) fn fixture() -> (
    TestState<crate::testing::BlockCache, TestDb, LocalNetwork>,
    RegisteredKey,
    PendingPayment,
    ChainAnchor,
    MerklePath,
) {
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
    let account = st.test_account().unwrap().id();
    let parent = FullViewingKey::from(st.test_account().unwrap().usk().orchard());
    let id = KeyId::new(Purpose::Receive, 8);
    let fvk = id.derive(&parent).unwrap();
    let (height, _, _) = st.generate_next_block(
        &IronwoodFvk(fvk.clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(100_000),
    );
    st.scan_cached_blocks(height, 1);
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, height)
            .unwrap()
            .is_empty()
    );
    let data: Vec<u8> = st
        .cache()
        .0
        .query_row(
            "SELECT data FROM compactblocks WHERE height=?1",
            [u32::from(height)],
            |r| r.get(0),
        )
        .unwrap();
    let block = CompactBlock::decode(data.as_slice()).unwrap();
    let tx = &block.vtx[0];
    let action = CompactAction::try_from(&tx.ironwood_actions[0]).unwrap();
    let (note, _) = try_compact_note_decryption(
        &IronwoodDomain::for_compact_action(&action),
        &fvk.to_ivk(Scope::External).prepare(),
        &action,
    )
    .unwrap();
    let encryption = IronwoodNoteEncryption::new(None, note, [4; 512]);
    let ciphertext = encryption.encrypt_note_plaintext();
    assert_eq!(
        &ciphertext[..52],
        tx.ironwood_actions[0].ciphertext.as_slice()
    );
    let key = st
        .wallet_mut()
        .db_mut()
        .watch_swap_receive_key(account, 8, height + 1)
        .unwrap();
    let candidate = PendingPayment {
        txid: tx.txid(),
        action_index: 0,
        height,
        block_hash: block.hash(),
        tx_index: tx.index.try_into().unwrap(),
        position: 0,
        encrypted_note: EncryptedNote::from_parts(
            note.rho().to_bytes(),
            orchard::note::ExtractedNoteCommitment::from(note.commitment()).to_bytes(),
            IronwoodDomain::epk_bytes(encryption.epk()).0,
            ciphertext[..52].try_into().unwrap(),
            ciphertext[52..].try_into().unwrap(),
        ),
    };
    let path = MerklePath::from_parts(
        0,
        std::array::from_fn(|level| MerkleHashOrchard::empty_root((level as u8).into())),
    );
    let through = ChainAnchor {
        height,
        hash: block.hash().0,
    };
    st.wallet_mut()
        .db_mut()
        .queue_swap_payment(account, id, &candidate)
        .unwrap();
    (st, key, candidate, through, path)
}

#[test]
fn swap_payment_applies_unscanned_key_note_atomically_and_reopens() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let mut bad_path = path.auth_path();
    bad_path[0] = MerkleHashOrchard::empty_root(1.into());
    assert!(
        st.wallet_mut()
            .db_mut()
            .apply_pending_swap_payment(
                account,
                key.key_id(),
                &candidate,
                through,
                Some((through, &MerklePath::from_parts(0, bad_path)))
            )
            .is_err()
    );
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, through.height)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        st.wallet()
            .db()
            .pending_swap_payments(account, key.key_id())
            .unwrap()
            .len(),
        1
    );
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
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account, through.height)
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note().value().inner(), 100_000);
    assert_eq!(notes[0].swap_key_id(), Some(key.key_id()));
    assert!(
        st.wallet()
            .db()
            .pending_swap_payments(account, key.key_id())
            .unwrap()
            .is_empty()
    );
    assert!(crate::wallet::enhance_pir::is_protected(st.wallet().conn(), candidate.txid).unwrap());
    let stored_memo: Vec<u8> = st
        .wallet()
        .conn()
        .query_row("SELECT memo FROM ironwood_received_notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored_memo, vec![4; 512]);
    // A duplicate directory answer does not double credit the note.
    st.wallet_mut()
        .db_mut()
        .queue_swap_payment(account, key.key_id(), &candidate)
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .apply_pending_swap_payment(account, key.key_id(), &candidate, through, None)
            .unwrap(),
        PaymentApplication::Applied
    );
    assert_eq!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, through.height)
            .unwrap()
            .len(),
        1
    );
    // Appending after importing a witness must preserve the evolving tree.
    let (later, _, _) = st.generate_next_block(
        &IronwoodFvk(key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(40_000),
    );
    st.scan_cached_blocks(later, 1);
    let tx = st.wallet_mut().conn_mut().transaction().unwrap();
    assert!(
        crate::ironwood_tree(&tx)
            .unwrap()
            .witness_at_checkpoint_id(0u64.into(), &later)
            .unwrap()
            .is_some()
    );
}

#[test]
fn swap_payment_incomplete_history_preserves_queue_and_balance() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet()
        .conn()
        .execute("DELETE FROM ironwood_nullifier_scan_blocks", [])
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
    assert_eq!(
        st.wallet()
            .db()
            .pending_swap_payments(account, key.key_id())
            .unwrap()
            .len(),
        1
    );
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, through.height)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn swap_payment_imports_an_already_spent_note_without_crediting_it() {
    use zcash_keys::address::{Address, UnifiedAddress};
    for pruned in [false, true] {
        let (mut st, key, candidate, _, _) = fixture();
        let account = st.test_account().unwrap().id();
        let recovered = candidate
            .encrypted_note
            .decrypt(
                &FullViewingKey::from(st.test_account().unwrap().usk().orchard()),
                key.key_id(),
            )
            .unwrap();
        let to = Address::Unified(
            UnifiedAddress::from_receivers(Some(key.receiver()), None, None).unwrap(),
        );
        let (height, _) = st.generate_next_block_spending(
            &IronwoodFvk(key.full_viewing_key().clone()),
            (*recovered.nullifier(), Zatoshis::const_from_u64(100_000)),
            to,
            Zatoshis::const_from_u64(20_000),
        );
        st.scan_cached_blocks(height, 1);
        let through = ChainAnchor {
            height,
            hash: st.wallet().get_block_hash(height).unwrap().unwrap().0,
        };
        let mut tree =
            incrementalmerkletree::frontier::CommitmentTree::<MerkleHashOrchard, 32>::empty();
        let mut witness = None;
        let mut stmt = st
            .cache()
            .0
            .prepare("SELECT data FROM compactblocks ORDER BY height")
            .unwrap();
        for block in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap() {
            let block = CompactBlock::decode(block.unwrap().as_slice()).unwrap();
            for tx in block.vtx {
                for action in tx.ironwood_actions {
                    let cmx = MerkleHashOrchard::from_cmx(&action.cmx().unwrap());
                    match &mut witness {
                        None => {
                            tree.append(cmx).unwrap();
                            witness = incrementalmerkletree::witness::IncrementalWitness::from_tree(
                                tree.clone(),
                            );
                        }
                        Some(witness) => witness.append(cmx).unwrap(),
                    }
                }
            }
        }
        drop(stmt);
        let proof: MerklePath = witness.unwrap().path().unwrap().into();
        if pruned {
            st.wallet_mut()
                .db_mut()
                .retain_swap_spend_history(account)
                .unwrap();
            st.wallet()
                .conn()
                .execute("DELETE FROM nullifier_map", [])
                .unwrap();
            st.wallet()
                .conn()
                .execute("DELETE FROM ironwood_nullifier_scan_blocks", [])
                .unwrap();
            assert_eq!(
                st.wallet_mut()
                    .db_mut()
                    .apply_pending_swap_payment(
                        account,
                        key.key_id(),
                        &candidate,
                        through,
                        Some((through, &proof))
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
            st.scan_cached_blocks(candidate.height, 2);
        }

        assert_eq!(
            st.wallet_mut()
                .db_mut()
                .apply_pending_swap_payment(
                    account,
                    key.key_id(),
                    &candidate,
                    through,
                    Some((through, &proof))
                )
                .unwrap(),
            PaymentApplication::Applied
        );
        let spent: u64 = st
            .wallet()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ironwood_received_notes n
        JOIN ironwood_received_note_spends s ON s.ironwood_received_note_id=n.id WHERE n.nf=?1",
                [recovered.nullifier().to_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spent, 1);
        assert!(
            st.wallet()
                .db()
                .get_unspent_ironwood_notes_at_historical_height(account, height)
                .unwrap()
                .iter()
                .all(|n| n.note().nullifier(key.full_viewing_key()) != *recovered.nullifier())
        );
    }
}

#[test]
fn privately_imported_note_spends_into_ordinary_internal_change() {
    use std::convert::Infallible;
    use zcash_client_backend::{
        data_api::wallet::{ConfirmationsPolicy, input_selection::GreedyInputSelector},
        fees::{DustOutputPolicy, StandardFeeRule, standard},
        wallet::OvkPolicy,
    };
    use zcash_keys::address::{Address, UnifiedAddress};
    use zcash_protocol::ShieldedPool;
    use zip321::{Payment, TransactionRequest};
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().cloned().unwrap();
    st.wallet_mut()
        .db_mut()
        .apply_pending_swap_payment(
            account.id(),
            key.key_id(),
            &candidate,
            through,
            Some((through, &path)),
        )
        .unwrap();
    for _ in 0..5 {
        let (h, _) = st.generate_empty_block();
        st.scan_cached_blocks(h, 1);
    }
    let recipient =
        FullViewingKey::from(&orchard::keys::SpendingKey::from_bytes([0xf5; 32]).unwrap())
            .address_at(0u32, Scope::External);
    let address =
        Address::Unified(UnifiedAddress::from_receivers(Some(recipient), None, None).unwrap());
    let request = TransactionRequest::new(vec![Payment::without_memo(
        address.to_zcash_address(st.network()),
        Zatoshis::const_from_u64(50_000),
    )])
    .unwrap();
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
            request,
            ConfirmationsPolicy::MIN,
        )
        .unwrap();
    assert_eq!(
        proposal.input_count_in_pool(zcash_protocol::PoolType::IRONWOOD),
        1
    );
    let created = st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();
    let (h, _) = st.generate_next_block_including(created[0]);
    st.scan_cached_blocks(h, 1);
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account.id(), h)
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].swap_key_id().is_none());
    assert_eq!(
        notes[0].note().recipient(),
        FullViewingKey::from(account.usk().orchard()).address_at(0u32, Scope::Internal)
    );
}

#[test]
fn private_payment_uses_its_witness_anchor_and_rewind_invalidates_directory_progress() {
    let (mut st, key, candidate, proof_anchor, path) = fixture();
    let account = st.test_account().unwrap().id();
    let (height, _, _) = st.generate_next_block(
        &IronwoodFvk(key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(10_000),
    );
    st.scan_cached_blocks(height, 1);
    let through = ChainAnchor {
        height,
        hash: st.wallet().db().get_block_hash(height).unwrap().unwrap().0,
    };
    // The newer block changed the root. The supplied path belongs to the older accepted checkpoint.
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .apply_pending_swap_payment(
                account,
                key.key_id(),
                &candidate,
                through,
                Some((proof_anchor, &path))
            )
            .unwrap(),
        PaymentApplication::Applied
    );
    let db = st.wallet_mut().db_mut();
    db.finish_sweep(account, key.key_id(), through).unwrap();
    assert!(!db.swap_history_pending(account, through.height).unwrap());
    st.truncate_to_height_retaining_cache(proof_anchor.height);
    let db = st.wallet().db();
    assert_eq!(
        db.swap_lookup_coverage(account, key.key_id()).unwrap(),
        None
    );
    assert!(db.swap_history_pending(account, through.height).unwrap());
    let notes = st
        .wallet()
        .db()
        .get_unspent_ironwood_notes_at_historical_height(account, proof_anchor.height)
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note().value().inner(), 100_000);
}

#[test]
fn sweep_steps_queue_a_directory_lookup_and_apply_it_once() {
    let (mut st, key, candidate, through, path) = fixture();
    let account = st.test_account().unwrap().id();
    let key = key.key_id();
    // Start from the directory's report alone, not a queued candidate.
    st.wallet()
        .conn()
        .execute("DELETE FROM ironwood_swap_payment_recovery", [])
        .unwrap();
    let note = candidate.encrypted_note.to_bytes();
    let payment = DirectoryPayment {
        height: u32::from(candidate.height),
        block_hash: candidate.block_hash.0,
        txid: *candidate.txid.as_ref(),
        tx_index: candidate.tx_index.into(),
        action_index: candidate.action_index,
        position: candidate.position.into(),
        action_nullifier: note[..32].try_into().unwrap(),
        cmx: note[32..64].try_into().unwrap(),
        ephemeral_key: note[64..96].try_into().unwrap(),
        ciphertext_prefix: note[96..148].try_into().unwrap(),
    };
    let suffix: [u8; 528] = note[148..].try_into().unwrap();
    let db = st.wallet_mut().db_mut();
    db.prepare_swap_discovery_batch(account, through, 0, std::num::NonZeroU32::new(8).unwrap())
        .unwrap();

    let deferred = |result: Result<_, Error>| match result {
        Err(Error::SweepDeferred(reason)) => reason,
        other => panic!("expected a deferral, got {:?}", other.map(|_| ())),
    };
    assert_eq!(
        deferred(db.swap_publication_anchor(through.height + 1, through)),
        SweepDeferral::UnknownAnchor
    );
    let far = ChainAnchor {
        height: through.height + MAX_PUBLICATION_LAG + 1,
        ..through
    };
    assert_eq!(
        deferred(db.swap_publication_anchor(through.height, far)),
        SweepDeferral::StalePublication
    );
    let anchor = db.swap_publication_anchor(through.height, through).unwrap();
    assert_eq!(anchor, through);

    assert_eq!(
        db.swap_note_data_needed(account, key, std::slice::from_ref(&payment))
            .unwrap(),
        [0]
    );
    let note_data = BTreeMap::from([(payment.position, suffix)]);
    assert!(
        db.queue_swap_directory_lookup(
            account,
            key,
            anchor,
            std::slice::from_ref(&payment),
            &BTreeMap::new()
        )
        .is_err()
    );
    db.queue_swap_directory_lookup(
        account,
        key,
        anchor,
        std::slice::from_ref(&payment),
        &note_data,
    )
    .unwrap();
    // Queued now, so a repeated report needs no note data, and a contradicting one fails.
    assert!(
        db.swap_note_data_needed(account, key, std::slice::from_ref(&payment))
            .unwrap()
            .is_empty()
    );
    let moved = DirectoryPayment {
        position: 1,
        ..payment.clone()
    };
    assert!(db.swap_note_data_needed(account, key, &[moved]).is_err());

    assert_eq!(
        db.apply_swap_sweep(account, key, through, anchor, |_, _| None)
            .unwrap(),
        PaymentApplication::AwaitingWitness
    );
    let siblings = path.auth_path().map(|hash| hash.to_bytes());
    assert_eq!(
        db.apply_swap_sweep(account, key, through, anchor, |position, cmx| {
            (position == candidate.position && cmx == candidate.encrypted_note.commitment())
                .then_some(siblings)
        })
        .unwrap(),
        PaymentApplication::Applied
    );
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, through.height)
            .unwrap()
            .iter()
            .any(|n| n.swap_key_id() == Some(key))
    );
    let db = st.wallet_mut().db_mut();
    assert!(!db.swap_history_pending(account, through.height).unwrap());
    assert!(
        db.swap_note_data_needed(account, key, std::slice::from_ref(&payment))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_publication_short_of_the_target_waits() {
    let (mut st, key, _, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet()
        .conn()
        .execute(
            "UPDATE ironwood_swap_sweeps SET target_height = ?1",
            [u32::from(through.height) + 1],
        )
        .unwrap();
    // The attempt still counts, so a lagging publication backs off.
    assert!(matches!(
        st.wallet_mut()
            .db_mut()
            .begin_swap_discovery_attempt(account, key.key_id(), through, 0),
        Err(Error::SweepDeferred(SweepDeferral::TargetNotReached))
    ));
    let attempts: u32 = st
        .wallet()
        .conn()
        .query_row("SELECT attempts FROM ironwood_swap_sweeps", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(matches!(
        st.wallet_mut().db_mut().queue_swap_directory_lookup(
            account,
            key.key_id(),
            through,
            &[],
            &BTreeMap::new()
        ),
        Err(Error::SweepDeferred(SweepDeferral::TargetNotReached))
    ));
}
