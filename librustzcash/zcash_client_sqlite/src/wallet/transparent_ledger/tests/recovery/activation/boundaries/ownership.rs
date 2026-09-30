use super::*;
use transparent::keys::NonHardenedChildIndex;
use zcash_keys::address::Address;

fn future_receiver(
    st: &State,
    account: AccountUuid,
) -> (
    TransparentAddress,
    NonHardenedChildIndex,
    secp256k1::PublicKey,
    TransparentAddress,
) {
    let ws = watch(st, account);
    let (edge, index) = ws
        .addresses
        .iter()
        .filter_map(|w| match w.origin {
            WatchOrigin::Derived { scope, index } if scope == TransparentKeyScope::EXTERNAL => {
                Some((w.address, index))
            }
            _ => None,
        })
        .max_by_key(|(_, i)| *i)
        .unwrap();
    let next = NonHardenedChildIndex::from_index(index.index() + 1).unwrap();
    let ufvk = st
        .test_account()
        .unwrap()
        .usk()
        .to_unified_full_viewing_key();
    let key = ufvk
        .transparent()
        .unwrap()
        .derive_address_pubkey(TransparentKeyScope::EXTERNAL, next)
        .unwrap();
    (TransparentAddress::from_pubkey(&key), next, key, edge)
}
fn grow(st: &mut State, account: AccountUuid, edge: TransparentAddress) {
    let ws = watch(st, account);
    let mut c = commit(&ws);
    c.receives = vec![receive(211, edge, 1_000, below_target(&ws, 2))];
    c.coverage = full_coverage(&ws);
    assert!(apply(st, c).unwrap().window_grew);
}
fn import(st: &mut State, owner: AccountUuid, key: secp256k1::PublicKey) {
    st.wallet_mut()
        .db_mut()
        .import_standalone_transparent_pubkey(owner, key)
        .unwrap();
}

