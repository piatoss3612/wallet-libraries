use super::*;
use zakura_one_time_ivk::Purpose;
use zcash_client_backend::data_api::one_time_ivk::restore::{DiscoveryStatus, LookupWork};

/// Receive key 8, which the fixtures pay before registering it.
pub(in crate::wallet::one_time_ivk) fn key8() -> KeyId {
    KeyId::new(Purpose::Receive, 8)
}

/// A filter check that marks `key` seen and paid.
pub(in crate::wallet::one_time_ivk) fn paid(key: KeyId) -> FilterCheck {
    FilterCheck {
        key,
        seen: true,
        paid: true,
        recent: false,
    }
}

/// The publication ending at the stored block `height`, as the directory declares it.
pub(in crate::wallet::one_time_ivk) fn publication_at(
    st: &State,
    height: BlockHeight,
) -> PublicationEnd {
    let (hash, tree_size): ([u8; 32], u64) = row(
        st.wallet().conn(),
        "SELECT hash, ironwood_commitment_tree_size FROM blocks WHERE height = ?1",
        [u32::from(height)],
    );
    PublicationEnd {
        height,
        hash,
        tree_size,
    }
}

/// Runs a whole discovery pass of `account` against `publication`: incoming `checks`, and
/// no refund key used. Returns whether the store took the publication.
pub(in crate::wallet::one_time_ivk) fn run_pass(
    db: &mut Db,
    account: AccountUuid,
    publication: PublicationEnd,
    checks: &[FilterCheck],
) -> bool {
    let done = EnumerationCursor {
        next: 100,
        reach: 100,
    };
    let now = clock_now();
    let took = db
        .record_one_time_filter_checks(
            account,
            publication,
            Purpose::Receive,
            checks,
            done,
            true,
            now,
        )
        .unwrap();
    took && db
        .record_one_time_filter_checks(account, publication, Purpose::Refund, &[], done, true, now)
        .unwrap()
}

/// `account`'s discovery status.
pub(in crate::wallet::one_time_ivk) fn status(db: &Db, account: AccountUuid) -> DiscoveryStatus {
    db.one_time_discovery(account, clock_now())
        .unwrap()
        .unwrap()
}

/// Records `payments` as `key`'s whole history read from `revision`, from page zero.
pub(in crate::wallet::one_time_ivk) fn look_up(
    db: &mut Db,
    account: AccountUuid,
    key: KeyId,
    revision: PublicationEnd,
    payments: &[PendingPayment],
) -> bool {
    let records: Vec<_> = payments.iter().map(directory_answer).collect();
    db.record_one_time_lookup(account, key, revision, 0, &records, true, clock_now())
        .unwrap()
}

/// The path of the chain's Ironwood leaf at `position` at the cached tip.
pub(in crate::wallet::one_time_ivk) fn witness_at(st: &State, position: u32) -> MerklePath {
    use incrementalmerkletree::{frontier::CommitmentTree, witness::IncrementalWitness};
    let mut tree = CommitmentTree::<MerkleHashOrchard, 32>::empty();
    let mut witness: Option<IncrementalWitness<_, 32>> = None;
    let blocks: Vec<Vec<u8>> = column(
        &st.cache().0,
        "SELECT data FROM compactblocks ORDER BY height",
        [],
    );
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
    witness.unwrap().path().unwrap().into()
}

/// `path`'s sibling hashes, as the directory serves them.
fn siblings(path: &MerklePath) -> [[u8; 32]; 32] {
    std::array::from_fn(|level| path.auth_path()[level].to_bytes())
}

/// Records the path of `key`'s late payment `candidate` at the scanned tip.
pub(in crate::wallet::one_time_ivk) fn send_witness(
    st: &mut State,
    key: KeyId,
    candidate: &PendingPayment,
) {
    let account = st.test_account().unwrap().id();
    let (anchor, path) = (tip(st), witness_at(st, candidate.position));
    let db = st.wallet_mut().db_mut();
    db.record_one_time_witness(account, key, candidate.position, anchor, siblings(&path))
        .unwrap();
}

/// Pays [`key8`] before the wallet knows it and scans past the payment, then runs a pass
/// that finds the key paid and records its lookup: the payment is a late candidate. Returns
/// the payment and the publication, which ends at the scanned tip.
pub(in crate::wallet::one_time_ivk) fn late_fixture() -> (State, PendingPayment, PublicationEnd) {
    late_fixture_in(ironwood_wallet())
}

