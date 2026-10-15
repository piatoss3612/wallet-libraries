use super::*;
use zcash_client_backend::data_api::one_time_ivk::restore::{EarlyRestoreKey, PublicationEnd};

/// Receive key `index`.
fn receive(index: u64) -> KeyId {
    KeyId::new(Purpose::Receive, index)
}

/// What the filters say about receive keys 0 to 8, of which `paid` is paid.
fn filtered(paid: u64) -> Vec<EarlyRestoreKey> {
    (0..=8)
        .map(|index| EarlyRestoreKey {
            key: receive(index),
            seen: false,
            paid: index == paid,
            recent: false,
        })
        .collect()
}

/// The decoded cached block at `height`.
fn cached(st: &State, height: BlockHeight) -> CompactBlock {
    let sql = "SELECT data FROM compactblocks WHERE height = ?1";
    let data: Vec<u8> = value(&st.cache().0, sql, [u32::from(height)]);
    CompactBlock::decode(data.as_slice()).unwrap()
}

/// The publication end at cached block `height`, as its manifest declares it.
fn end_at(st: &State, height: BlockHeight) -> PublicationEnd {
    let block = cached(st, height);
    PublicationEnd {
        height,
        hash: block.hash().0,
        tree_size: block
            .chain_metadata
            .unwrap()
            .ironwood_commitment_tree_size
            .into(),
    }
}

/// The sibling hashes of the Ironwood leaf at `position` among the cached blocks
/// through `height`, as a publication ending there serves them.
fn siblings_at(st: &State, position: u32, height: BlockHeight) -> [[u8; 32]; 32] {
    use incrementalmerkletree::{frontier::CommitmentTree, witness::IncrementalWitness};
    let mut tree = CommitmentTree::<MerkleHashOrchard, 32>::empty();
    let mut witness: Option<IncrementalWitness<_, 32>> = None;
    let sql = "SELECT data FROM compactblocks WHERE height <= ?1 ORDER BY height";
    let blocks: Vec<Vec<u8>> = column(&st.cache().0, sql, [u32::from(height)]);
    let actions = blocks
        .iter()
        .flat_map(|data| CompactBlock::decode(data.as_slice()).unwrap().vtx)
        .flat_map(|tx| tx.ironwood_actions);
    for (leaf, action) in actions.enumerate() {
        let cmx = MerkleHashOrchard::from_cmx(&action.cmx().unwrap());
        match &mut witness {
            Some(witness) => witness.append(cmx).unwrap(),
            None => {
                tree.append(cmx).unwrap();
                if leaf == position as usize {
                    witness = IncrementalWitness::from_tree(tree.clone());
                }
            }
        }
    }
    let path: MerklePath = witness.unwrap().path().unwrap().into();
    path.auth_path().map(|hash| hash.to_bytes())
}

/// Retains spend history in a new [`ironwood_wallet`], then mines a block paying receive
/// key 8 and two empty blocks, scanning none of them. Returns the wallet, the payment as
/// a candidate, and the first and last heights.
fn unscanned_restore() -> (State, PendingPayment, BlockHeight, BlockHeight) {
    let mut st = ironwood_wallet();
    let account = st.test_account().unwrap().id();
    retain(st.wallet_mut().db_mut(), account);
    let fvk = derive(&st, receive(8));
    let recipient = IronwoodFvk(fvk.clone());
    let value = Zatoshis::const_from_u64(100_000);
    let (paid, _, _) = st.generate_next_block(&recipient, AddressType::DefaultExternal, value);
    st.generate_empty_block();
    let (last, _) = st.generate_empty_block();
    let candidate = candidate_at(&st, paid, &fvk);
    (st, candidate, paid, last)
}

/// The early restore state of the test account.
fn restore_state(st: &State) -> u8 {
    let sql = "SELECT state FROM ironwood_one_time_restores";
    value(st.wallet().conn(), sql, [])
}

/// Key `key`'s sweep row: its finished height and whether it is provisional.
fn sweep_row(st: &State, key: KeyId) -> (Option<u32>, bool) {
    let sql = "SELECT s.done_height, s.provisional FROM ironwood_one_time_sweeps s
               JOIN ironwood_one_time_keys k ON k.id = s.one_time_key_id
               WHERE k.purpose = ?1 AND k.key_index = ?2";
    let purpose = u8::from(key.purpose() == Purpose::Receive);
    row(
        st.wallet().conn(),
        sql,
        rusqlite::params![purpose, key.index().to_be_bytes()],
    )
}

/// Whether key `key` is closed.
fn closed(st: &State, key: KeyId) -> bool {
    let sql = "SELECT closed_at IS NOT NULL FROM ironwood_one_time_keys
               WHERE purpose = 1 AND key_index = ?1";
    value(st.wallet().conn(), sql, [key.index().to_be_bytes()])
}

