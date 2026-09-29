use super::*;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::data_api::{
    WalletRead,
    testing::{AddressType, IronwoodFvk},
};
use zcash_protocol::value::Zatoshis;

type State = TestState<crate::testing::BlockCache, TestDb, LocalNetwork>;
const NOW: i64 = 1_000_000;

pub(super) fn fixture() -> State {
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
    let ordinary = FullViewingKey::from(st.test_account().unwrap().usk().orchard());
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(ordinary),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(100_000),
    );
    st.scan_cached_blocks(h, 1);
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    st
}

fn prepare(st: &mut State, now: i64) -> ReceiveReservation {
    let account = st.test_account().unwrap().id();
    let r = st
        .wallet_mut()
        .db_mut()
        .prepare_swap_receive_reservation(account, now, BlockHeight::from_u32(100_000))
        .unwrap();
    let a = anchor(st);
    assert!(
        st.wallet_mut()
            .db_mut()
            .verify_swap_receive_history(account, r.id, a, &[])
            .unwrap()
    );
    r
}

fn quote(st: &mut State, r: &ReceiveReservation, request: &str, start: bool) {
    let account = st.test_account().unwrap().id();
    let db = st.wallet_mut().db_mut();
    db.begin_swap_receive_quote(account, r.id, request, NOW)
        .unwrap();
    db.record_swap_receive_quote(account, request, request, None, NOW + 60)
        .unwrap();
    if start {
        db.start_swap_receive_quote(account, request).unwrap();
    }
}

fn observe(st: &mut State, request: &str, status: &str, funded: bool, now: i64) {
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .observe_swap_receive_quote(account, request, status, funded, now)
        .unwrap();
}

fn pay(st: &mut State, r: &ReceiveReservation) {
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(r.key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(100_000),
    );
    st.scan_cached_blocks(h, 1);
}

pub(super) fn anchor(st: &State) -> ChainAnchor {
    let db = st.wallet().db();
    let height = db.block_fully_scanned().unwrap().unwrap().block_height();
    ChainAnchor {
        height,
        hash: db.get_block_hash(height).unwrap().unwrap().0,
    }
}

#[test]
fn reclamation_waits_until_latest_quote_deadline_plus_cooldown() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    quote(&mut st, &r, "first", false);
    let db = st.wallet_mut().db_mut();
    db.begin_swap_receive_quote(account, r.id, "second", NOW + 30)
        .unwrap();
    db.record_swap_receive_quote(account, "second", "second", None, NOW + 120)
        .unwrap();
    db.start_swap_receive_quote(account, "second").unwrap();

    let eligible_at = NOW + 120 + RECEIVE_RECLAIM_SECONDS;
    for request in ["first", "second"] {
        observe(&mut st, request, "PENDING_DEPOSIT", false, eligible_at - 1);
    }
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    assert!(
        db.swap_receive_reclaim_candidates(account, eligible_at - 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        !db.reclaim_swap_receive_reservation(account, r.id, eligible_at - 1, a)
            .unwrap()
    );
    assert_eq!(
        db.swap_receive_reclaim_candidates(account, eligible_at)
            .unwrap(),
        vec![r.id]
    );
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, eligible_at, a)
            .unwrap()
    );
}

#[test]
fn unquoted_draft_waits_until_creation_plus_cooldown() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    let eligible_at = NOW + RECEIVE_RECLAIM_SECONDS;
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    assert!(
        db.swap_receive_reclaim_candidates(account, eligible_at - 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        !db.reclaim_swap_receive_reservation(account, r.id, eligible_at - 1, a)
            .unwrap()
    );
    assert_eq!(
        db.swap_receive_reclaim_candidates(account, eligible_at)
            .unwrap(),
        vec![r.id]
    );
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, eligible_at, a)
            .unwrap()
    );
}