/// [`late_fixture`] in `st`, an [`ironwood_wallet`] with no blocks.
pub(in crate::wallet::one_time_ivk) fn late_fixture_in(
    mut st: State,
) -> (State, PendingPayment, PublicationEnd) {
    let account = st.test_account().unwrap().id();
    let candidate = pay_candidate(&mut st, key8());
    st.generate_and_scan_empty_blocks_with_one_time_ivks(2);
    let publication = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    assert!(look_up(
        db,
        account,
        key8(),
        publication,
        &[candidate.clone()]
    ));
    (st, candidate, publication)
}

/// The note values the wallet holds for one-time keys, with whether each is spent.
fn one_time_notes(st: &State) -> Vec<(i64, bool)> {
    rows(
        st.wallet().conn(),
        "SELECT n.value, EXISTS(SELECT 1 FROM ironwood_received_note_spends s
             WHERE s.ironwood_received_note_id = n.id)
         FROM ironwood_received_notes n WHERE n.one_time_key_id IS NOT NULL ORDER BY n.id",
        [],
    )
}

/// A payment found after the scan passed its block waits for its inclusion path, then is
/// admitted unspent, once, and discovery completes.
#[test]
fn a_late_payment_is_admitted_once_its_path_arrives() {
    let (mut st, candidate, publication) = late_fixture();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    let found = status(db, account);
    assert_eq!(found.publication, Some(publication));
    assert!(found.complete);
    assert!(found.lookups.is_empty());
    assert_eq!(found.witnesses.len(), 1);
    assert_eq!(found.witnesses[0].position, candidate.position);
    assert!(one_time_notes(&st).is_empty());

    send_witness(&mut st, key8(), &candidate);
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
    let db = st.wallet_mut().db_mut();
    assert!(pending(db, account, key8()).is_empty());
    assert!(status(db, account).witnesses.is_empty());
    // The same answer again changes nothing.
    assert!(!look_up(db, account, key8(), publication, &[candidate]));
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
}

/// A late payment whose note was spent before discovery found it is admitted spent: the
/// spending transaction carries an output the scan recognized, so the journal holds the
/// note's nullifier.
#[test]
fn a_spent_late_payment_is_admitted_spent_through_the_journal() {
    let mut st = ironwood_wallet();
    let account = st.test_account().unwrap().id();
    let candidate = pay_candidate(&mut st, key8());
    let note = candidate
        .encrypted_note
        .decrypt(&parent(&st), key8())
        .unwrap();
    let spender = IronwoodFvk(parent(&st));
    let (height, _) = st.generate_next_block_spending(
        &spender,
        (*note.nullifier(), Zatoshis::const_from_u64(100_000)),
        unified(foreign_receiver()),
        Zatoshis::const_from_u64(20_000),
    );
    st.scan_cached_blocks_with_one_time_ivks(height, 1);
    st.generate_and_scan_empty_blocks_with_one_time_ivks(2);
    let publication = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    assert!(look_up(
        db,
        account,
        key8(),
        publication,
        &[candidate.clone()]
    ));
    send_witness(&mut st, key8(), &candidate);
    assert_eq!(one_time_notes(&st), [(100_000, true)]);
}

/// Issuance waits until a restored account's discovery is complete. A fresh account
/// needs none.
#[test]
fn issuance_waits_for_discovery() {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let publication = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    let issued = prepare_from(db, account, clock_now(), start(), None);
    assert_eq!(issued.unwrap().err(), Some(ReservationPolicy::Gap));
    assert!(!status(db, account).complete);
    assert!(run_pass(db, account, publication, &[]));
    assert!(status(db, account).complete);
    assert!(
        prepare_from(db, account, clock_now(), start(), None)
            .unwrap()
            .is_ok()
    );

    let mut fresh = restored_wallet();
    let account = fresh.test_account().unwrap().id();
    let db = fresh.wallet_mut().db_mut();
    db.mark_one_time_account_fresh(account).unwrap();
    assert!(
        db.one_time_discovery(account, clock_now())
            .unwrap()
            .is_none()
    );
    assert!(
        prepare_from(db, account, clock_now(), start(), None)
            .unwrap()
            .is_ok()
    );
}

/// A publication above the wallet's chain tip, or older than the current pass's, is
/// refused.
#[test]
fn stale_and_future_publications_are_refused() {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let current = publication_at(&st, tip(&st).height);
    let mut future = current;
    future.height = current.height + 10;
    let db = st.wallet_mut().db_mut();
    assert!(!run_pass(db, account, future, &[]));
    assert!(run_pass(db, account, current, &[]));
    st.generate_and_scan_empty_blocks_with_one_time_ivks(1);
    let newer = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, newer, &[]));
    assert!(!run_pass(db, account, current, &[]));
}

