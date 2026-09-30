use super::*;

#[test]
fn recovery_batches_resume_pages_before_gaps_and_persist_across_reopen() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let ws = watch(&st, account);
    let addr = ws.addresses[0].address;
    let start = ws.addresses[0].required_from;
    let target = ws.target.unwrap().height;
    let mut c = commit(&ws);
    c.coverage = full_coverage(&ws)
        .into_iter()
        .filter(|r| r.address != addr)
        .collect();
    c.coverage.push(AddressRange {
        address: addr,
        from: start,
        through: start + 1,
    });
    c.unsupported.push(AddressRange {
        address: addr,
        from: start + 2,
        through: target,
    });
    c.opened_pages.push(PageRequest {
        page: b"resume".to_vec(),
        addresses: vec![addr],
        from: start + 3,
        through: start + 4,
    });
    apply(&mut st, c).unwrap();
    let batch = work(&st, account, 256);
    assert_eq!(batch.context, ws.context());
    assert!(!batch.has_more);
    assert!(
        matches!(&batch.items[0], TransparentRecoveryWork::ResumePage(p) if p.request.page == b"resume")
    );
    let ranges: Vec<_> = batch
        .items
        .iter()
        .filter_map(|w| match w {
            TransparentRecoveryWork::CheckRange(r) => Some(*r),
            _ => None,
        })
        .collect();
    assert_eq!(
        ranges,
        vec![
            AddressRange {
                address: addr,
                from: start + 2,
                through: start + 2
            },
            AddressRange {
                address: addr,
                from: start + 5,
                through: target
            }
        ]
    );
    let short = work(&st, account, 1);
    assert_eq!(short.items, batch.items[..1]);
    assert!(short.has_more);
    let reopened = crate::WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        crate::testing::db::test_clock(),
        crate::testing::db::test_rng(),
    )
    .unwrap()
    .with_transparent_ledger_mode(PrivateShadow);
    assert_eq!(
        reopened
            .transparent_recovery_work(account, NonZeroUsize::new(256).unwrap())
            .unwrap(),
        batch
    );
    let mut c = commit(&watch(&st, account));
    c.completed_pages.push(b"resume".to_vec());
    apply(&mut st, c).unwrap();
    let mut c = commit(&watch(&st, account));
    c.revision.source = b"supported-other-source".to_vec();
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    assert!(work(&st, account, 1).items.is_empty());
    assert!(!work(&st, account, 1).has_more);
    scan_new_blocks(&mut st, 1);
    assert!(work(&st, account, 256).items.iter().all(|w| matches!(w, TransparentRecoveryWork::CheckRange(r) if r.from == target + 1 && r.through == target + 1)));
}

#[test]
fn recovery_work_observes_window_growth_birthday_and_rewind() {
    let (mut st, account) = shadow_wallet();
    conn(&st)
        .execute(
            "UPDATE accounts SET birthday_height = birthday_height + 2",
            [],
        )
        .unwrap();
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    assert!(work(&st, account, 256).items.is_empty());
    scan_new_blocks(&mut st, 2);
    assert!(!work(&st, account, 256).items.is_empty());
    st.truncate_to_height(ws.target.unwrap().height);
    assert!(work(&st, account, 256).items.is_empty());
    conn(&st)
        .execute(
            "UPDATE accounts SET birthday_height = birthday_height - 1",
            [],
        )
        .unwrap();
    assert!(work(&st, account, 256).items.iter().all(|w| matches!(w, TransparentRecoveryWork::CheckRange(r) if r.from == ws.addresses[0].required_from - 1 && r.through == r.from)));
    // An event at the edge grows the candidate window, exposing more durable missing work.
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.receives = vec![receive(
        61,
        ws.addresses.last().unwrap().address,
        20_000,
        below_target(&ws, 0),
    )];
    apply(&mut st, c).unwrap();
    assert!(!work(&st, account, 256).items.is_empty());
}

#[test]
fn recovery_work_refuses_unconfigured_unknown_and_quarantined_accounts() {
    let (st, account) = shadow_wallet();
    let limit = NonZeroUsize::new(1).unwrap();
    let unknown = AccountUuid(uuid::Uuid::nil());
    assert!(matches!(
        st.wallet().db().transparent_recovery_work(unknown, limit),
        Err(SqliteClientError::AccountUnknown)
    ));
    conn(&st).execute("INSERT INTO tpir_quarantined_accounts (account_id) SELECT id FROM accounts WHERE uuid = ?1", [account.0]).unwrap();
    assert!(matches!(
        st.wallet().db().transparent_recovery_work(account, limit),
        Err(SqliteClientError::TransparentLedgerCommitRejected(
            CommitRejection::Refused(RefusedCommit::AccountQuarantined)
        ))
    ));
}