#[test]
fn fills_lowest_expired_hole_and_preserves_old_quotes() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let mut hole = None;
    for i in 0..5 {
        let r = prepare(&mut st, NOW);
        assert_eq!(r.key.key_id().index(), i);
        let request = format!("quote-{i}");
        quote(&mut st, &r, &request, true);
        if i == 1 {
            hole = Some(r);
        } else {
            pay(&mut st, &r);
            observe(&mut st, &request, "SUCCESS", true, NOW + 1);
        }
    }
    let r = hole.unwrap();
    let now = NOW + 61 + RECEIVE_RECLAIM_SECONDS;
    observe(&mut st, "quote-1", "PENDING_DEPOSIT", false, now);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    assert_eq!(
        db.swap_receive_reclaim_candidates(account, now).unwrap(),
        vec![r.id]
    );
    // Neither global scanning nor a recovery checkpoint is an empty-address check.
    db.conn
        .execute("DELETE FROM ironwood_swap_receive_checks", [])
        .unwrap();
    db.mark_swap_directory_checked(account, r.key.key_id(), a)
        .unwrap();
    assert!(
        !db.reclaim_swap_receive_reservation(account, r.id, now, a)
            .unwrap()
    );
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, now, a)
            .unwrap()
    );
    assert!(db.has_swap_receive_quote(account, "quote-1").unwrap());
    db.close_received_swap_reservations(account, now).unwrap();
    assert!(
        db.swap_receive_quotes_due(account, now + 31)
            .unwrap()
            .is_empty()
    );
    let recycled = prepare(&mut st, now);
    assert_eq!(recycled.key.key_id().index(), 1);
    assert_ne!(recycled.id, r.id);
    quote(&mut st, &recycled, "new-quote-1", true);
    assert_eq!(prepare(&mut st, now).key.key_id().index(), 5);
    // An old payment still decrypts under the same key after the reservation changes.
    pay(&mut st, &r);
    assert!(
        st.wallet()
            .db()
            .get_unspent_ironwood_notes_at_historical_height(account, anchor(&st).height)
            .unwrap()
            .iter()
            .any(|n| n.swap_key_id() == Some(r.key.key_id()))
    );
}

#[test]
fn restart_and_rejected_quote_reuse_the_same_draft() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    st.wallet_mut()
        .db_mut()
        .begin_swap_receive_quote(account, r.id, "too-low", NOW)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .reject_swap_receive_quote(account, "too-low")
        .unwrap();
    assert!(
        st.wallet()
            .db()
            .get_swap_scan_window(anchor(&st).height)
            .unwrap()
            .0
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
    let again = prepare(&mut st, NOW + 1);
    assert_eq!(again.id, r.id);
    assert_eq!(again.key.receiver(), r.key.receiver());
    quote(&mut st, &again, "accepted", true);
    assert_eq!(prepare(&mut st, NOW + 2).key.key_id().index(), 1);
}

#[test]
fn only_three_distinct_unfunded_reservations_are_allowed() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    for i in 0..RECEIVE_UNFUNDED_LIMIT {
        let r = prepare(&mut st, NOW);
        quote(&mut st, &r, &format!("quote-{i}"), true);
    }
    assert!(matches!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_receive_reservation(account, NOW, start()),
        Err(Error::ReservationPolicy(
            super::super::ReservationPolicy::Limit
        ))
    ));
    observe(&mut st, "quote-0", "PROCESSING", true, NOW + 1);
    assert_eq!(prepare(&mut st, NOW + 1).key.key_id().index(), 3);
}

#[test]
fn funded_deposits_without_zec_receipts_cannot_exceed_recovery_gap() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let mut first = None;
    for i in 0..RECEIVE_GAP_LIMIT {
        let r = prepare(&mut st, NOW);
        assert_eq!(r.key.key_id().index(), i);
        let request = format!("quote-{i}");
        quote(&mut st, &r, &request, true);
        observe(&mut st, &request, "PROCESSING", true, NOW + 1);
        if i == 0 {
            first = Some(r);
        }
    }
    assert!(matches!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_receive_reservation(account, NOW, start()),
        Err(Error::ReservationPolicy(
            super::super::ReservationPolicy::Gap
        ))
    ));
    pay(&mut st, &first.unwrap());
    assert_eq!(
        prepare(&mut st, NOW + 2).key.key_id().index(),
        RECEIVE_GAP_LIMIT
    );
}