#[test]
fn imported_receiver_has_one_candidate_owner_in_either_commit_order() {
    for b_first in [false, true] {
        let (mut st, accounts) = shadow_wallet_with(1);
        let (a, b) = (accounts[0], accounts[1]);
        let (address, _, key, edge) = future_receiver(&st, a);
        import(&mut st, b, key);
        let before = production_dump(conn(&st));
        grow(&mut st, a, edge);
        assert!(!watch(&st, a).addresses.iter().any(|w| w.address == address));
        assert!(watch(&st, b).addresses.iter().any(|w| w.address == address));
        let mut a_commit = commit(&watch(&st, a));
        a_commit.coverage = full_coverage(&watch(&st, a));
        let mut b_commit = commit(&watch(&st, b));
        b_commit.coverage = full_coverage(&watch(&st, b));
        b_commit.receives = vec![receive(
            212,
            address,
            20_000,
            below_target(&watch(&st, b), 1),
        )];
        for c in if b_first {
            vec![b_commit, a_commit]
        } else {
            vec![a_commit, b_commit]
        } {
            apply(&mut st, c).unwrap();
        }
        assert_eq!(production_dump(conn(&st)), before);
        assert_eq!(count(&st, "tpir_quarantined_accounts"), 0);
        assert!(
            !recovery(&st, a)
                .receives
                .iter()
                .any(|r| r.address == address)
        );
        assert!(
            recovery(&st, b)
                .receives
                .iter()
                .any(|r| r.address == address)
        );
        let reopened = crate::WalletDb::for_path(
            st.wallet().data_file_path(),
            *st.network(),
            crate::testing::db::test_clock(),
            crate::testing::db::test_rng(),
        )
        .unwrap()
        .with_transparent_ledger_mode(PrivateShadow);
        assert_eq!(reopened.transparent_watch_set(a).unwrap(), watch(&st, a));
        // B's activity at the shared receiver extends A's window on A's next commit. Cover it
        // until the window stops growing, as a coordinator would.
        loop {
            let ws = watch(&st, a);
            let mut c = commit(&ws);
            c.coverage = full_coverage(&ws);
            if !apply(&mut st, c).unwrap().window_grew {
                break;
            }
        }
        qualify(&mut st, &revision(1, true));
        set_policy(&mut st, PrivateRequired);
        let encoded = Address::Transparent(address).encode(st.network());
        let owner = |st: &State| -> i64 {
            conn(st)
                .query_row(
                    "SELECT account_id FROM addresses WHERE cached_transparent_receiver_address = ?1",
                    [&encoded],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let importer = owner(&st);
        // Promotion succeeds instead of refusing and rolling back on every retry. Writing the
        // window leaves the receiver with its owner; projecting A's receive at the window edge then
        // runs the wallet's gap-limit generation, which transfers the adjacent receiver under the
        // existing rule. A is active, and recovery work asks it to cover the receiver before its
        // authority returns.
        promote(&mut st, a).unwrap();
        assert_ne!(owner(&st), importer);
        assert!(watch(&st, a).addresses.iter().any(|w| w.address == address));
        assert!(!watch(&st, b).addresses.iter().any(|w| w.address == address));
        assert_ne!(snapshot(&st, a).authority, TransparentAuthority::Private);
        assert!(work(&st, a, 256).items.iter().any(|item| matches!(
            item,
            TransparentRecoveryWork::CheckRange(range) if range.address == address
        )));
        let mut c = commit(&watch(&st, a));
        c.coverage = full_coverage(&watch(&st, a));
        c.receives = vec![receive(
            212,
            address,
            20_000,
            below_target(&watch(&st, a), 1),
        )];
        apply(&mut st, c).unwrap();
        // New activity may grow the derivation window; complete the freshly scheduled gaps.
        for _ in 0..3 {
            if snapshot(&st, a).authority == TransparentAuthority::Private {
                break;
            }
            let ws = watch(&st, a);
            let mut c = commit(&ws);
            c.coverage = full_coverage(&ws);
            apply(&mut st, c).unwrap();
        }
        assert_eq!(snapshot(&st, a).authority, TransparentAuthority::Private);
    }
}

#[test]
fn import_after_candidate_recovery_discards_conflicting_work_without_quarantine() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let (a, b) = (accounts[0], accounts[1]);
    let (address, _, key, edge) = future_receiver(&st, a);
    grow(&mut st, a, edge);
    let ws = watch(&st, a);
    let mut c = commit(&ws);
    c.receives = vec![receive(212, address, 20_000, below_target(&ws, 1))];
    c.coverage = full_coverage(&ws)
        .into_iter()
        .map(|r| AddressRange {
            through: r.through - 1,
            ..r
        })
        .collect();
    c.opened_pages = vec![PageRequest {
        page: b"owned-later".to_vec(),
        addresses: vec![address],
        from: ws.target.unwrap().height,
        through: ws.target.unwrap().height,
    }];
    apply(&mut st, c).unwrap();
    let mut stale = commit(&ws);
    stale.receives = vec![receive(212, address, 20_000, below_target(&ws, 1))];
    let received = stale.receives[0].clone();
    import(&mut st, b, key);
    assert!(
        !recovery(&st, a)
            .receives
            .iter()
            .any(|r| r.address == address)
    );
    assert!(watch(&st, a).pending_pages.is_empty());
    assert_eq!(
        rejection(apply(&mut st, stale)),
        CommitRejection::Stale(StaleCommit::AddressNotWatched(address))
    );
    assert_eq!(count(&st, "tpir_quarantined_accounts"), 0);
    let mut c = commit(&watch(&st, b));
    c.receives = vec![received];
    c.coverage = full_coverage(&watch(&st, b));
    apply(&mut st, c).unwrap();
}

#[test]
fn imported_ownership_filters_all_derivable_scopes() {
    for (slot, scope) in [
        TransparentKeyScope::EXTERNAL,
        TransparentKeyScope::INTERNAL,
        TransparentKeyScope::EPHEMERAL,
    ]
    .into_iter()
    .enumerate()
    {
        let (mut st, accounts) = shadow_wallet_with(1);
        let index = NonHardenedChildIndex::from_index(100).unwrap();
        let ufvk = st
            .test_account()
            .unwrap()
            .usk()
            .to_unified_full_viewing_key();
        let key = ufvk
            .transparent()
            .unwrap()
            .derive_address_pubkey(scope, index)
            .unwrap();
        let address = TransparentAddress::from_pubkey(&key);
        import(&mut st, accounts[1], key);
        conn(&st).execute("INSERT INTO tpir_candidate_windows(account_id,key_scope,end_index) VALUES (?1,?2,101)", rusqlite::params![st.test_account().unwrap().account().internal_id().0, slot]).unwrap();
        assert!(
            !watch(&st, accounts[0])
                .addresses
                .iter()
                .any(|w| w.address == address)
        );
        assert!(
            watch(&st, accounts[1])
                .addresses
                .iter()
                .any(|w| w.address == address)
        );
    }
}

#[test]
fn ownership_cleanup_failure_rolls_back_the_import_and_evidence() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let (address, _, key, edge) = future_receiver(&st, accounts[0]);
    grow(&mut st, accounts[0], edge);
    let ws = watch(&st, accounts[0]);
    let mut c = commit(&ws);
    c.receives = vec![receive(212, address, 20_000, below_target(&ws, 1))];
    apply(&mut st, c).unwrap();
    let before = production_dump(conn(&st));
    let evidence = recovery(&st, accounts[0]);
    conn(&st).execute_batch("CREATE TEMP TRIGGER fail_import_cleanup BEFORE DELETE ON tpir_receive_events BEGIN SELECT RAISE(ABORT, 'injected cleanup failure'); END;").unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .import_standalone_transparent_pubkey(accounts[1], key)
            .is_err()
    );
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(recovery(&st, accounts[0]), evidence);
    assert!(
        watch(&st, accounts[0])
            .addresses
            .iter()
            .any(|w| w.address == address)
    );
}
