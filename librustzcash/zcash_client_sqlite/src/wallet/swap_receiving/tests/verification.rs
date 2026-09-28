use super::reservations::{anchor, fixture};
use super::*;
use rusqlite::params;
use zakura_swap_receiving::lifecycle::ChainAnchor;
use zcash_client_backend::{
    data_api::{
        WalletRead,
        chain::BlockSource,
        testing::{AddressType, IronwoodFvk},
    },
    proto::compact_formats::CompactBlock,
};
use zcash_protocol::value::Zatoshis;
type State = TestState<crate::testing::BlockCache, TestDb, LocalNetwork>;

fn reserve(st: &mut State) -> ReceiveReservation {
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .prepare_swap_receive_reservation(account, 1_000_000, BlockHeight::from_u32(100_000))
        .unwrap()
}
fn blocks(st: &State, start: BlockHeight, count: usize) -> Vec<CompactBlock> {
    if count == 0 {
        return Vec::new();
    }
    let mut blocks = Vec::new();
    st.cache()
        .with_blocks::<_, crate::error::SqliteClientError>(Some(start), Some(count), |b| {
            blocks.push(b);
            Ok(())
        })
        .unwrap();
    blocks
}

#[test]
fn fresh_history_accepts_zero_to_five_blocks_but_not_six() {
    for count in [0, 1, 5, 6] {
        let mut st = fixture();
        let account = st.test_account().unwrap().id();
        let r = reserve(&mut st);
        let a = anchor(&st);
        st.generate_and_scan_empty_blocks(count);
        let tail = blocks(&st, a.height + 1, count);
        let db = st.wallet_mut().db_mut();
        if count > 5 {
            assert!(db.swap_receive_verification_tail(account, r.id, a).is_err());
            assert!(
                db.verify_swap_receive_history(account, r.id, a, &tail)
                    .is_err()
            );
        } else {
            assert_eq!(
                db.swap_receive_verification_tail(account, r.id, a)
                    .unwrap()
                    .is_some(),
                count > 0
            );
            assert!(
                db.verify_swap_receive_history(account, r.id, a, &tail)
                    .unwrap()
            );
            assert!(
                db.verified_swap_receive_reservation(account, r.id)
                    .unwrap()
                    .is_some()
            );
        }
    }
}

#[test]
fn verified_draft_survives_quotes_restart_and_continuous_scanning() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_swap_receive_quote(account, r.id, "unverified", 1_000_000)
            .is_err()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &[])
            .unwrap()
    );
    db.begin_swap_receive_quote(account, r.id, "first", 1_000_000)
        .unwrap();
    assert!(
        db.swap_directory_check(account, r.key.key_id())
            .unwrap()
            .is_none()
    );
    st.generate_and_scan_empty_blocks(7);
    let reopened = WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        test_clock(),
        test_rng(),
    )
    .unwrap();
    *st.wallet_mut().db_mut() = reopened;
    let current = anchor(&st);
    let db = st.wallet_mut().db_mut();
    assert_eq!(
        db.verified_swap_receive_reservation(account, r.id).unwrap(),
        Some(current)
    );
    db.begin_swap_receive_quote(account, r.id, "edit", 1_000_001)
        .unwrap();
    // An old publication is allowed for a verified draft, not for a fresh history lookup.
    assert!(db.swap_receive_verification_tail(account, r.id, a).is_err());
}

#[test]
fn missing_key_coverage_requires_the_tail_even_when_wallet_is_synced() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    st.wallet_mut()
        .db_mut()
        .verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    st.generate_and_scan_empty_blocks(3);
    let tail = blocks(&st, a.height + 1, 3);
    let db = st.wallet_mut().db_mut();
    assert_eq!(
        db.block_fully_scanned().unwrap().unwrap().block_height(),
        a.height + 3
    );
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.begin_swap_receive_quote(account, r.id, "hole", 1_000_000)
            .is_err()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &tail[..2])
            .is_err()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &tail)
            .unwrap()
    );
}

#[test]
fn existing_per_key_coverage_avoids_redownloading_tail() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    db.begin_swap_receive_quote(account, r.id, "active", 1_000_000)
        .unwrap();
    st.generate_and_scan_empty_blocks(5);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.swap_receive_verification_tail(account, r.id, a)
            .unwrap()
            .is_none()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &[])
            .unwrap()
    );
}

#[test]
fn payment_in_unwatched_tail_permanently_excludes_address_without_credit() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(r.key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(12345),
    );
    st.scan_cached_blocks(h, 1);
    let tail = blocks(&st, h, 1);
    let db = st.wallet_mut().db_mut();
    assert!(
        !db.verify_swap_receive_history(account, r.id, a, &tail)
            .unwrap()
    );
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.begin_swap_receive_quote(account, r.id, "paid", 1_000_000)
            .is_err()
    );
    assert_eq!(
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM ironwood_received_notes WHERE receiving_key_id IS NOT NULL",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(reserve(&mut st).key.key_id().index(), 1);
    st.truncate_to_height_retaining_cache(a.height);
    assert_eq!(reserve(&mut st).key.key_id().index(), 1);
}