/// The queued early payments: their heights and whether each is a scan hint.
fn queued(st: &State) -> Vec<(u32, bool)> {
    let sql = "SELECT height, scan_hint FROM ironwood_one_time_payment_recovery ORDER BY height";
    rows(st.wallet().conn(), sql, [])
}

/// Keys the scanner tries on a batch covering `range`.
fn keys_for(st: &State, range: std::ops::Range<BlockHeight>) -> Vec<KeyId> {
    let keys = st
        .wallet()
        .get_one_time_scanning_keys_for_range(range)
        .unwrap();
    keys.iter().map(|k| k.key_id()).collect()
}

/// A payment in a block the scan has yet to reach is a scan hint: only that batch tries
/// its key, the scan finds the note there, and the key's restore finishes at the anchor
/// and closes once the scan accepts the publication.
#[test]
fn a_hinted_payment_is_found_by_the_scan_and_settles_its_key() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let db = st.wallet_mut().db_mut();
    assert_eq!(
        db.one_time_early_restore_start(account).unwrap(),
        Some(paid)
    );
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    let answer = [directory_answer(&candidate)];
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| None)
        .unwrap();
    assert_eq!(queued(&st), [(u32::from(paid), true)]);
    // Only the batch holding the payment tries the key.
    assert_eq!(keys_for(&st, paid..paid + 1), [receive(8)]);
    assert!(keys_for(&st, paid + 1..last + 1).is_empty());
    assert!(scanning_keys(&st).is_empty());

    st.scan_cached_blocks_with_one_time_ivks(paid, 1);
    assert_eq!(unspent_keys(&st, paid), [Some(receive(8))]);
    assert!(queued(&st).is_empty());
    // Done at the anchor, but provisional until the scan stores it.
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), true));
    assert!(!closed(&st, receive(8)));

    st.scan_cached_blocks_with_one_time_ivks(paid + 1, 2);
    assert_eq!(restore_state(&st), 1);
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), false));
    assert!(closed(&st, receive(8)) && closed(&st, receive(0)));
    assert_eq!(unspent_keys(&st, last), [Some(receive(8))]);
    let db = st.wallet().db();
    assert_eq!(db.one_time_early_restore_start(account).unwrap(), None);
    assert!(!db.one_time_history_pending(account, last).unwrap());
}

/// A payment in a block the scan already passed is queued with its witness and inserted
/// once the publication is accepted and every block from the payment on carries spend
/// evidence.
#[test]
fn a_payment_in_a_scanned_block_is_inserted_with_its_witness() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    // The scan passes the payment before its lookup comes in.
    st.scan_cached_blocks_with_one_time_ivks(paid, 3);
    assert!(unspent_keys(&st, last).is_empty());
    assert_eq!(restore_state(&st), 1);

    let siblings = siblings_at(&st, candidate.position, last);
    let answer = [directory_answer(&candidate)];
    let db = st.wallet_mut().db_mut();
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| Some(siblings))
        .unwrap();
    assert_eq!(unspent_keys(&st, last), [Some(receive(8))]);
    assert!(queued(&st).is_empty());
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), false));
    assert!(closed(&st, receive(8)));
}

/// A hint at the wrong block finds nothing there, so the key falls back to a restore
/// sweep, which finds the payment once the scan is done.
#[test]
fn a_hint_that_finds_nothing_falls_back_to_a_sweep() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let mut misplaced = directory_answer(&candidate);
    misplaced.height = u32::from(last);
    misplaced.block_hash = end.hash;
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    db.record_one_time_early_lookup(account, receive(8), &[misplaced], |_, _| None)
        .unwrap();
    for height in [paid, paid + 1, last] {
        st.scan_cached_blocks_with_one_time_ivks(height, 1);
    }
    assert!(unspent_keys(&st, last).is_empty());
    assert!(queued(&st).is_empty());
    assert_eq!(sweep_row(&st, receive(8)), (None, false));
    let db = st.wallet().db();
    assert!(db.one_time_history_pending(account, last).unwrap());
}