/// A rewind below a late payment returns it to a hint, which the rescan settles.
#[test]
fn a_rewind_returns_late_payments_to_hints() {
    let mut st = ironwood_wallet();
    st.generate_and_scan_empty_blocks_with_one_time_ivks(1);
    let (mut st, candidate, _) = late_fixture_in(st);
    st.truncate_to_height_retaining_cache(candidate.height - 1);
    let hint: bool = value(
        st.wallet().conn(),
        "SELECT scan_hint FROM ironwood_one_time_payment_recovery",
        [],
    );
    assert!(hint);
    let tip = st.latest_cached_block().unwrap().height();
    st.scan_cached_blocks_with_one_time_ivks(
        candidate.height,
        u32::from(tip - candidate.height) as usize + 1,
    );
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
}

/// A publication ending before the birthday completes discovery without lookups, still
/// protecting its paid keys from issuance.
#[test]
fn a_publication_before_the_birthday_needs_no_lookups() {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let birthday = st.test_account().unwrap().birthday().height();
    let publication = PublicationEnd {
        height: birthday - 1,
        hash: [3; 32],
        tree_size: 0,
    };
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    let found = status(db, account);
    assert!(found.complete);
    assert!(found.lookups.is_empty());
    assert!(key_state(db, key8()).1);
}

/// The publication ending at cached block `height`, before the wallet stores it.
fn cached_publication(st: &State, height: BlockHeight) -> PublicationEnd {
    let data: Vec<u8> = value(
        &st.cache().0,
        "SELECT data FROM compactblocks WHERE height = ?1",
        [u32::from(height)],
    );
    let block = CompactBlock::decode(data.as_slice()).unwrap();
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

/// Mines, without scanning, a block paying [`key8`] above a scanned tip, and runs a pass
/// whose publication ends there, recording the payment: a scan hint. Returns the payment.
fn hint_fixture() -> (State, PendingPayment) {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let fvk = derive(&st, key8());
    let amount = Zatoshis::const_from_u64(100_000);
    let paid_to = IronwoodFvk(fvk.clone());
    let (height, _, _) = st.generate_next_block(&paid_to, AddressType::DefaultExternal, amount);
    let candidate = candidate_at(&st, height, &fvk);
    st.wallet_mut().update_chain_tip(height).unwrap();
    let publication = cached_publication(&st, height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    assert!(look_up(
        db,
        account,
        key8(),
        publication,
        &[candidate.clone()]
    ));
    let hint: bool = value(
        st.wallet().conn(),
        "SELECT scan_hint FROM ironwood_one_time_payment_recovery",
        [],
    );
    assert!(hint);
    (st, candidate)
}

/// A payment whose block the scan has not stored is a hint: the batch holding it tries the
/// key, finds the note itself, judges the publication, and leaves nothing pending.
#[test]
fn a_hint_is_found_by_the_scan() {
    let (mut st, candidate) = hint_fixture();
    let account = st.test_account().unwrap().id();
    assert!(!status(st.wallet_mut().db_mut(), account).complete);
    st.scan_cached_blocks_with_one_time_ivks(candidate.height, 1);
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
    let db = st.wallet_mut().db_mut();
    assert!(pending(db, account, key8()).is_empty());
    assert!(status(db, account).complete);
}

/// A scan that did not try a hint's key, here a plain scan, leaves its payment late; the
/// late path then admits it.
#[test]
fn a_hint_a_scan_skips_becomes_late() {
    let (mut st, candidate) = hint_fixture();
    st.scan_cached_blocks(candidate.height, 1);
    assert!(one_time_notes(&st).is_empty());
    let late: bool = value(
        st.wallet().conn(),
        "SELECT scan_hint = 0 FROM ironwood_one_time_payment_recovery",
        [],
    );
    assert!(late);
    send_witness(&mut st, key8(), &candidate);
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
}

/// A publication whose last block differs from the wallet's is rejected: nothing is due
/// against it, and the account waits for a newer one. Its paid key's lookup stays due in
/// the next pass.
#[test]
fn a_publication_off_the_wallets_chain_is_rejected() {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let mut publication = publication_at(&st, tip(&st).height);
    publication.hash = [9; 32];
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    let found = status(db, account);
    assert!(!found.complete);
    assert!(found.lookups.is_empty());
    st.generate_and_scan_empty_blocks_with_one_time_ivks(1);
    let publication = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[]));
    let found = status(db, account);
    assert!(!found.complete);
    assert_eq!(found.lookups.len(), 1);
    assert!(look_up(db, account, key8(), publication, &[]));
    assert!(status(db, account).complete);
}