#[test]
fn malformed_or_orphaned_tail_cannot_establish_absence() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    st.generate_and_scan_empty_blocks(1);
    let tail = blocks(&st, a.height + 1, 1);
    let db = st.wallet_mut().db_mut();
    for field in 0..4 {
        let mut bad = tail.clone();
        match field {
            0 => bad[0].hash[0] ^= 1,
            1 => bad[0].prev_hash[0] ^= 1,
            2 => bad[0].height += 1,
            _ => bad[0].chain_metadata = None,
        }
        assert!(
            db.verify_swap_receive_history(account, r.id, a, &bad)
                .is_err()
        );
    }
    assert!(
        db.verify_swap_receive_history(account, r.id, ChainAnchor { hash: [0; 32], ..a }, &tail)
            .is_err()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &tail)
            .unwrap()
    );
    st.truncate_to_height_retaining_cache(a.height);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.begin_swap_receive_quote(account, r.id, "rewound", 1_000_000)
            .is_err()
    );
}

#[test]
fn receipt_racing_quote_exposure_is_rechecked_atomically() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    db.begin_swap_receive_quote(account, r.id, "active", 1_000_000)
        .unwrap();
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(r.key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(12345),
    );
    st.scan_cached_blocks(h, 1);
    let db = st.wallet_mut().db_mut();
    assert!(
        db.begin_swap_receive_quote(account, r.id, "race", 1_000_001)
            .is_err()
    );
    assert_eq!(
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM ironwood_swap_receive_quotes WHERE request_id='race'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn an_interior_coverage_hole_blocks_a_verified_draft() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    db.begin_swap_receive_quote(account, r.id, "watch", 1_000_000)
        .unwrap();
    st.generate_and_scan_empty_blocks(5);
    let tail = blocks(&st, a.height + 1, 5);
    let db = st.wallet_mut().db_mut();
    let key: i64 = db
        .conn
        .query_row(
            "SELECT receiving_key_id FROM ironwood_swap_receive_reservations WHERE id=?1",
            [r.id],
            |r| r.get(0),
        )
        .unwrap();
    db.conn
        .execute(
            "UPDATE ironwood_receiving_key_scan_ranges SET range_end=?2 WHERE receiving_key_id=?1",
            params![key, u32::from(a.height) + 2],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO ironwood_receiving_key_scan_ranges VALUES (?1,?2,?3)",
            params![key, u32::from(a.height) + 3, u32::from(a.height) + 6],
        )
        .unwrap();
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &[])
            .is_err()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, a, &tail)
            .unwrap()
    );
}

#[test]
fn retired_reservation_reclaims_only_after_full_tail_and_provider_checks() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let initial = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, initial, &[])
        .unwrap();
    db.begin_swap_receive_quote(account, r.id, "expired", 1_000_000)
        .unwrap();
    db.record_swap_receive_quote(account, "expired", "deposit", None, 1_000_060)
        .unwrap();
    db.start_swap_receive_quote(account, "deposit").unwrap();
    db.observe_swap_receive_quote(account, "expired", "FAILED", false, 1_000_061)
        .unwrap();
    st.generate_and_scan_empty_blocks(10);
    let history = anchor(&st);
    st.generate_and_scan_empty_blocks(5);
    let through = anchor(&st);
    let tail = blocks(&st, history.height + 1, 5);
    let now = 1_000_061 + RECEIVE_RECLAIM_SECONDS;
    let db = st.wallet_mut().db_mut();
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.verify_swap_receive_history(account, r.id, history, &tail)
            .unwrap()
    );
    // Complete chain coverage does not replace fresh provider reconciliation.
    assert!(
        !db.reclaim_swap_receive_reservation(account, r.id, now, through)
            .unwrap()
    );
    db.observe_swap_receive_quote(account, "expired", "FAILED", false, now)
        .unwrap();
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, now, history)
            .is_err()
    );
    assert!(
        db.reclaim_swap_receive_reservation(account, r.id, now, through)
            .unwrap()
    );
}

#[test]
fn discovered_payment_invalidates_cached_address_check() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let db = st.wallet_mut().db_mut();
    db.verify_swap_receive_history(account, r.id, a, &[])
        .unwrap();
    db.request_swap_receive_recheck(account, r.key.key_id(), a)
        .unwrap();
    assert!(
        db.verified_swap_receive_reservation(account, r.id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.begin_swap_receive_quote(account, r.id, "recheck", 1_000_000)
            .is_err()
    );
}

#[test]
fn truncated_actions_and_partial_paid_tails_never_commit_verification() {
    let mut st = fixture();
    let account = st.test_account().unwrap().id();
    let r = reserve(&mut st);
    let a = anchor(&st);
    let (h, _, _) = st.generate_next_block(
        &IronwoodFvk(r.key.full_viewing_key().clone()),
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(12345),
    );
    st.scan_cached_blocks(h, 1);
    st.generate_and_scan_empty_blocks(1);
    let tail = blocks(&st, h, 2);
    let db = st.wallet_mut().db_mut();
    for variant in 0..3 {
        let mut bad = tail.clone();
        match variant {
            0 => bad[0].vtx.clear(),
            1 => bad[0].vtx[0].ironwood_actions[0].ciphertext.clear(),
            _ => bad[1].hash[0] ^= 1,
        }
        assert!(
            db.verify_swap_receive_history(account, r.id, a, &bad)
                .is_err()
        );
        assert!(
            db.verified_swap_receive_reservation(account, r.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM ironwood_swap_receive_used", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }
    assert!(
        !db.verify_swap_receive_history(account, r.id, a, &tail)
            .unwrap()
    );
}