/// A publication whose last block differs from the wallet's is rejected: keys it
/// registered without a payment go, the rest fall back to restore sweeps, and the
/// incoming lookahead restarts from index zero.
#[test]
fn a_mismatched_publication_falls_back_to_sweeps() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let mut end = end_at(&st, last);
    end.tree_size += 1;
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    let answer = [directory_answer(&candidate)];
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| None)
        .unwrap();
    st.scan_cached_blocks_with_one_time_ivks(paid, 3);
    assert_eq!(restore_state(&st), 2);
    // The hint found the note before the anchor was judged, so its key stays.
    assert_eq!(unspent_keys(&st, last), [Some(receive(8))]);
    assert_eq!(sweep_row(&st, receive(8)), (None, false));
    let sql = "SELECT COUNT(*) FROM ironwood_one_time_keys k
               JOIN ironwood_one_time_sweeps s ON s.one_time_key_id = k.id
               WHERE k.purpose = 1 AND s.done_height IS NULL AND s.provisional = 0";
    let pending: u32 = value(st.wallet().conn(), sql, []);
    assert_eq!(pending, RECEIVE_GAP_LIMIT as u32);
    let db = st.wallet().db();
    assert_eq!(db.one_time_early_restore_start(account).unwrap(), None);
}

/// An early restore needs a restored account none of whose history is scanned, with an
/// anchor at or above its restore start.
#[test]
fn an_early_restore_needs_unscanned_history() {
    let (mut st, _, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let below = PublicationEnd {
        height: paid - 1,
        ..end
    };
    let db = st.wallet_mut().db_mut();
    assert!(
        !db.begin_one_time_early_restore(account, below, &[])
            .unwrap()
    );
    assert_eq!(
        db.one_time_early_restore_start(account).unwrap(),
        Some(paid)
    );
    st.scan_cached_blocks_with_one_time_ivks(paid, 1);
    let db = st.wallet_mut().db_mut();
    assert_eq!(db.one_time_early_restore_start(account).unwrap(), None);
    assert!(!db.begin_one_time_early_restore(account, end, &[]).unwrap());

    let mut fresh = ironwood_wallet();
    let account = fresh.test_account().unwrap().id();
    let db = fresh.wallet_mut().db_mut();
    db.mark_one_time_account_fresh(account).unwrap();
    assert_eq!(db.one_time_early_restore_start(account).unwrap(), None);
}

/// A recent key scans from the block after the anchor, never the history before it, and
/// is not closed when the publication is accepted.
#[test]
fn a_recent_key_scans_only_after_the_anchor() {
    let (mut st, _, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let mut keys = filtered(u64::MAX);
    keys[2].recent = true;
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &keys)
            .unwrap()
    );
    assert!(keys_for(&st, paid..last + 1).is_empty());
    assert_eq!(keys_for(&st, paid..last + 2), [receive(2)]);
    st.scan_cached_blocks_with_one_time_ivks(paid, 3);
    assert_eq!(restore_state(&st), 1);
    assert!(!closed(&st, receive(2)) && closed(&st, receive(3)));
}

/// A hint whose batch the scanner stored without trying its key, because the lookup came
/// in after the scanner took its keys, becomes a payment to insert rather than a miss.
#[test]
fn a_late_hint_becomes_a_payment_to_insert() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let siblings = siblings_at(&st, candidate.position, last);
    let answer = [directory_answer(&candidate)];
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| Some(siblings))
        .unwrap();
    assert_eq!(queued(&st), [(u32::from(paid), true)]);
    // The batch's key snapshot predates the lookup.
    st.scan_cached_blocks(paid, 3);
    let db = st.wallet_mut().db_mut();
    let (mode, session) = (db.transparent_ledger_mode, db.enhance_pir_session.clone());
    tx(db, |c, p| {
        store::restore::early::after_blocks_stored(c, p, mode, &session, &[], paid..last + 1)
    })
    .unwrap();
    assert_eq!(restore_state(&st), 1);
    assert_eq!(unspent_keys(&st, last), [Some(receive(8))]);
    assert!(queued(&st).is_empty());
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), false));
}

/// Adds a second account from the test seed with the test account's birthday, which
/// rewinds the wallet to the block before it.
fn add_account_at_birthday(st: &mut State) {
    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    let birthday = st.test_account().unwrap().birthday().clone();
    let db = st.wallet_mut().db_mut();
    db.create_account("added", &seed, &birthday, None).unwrap();
}

/// Adding an account while another account's early restore waits for its anchor keeps
/// that restore: a payment queued for insertion becomes a hint again, keys done at the
/// anchor stay done, and the rescan finds the payment and accepts the publication.
#[test]
fn adding_an_account_keeps_an_early_restore_in_progress() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    // The scan passes the payment before its lookup comes in, short of the anchor.
    st.scan_cached_blocks_with_one_time_ivks(paid, 2);
    let siblings = siblings_at(&st, candidate.position, last);
    let answer = [directory_answer(&candidate)];
    let db = st.wallet_mut().db_mut();
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| Some(siblings))
        .unwrap();
    assert_eq!(queued(&st), [(u32::from(paid), false)]);

    add_account_at_birthday(&mut st);
    assert_eq!(restore_state(&st), 0);
    assert_eq!(queued(&st), [(u32::from(paid), true)]);
    assert_eq!(sweep_row(&st, receive(0)), (Some(u32::from(last)), true));
    assert_eq!(keys_for(&st, paid..paid + 1), [receive(8)]);

    st.scan_cached_blocks_with_one_time_ivks(paid, 3);
    assert_eq!(restore_state(&st), 1);
    assert_eq!(unspent_keys(&st, last), [Some(receive(8))]);
    assert!(queued(&st).is_empty());
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), false));
    assert!(closed(&st, receive(8)) && closed(&st, receive(0)));
    let db = st.wallet().db();
    assert!(!db.one_time_history_pending(account, last).unwrap());
}