#[test]
fn recovery_work_caps_batches_and_orders_pending_pages() {
    let (mut st, account) = shadow_wallet();
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    for i in (0..258).rev() {
        c.opened_pages.push(PageRequest {
            page: format!("page-{i:03}").into_bytes(),
            addresses: vec![external(&ws)],
            from: ws.addresses[0].required_from,
            through: ws.target.unwrap().height,
        });
    }
    apply(&mut st, c).unwrap();
    let batch = work(&st, account, usize::MAX);
    assert_eq!(batch.items.len(), 256);
    assert!(batch.has_more);
    assert!(
        matches!(&batch.items[0], TransparentRecoveryWork::ResumePage(p) if p.request.page == b"page-000")
    );
    let mut c = commit(&watch(&st, account));
    c.completed_pages = batch
        .items
        .iter()
        .map(|item| match item {
            TransparentRecoveryWork::ResumePage(p) => p.request.page.clone(),
            _ => panic!("pages must precede ranges"),
        })
        .collect();
    apply(&mut st, c).unwrap();
    assert!(
        matches!(&work(&st, account, 1).items[0], TransparentRecoveryWork::ResumePage(p) if p.request.page == b"page-256")
    );
}

#[test]
fn recovery_work_without_a_target_is_empty_and_handles_still_require_configuration() {
    let st = wallet_state(TestDbFactory::file_backed());
    let account = st.test_account().unwrap().id();
    let limit = NonZeroUsize::new(1).unwrap();
    let batch = st
        .wallet()
        .db()
        .transparent_recovery_work(account, limit)
        .unwrap();
    assert_eq!(batch.context, None);
    assert!(batch.items.is_empty());
    assert!(!batch.has_more);
    let reopened = crate::WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        crate::testing::db::test_clock(),
        crate::testing::db::test_rng(),
    )
    .unwrap();
    assert!(matches!(
        reopened.transparent_recovery_work(account, limit),
        Err(SqliteClientError::TransparentLedgerModeNotConfigured)
    ));
}

#[test]
fn scheduling_uses_supported_union_across_sources_and_is_account_scoped() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let ws = watch(&st, accounts[0]);
    let middle = ws.addresses[0].required_from + 3;
    let mut c = commit(&ws);
    c.coverage = full_coverage(&ws)
        .into_iter()
        .map(|mut r| {
            r.through = middle;
            r
        })
        .collect();
    apply(&mut st, c).unwrap();
    let mut c = commit(&ws);
    c.revision.source = b"second-source".to_vec();
    c.coverage = full_coverage(&ws)
        .into_iter()
        .map(|mut r| {
            r.from = middle + 1;
            r
        })
        .collect();
    apply(&mut st, c).unwrap();
    assert!(work(&st, accounts[0], 256).items.is_empty());
    assert!(recovery(&st, accounts[0]).blockers.is_empty());
    assert!(!work(&st, accounts[1], 256).items.is_empty());
    assert!(
        recovery(&st, accounts[1])
            .blockers
            .contains(&CandidateBlocker::IncompleteCoverage)
    );
}

#[test]
fn empty_interval_promotion_grants_no_funds_and_missing_coverage_revokes_authority() {
    let (mut st, account) = shadow_wallet();
    let target = watch(&st, account).target.unwrap();
    conn(&st)
        .execute(
            "UPDATE accounts SET birthday_height = ?1 WHERE uuid = ?2",
            rusqlite::params![u32::from(target.height + 1), account.0],
        )
        .unwrap();
    set_policy(&mut st, PrivateRequired);
    assert_eq!(count(&st, "tpir_qualified_revisions"), 0);
    promote(&mut st, account).unwrap();
    assert_eq!(
        snapshot(&st, account).authority,
        TransparentAuthority::Private
    );
    assert_eq!(
        snapshot(&st, account).authorized.unwrap().regular.total(),
        Zatoshis::ZERO
    );
    assert!(work(&st, account, 1).items.is_empty());
    scan_new_blocks(&mut st, 1);
    assert_eq!(
        snapshot(&st, account).authority,
        TransparentAuthority::Unavailable
    );
    assert!(!work(&st, account, 1).items.is_empty());
}

#[test]
fn a_pending_page_retains_its_revision_and_target_when_the_tip_advances() {
    let (mut st, account) = shadow_wallet();
    let ws = watch(&st, account);
    let original_context = ws.context().unwrap();
    let mut c = commit(&ws);
    let rev = c.revision.clone();
    let page = PageRequest {
        page: b"older-target".to_vec(),
        addresses: vec![external(&ws)],
        from: below_target(&ws, 2),
        through: ws.target.unwrap().height,
    };
    c.opened_pages.push(page.clone());
    apply(&mut st, c).unwrap();
    scan_new_blocks(&mut st, 1);
    let batch = work(&st, account, 1);
    assert!(batch.context.unwrap().target.height > original_context.target.height);
    let TransparentRecoveryWork::ResumePage(pending) = &batch.items[0] else {
        panic!("pending page must precede gaps")
    };
    assert_eq!(pending.target, original_context.target);
    assert_eq!(pending.revision, rev);
    assert_eq!(pending.request, page);
    let mut c = commit(&watch(&st, account));
    c.revision = pending.revision.clone();
    c.anchor = pending.target;
    c.completed_pages.push(pending.request.page.clone());
    c.coverage.push(AddressRange {
        address: pending.request.addresses[0],
        from: pending.request.from,
        through: pending.request.through,
    });
    apply(&mut st, c).unwrap();
    assert!(watch(&st, account).pending_pages.is_empty());
    assert!(work(&st, account, 256).items.iter().any(|item| matches!(item, TransparentRecoveryWork::CheckRange(r) if r.address == page.addresses[0] && r.from == original_context.target.height + 1)));
}