#[test]
fn unknown_outcomes_and_stale_or_out_of_order_observations_do_not_release() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    quote(&mut st, &r, "accepted", false);
    st.wallet_mut()
        .db_mut()
        .begin_swap_receive_quote(account, r.id, "lost-response", NOW)
        .unwrap();
    let now = NOW + 61 + RECEIVE_RECLAIM_SECONDS;
    observe(&mut st, "accepted", "PENDING_DEPOSIT", false, now);
    assert!(
        st.wallet()
            .db()
            .swap_receive_reclaim_candidates(account, now)
            .unwrap()
            .is_empty()
    );
    st.wallet_mut()
        .db_mut()
        .reject_swap_receive_quote(account, "lost-response")
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .swap_receive_reclaim_candidates(account, now)
            .unwrap(),
        vec![r.id]
    );
    assert!(
        st.wallet()
            .db()
            .swap_receive_reclaim_candidates(account, now + 121)
            .unwrap()
            .is_empty()
    );
    observe(&mut st, "accepted", "PROCESSING", true, now + 122);
    observe(&mut st, "accepted", "PENDING_DEPOSIT", false, now + 121);
    assert!(
        st.wallet()
            .db()
            .swap_receive_reclaim_candidates(account, now + 122)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn late_payment_between_empty_check_and_reclamation_prevents_reuse() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    quote(&mut st, &r, "late", true);
    let now = NOW + 61 + RECEIVE_RECLAIM_SECONDS;
    observe(&mut st, "late", "PENDING_DEPOSIT", false, now);
    let a = anchor(&st);
    st.wallet_mut()
        .db_mut()
        .verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    pay(&mut st, &r);
    assert!(
        !st.wallet_mut()
            .db_mut()
            .reclaim_swap_receive_reservation(account, r.id, now, a)
            .unwrap()
    );
    assert_eq!(prepare(&mut st, now).key.key_id().index(), 1);
}

#[test]
fn reclamation_rejects_changed_block_anchor_and_stale_coverage() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = prepare(&mut st, NOW);
    quote(&mut st, &r, "abandoned", true);
    let now = NOW + 61 + RECEIVE_RECLAIM_SECONDS;
    observe(&mut st, "abandoned", "PENDING_DEPOSIT", false, now);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, now, ChainAnchor { hash: [0; 32], ..a })
            .is_err()
    );
    let (h, _) = st.generate_empty_block();
    st.scan_cached_blocks(h, 1);
    assert!(
        st.wallet_mut()
            .db_mut()
            .reclaim_swap_receive_reservation(account, r.id, now, a)
            .is_err()
    );
}

#[test]
fn reorg_retains_used_marker_and_rechecks_draft_recovery_bound() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let before_payment = anchor(&st).height;
    let first = prepare(&mut st, NOW);
    quote(&mut st, &first, "first", true);
    pay(&mut st, &first);
    for i in 1..RECEIVE_GAP_LIMIT {
        let r = prepare(&mut st, NOW);
        let request = format!("funded-{i}");
        quote(&mut st, &r, &request, true);
        observe(&mut st, &request, "PROCESSING", true, NOW + 1);
    }
    let draft = prepare(&mut st, NOW);
    assert_eq!(draft.key.key_id().index(), RECEIVE_GAP_LIMIT);
    st.truncate_to_height_retaining_cache(before_payment);
    assert!(matches!(
        st.wallet_mut()
            .db_mut()
            .prepare_swap_receive_reservation(account, NOW, start()),
        Err(Error::ReservationPolicy(
            super::super::ReservationPolicy::Gap
        ))
    ));
    observe(
        &mut st,
        "first",
        "FAILED",
        false,
        NOW + RECEIVE_RECLAIM_SECONDS + 61,
    );
    assert!(
        !st.wallet()
            .db()
            .swap_receive_reclaim_candidates(account, NOW + RECEIVE_RECLAIM_SECONDS + 61)
            .unwrap()
            .contains(&first.id)
    );
}