/// A key whose hint already found its note, closed without scanning, would not find it
/// again after a rewind un-mines it, so that key alone falls back to a restore sweep.
#[test]
fn a_rewind_hands_a_found_payment_to_a_sweep() {
    let (mut st, candidate, paid, last) = unscanned_restore();
    let account = st.test_account().unwrap().id();
    let end = end_at(&st, last);
    let answer = [directory_answer(&candidate)];
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_one_time_early_restore(account, end, &filtered(8))
            .unwrap()
    );
    db.record_one_time_early_lookup(account, receive(8), &answer, |_, _| None)
        .unwrap();
    st.scan_cached_blocks_with_one_time_ivks(paid, 1);
    assert_eq!(sweep_row(&st, receive(8)), (Some(u32::from(last)), true));

    add_account_at_birthday(&mut st);
    assert_eq!(sweep_row(&st, receive(8)), (None, false));
    assert_eq!(sweep_row(&st, receive(0)), (Some(u32::from(last)), true));
    st.scan_cached_blocks_with_one_time_ivks(paid, 3);
    assert_eq!(restore_state(&st), 1);
    assert!(closed(&st, receive(0)));
    let db = st.wallet().db();
    assert!(db.one_time_history_pending(account, last).unwrap());
}

/// An account added to a wallet that already scanned its history restores early too.
/// Adding it requeues every block from its birthday, so a block the wallet keeps a row
/// for still counts as unscanned for it, and its payment there is found by a hint.
#[test]
fn an_added_account_restores_early_over_requeued_history() {
    let mut st = ironwood_wallet();
    let first = st.test_account().unwrap().id();
    retain(st.wallet_mut().db_mut(), first);
    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    let network = *st.network();
    let usk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
        &network,
        seed.expose_secret(),
        zip32::AccountId::try_from(1).unwrap(),
    )
    .unwrap();
    let fvk = receive(8)
        .derive(&FullViewingKey::from(usk.orchard()))
        .unwrap();
    let amount = Zatoshis::const_from_u64(100_000);
    let recipient = IronwoodFvk(fvk.clone());
    let (paid, _, _) = st.generate_next_block(&recipient, AddressType::DefaultExternal, amount);
    // Enough later blocks that adding the account keeps the payment's block row.
    let mut last = paid;
    for _ in 0..crate::PRUNING_DEPTH + 10 {
        last = st.generate_empty_block().0;
    }
    let blocks = (u32::from(last) - u32::from(paid) + 1) as usize;
    st.scan_cached_blocks_with_one_time_ivks(paid, blocks);
    let candidate = candidate_at(&st, paid, &fvk);
    let birthday = st.test_account().unwrap().birthday().clone();
    let db = st.wallet_mut().db_mut();
    let (added, _) = db.create_account("added", &seed, &birthday, None).unwrap();
    let sql = "SELECT EXISTS(SELECT 1 FROM blocks WHERE height = ?1)";
    assert!(value::<bool>(st.wallet().conn(), sql, [u32::from(paid)]));

    let end = end_at(&st, last);
    let answer = [directory_answer(&candidate)];
    let db = st.wallet_mut().db_mut();
    retain(db, added);
    assert_eq!(db.one_time_early_restore_start(added).unwrap(), Some(paid));
    assert!(
        db.begin_one_time_early_restore(added, end, &filtered(8))
            .unwrap()
    );
    db.record_one_time_early_lookup(added, receive(8), &answer, |_, _| None)
        .unwrap();
    assert_eq!(queued(&st), [(u32::from(paid), true)]);
    st.scan_cached_blocks_with_one_time_ivks(paid, blocks);
    assert_eq!(restore_state(&st), 1);
    let sql = "SELECT COUNT(*) FROM ironwood_received_notes n
               JOIN accounts a ON a.id = n.account_id
               WHERE a.uuid = ?1 AND n.one_time_key_id IS NOT NULL";
    assert_eq!(value::<u32>(st.wallet().conn(), sql, [added.0]), 1);
    assert!(queued(&st).is_empty());
}