/// Paging resumes from the cursor and refuses a page that does not continue it.
#[test]
fn paging_resumes_from_its_cursor() {
    let mut st = ironwood_wallet();
    let account = st.test_account().unwrap().id();
    let first = pay_candidate(&mut st, key8());
    let fvk = derive(&st, key8());
    let height = pay(&mut st, &fvk, 50_000);
    let second = candidate_at(&st, height, &fvk);
    st.generate_and_scan_empty_blocks_with_one_time_ivks(1);
    let publication = publication_at(&st, tip(&st).height);
    // The scan did not know the key, so both payments are late.
    assert!(unspent_keys(&st, publication.height).is_empty());
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    let now = clock_now();
    let page = |p: &PendingPayment| [directory_answer(p)];
    let record = |db: &mut Db, from: u32, p: &PendingPayment, done: bool| {
        db.record_one_time_lookup(account, key8(), publication, from, &page(p), done, now)
            .unwrap()
    };
    assert!(record(db, 0, &first, false));
    let LookupWork { applied, last, .. } = status(db, account).lookups[0].clone();
    assert_eq!(applied, 1);
    assert_eq!(
        last.map(|(position, _)| position),
        Some(u64::from(first.position))
    );
    assert!(!record(db, 2, &second, true));
    assert!(record(db, 1, &second, true));
    let found = status(db, account);
    assert!(found.lookups.is_empty());
    assert_eq!(found.witnesses.len(), 2);
}

/// A fresh account needs no discovery until a key it issued closes without a payment;
/// then a newer publication should check that key, and issuance still waits for nothing.
#[test]
fn a_fresh_account_rechecks_keys_that_closed_unpaid() {
    let mut st = scanned_wallet();
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    assert!(
        db.one_time_discovery(account, clock_now())
            .unwrap()
            .is_none()
    );
    let key = reserve_key(db, account, Purpose::Receive, start());
    exec(
        &db.conn,
        "UPDATE ironwood_one_time_keys SET closed_at = 1 WHERE key_index = ?1",
        [key.key_id().index().to_be_bytes()],
    );
    let found = status(db, account);
    assert!(found.complete);
    assert!(found.refresh_due);
    // A pass that finds the key paid keeps discovery incomplete until its lookup is done,
    // while issuance still waits for nothing.
    let publication = publication_at(&st, st.latest_cached_block().unwrap().height());
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key.key_id())]));
    let found = status(db, account);
    assert!(!found.complete);
    assert_eq!(found.lookups.len(), 1);
    assert!(
        prepare_from(db, account, clock_now(), start(), None)
            .unwrap()
            .is_ok()
    );
    assert!(look_up(db, account, key.key_id(), publication, &[]));
    assert!(status(db, account).complete);
}

/// A late payment whose blocks the scan finished without journal coverage, as before the
/// journal existed, has those blocks scanned again, then is admitted.
#[test]
fn a_late_payment_over_uncovered_blocks_is_rescanned() {
    let (mut st, candidate, _) = late_fixture();
    exec(
        st.wallet().conn(),
        "DELETE FROM ironwood_journal_coverage",
        [],
    );
    send_witness(&mut st, key8(), &candidate);
    assert!(one_time_notes(&st).is_empty());
    let queued: Vec<(u32, u32)> = rows(
        st.wallet().conn(),
        "SELECT block_range_start, block_range_end FROM scan_queue WHERE priority != ?1",
        [crate::wallet::scanning::priority_code(
            &ScanPriority::Scanned,
        )],
    );
    assert_eq!(
        queued.first().map(|r| r.0),
        Some(u32::from(candidate.height))
    );
    let tip = st.latest_cached_block().unwrap().height();
    let blocks = u32::from(tip) - u32::from(candidate.height) + 1;
    st.scan_cached_blocks_with_one_time_ivks(candidate.height, blocks as usize);
    assert_eq!(one_time_notes(&st), [(100_000, false)]);
}

/// A lookup refuses a revision that ends below its cutoff, which cannot show the history
/// through it.
#[test]
fn a_lookup_refuses_a_revision_below_its_cutoff() {
    let mut st = restored_wallet();
    let account = st.test_account().unwrap().id();
    let older = publication_at(&st, tip(&st).height);
    st.generate_and_scan_empty_blocks_with_one_time_ivks(2);
    let publication = publication_at(&st, tip(&st).height);
    let db = st.wallet_mut().db_mut();
    assert!(run_pass(db, account, publication, &[paid(key8())]));
    assert!(!look_up(db, account, key8(), older, &[]));
    assert!(look_up(db, account, key8(), publication, &[]));
}
