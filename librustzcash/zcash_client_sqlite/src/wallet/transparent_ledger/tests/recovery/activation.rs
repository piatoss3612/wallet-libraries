//! Activation: quarantine, qualification, promotion, projection, and input eligibility.

use zcash_client_backend::data_api::{
    AccountPurpose,
    transparent_ledger::{CommitRejection, IntegrityFailure, RefusedCommit},
};

use super::*;

/// Imports a view-only account sharing the test account's birthday.
fn import_account(st: &mut State, seed: u8) -> AccountUuid {
    let ufvk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
        st.network(),
        &[seed; 32],
        zip32::AccountId::ZERO,
    )
    .unwrap()
    .to_unified_full_viewing_key();
    let birthday = st.test_account().unwrap().birthday().clone();
    st.wallet_mut()
        .import_account_ufvk(
            &format!("account {seed}"),
            &ufvk,
            &birthday,
            AccountPurpose::ViewOnly,
            None,
        )
        .unwrap()
        .id()
}

/// A wallet like [`shadow_wallet`] with `extra` imported accounts, returned after the test
/// account.
fn shadow_wallet_with(extra: u8) -> (State, Vec<AccountUuid>) {
    let mut st = TestBuilder::new()
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    let mut accounts = vec![st.test_account().unwrap().id()];
    for seed in 0..extra {
        accounts.push(import_account(&mut st, 7 + seed));
    }
    scan_new_blocks(&mut st, 10);
    set_policy(&mut st, PrivateShadow);
    (st, accounts)
}

/// Returns why a commit was rejected, leaving any quarantine in place.
fn rejection(result: Result<CommitOutcome, SqliteClientError>) -> CommitRejection {
    match result {
        Err(SqliteClientError::TransparentLedgerCommitRejected(rejection)) => rejection,
        other => panic!("expected a rejected commit, got {other:?}"),
    }
}

fn source(name: &[u8], lineage: u64) -> RecoveryRevision {
    RecoveryRevision {
        source: name.to_vec(),
        ..revision(lineage, true)
    }
}

fn quarantined_accounts(st: &State) -> Vec<AccountUuid> {
    conn(st)
        .prepare(
            "SELECT a.uuid FROM tpir_quarantined_accounts q
             JOIN accounts a ON a.id = q.account_id ORDER BY a.id",
        )
        .unwrap()
        .query_map([], |row| row.get(0).map(AccountUuid))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn reader_version(st: &State) -> i64 {
    conn(st)
        .query_row("SELECT min_reader_version FROM tpir_meta", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// A receive of `account`'s external address, applied from `revision` with full coverage.
fn recover_one(
    st: &mut State,
    account: AccountUuid,
    revision: RecoveryRevision,
    tag: u8,
) -> ReceiveEvent {
    let ws = watch(st, account);
    let received = receive(tag, external(&ws), 40_000, below_target(&ws, 3));
    let mut c = commit(&ws);
    c.revision = revision;
    c.receives = vec![received.clone()];
    c.coverage = full_coverage(&ws);
    apply(st, c).unwrap();
    received
}

#[test]
fn integrity_failure_quarantines_the_source_and_every_affected_account() {
    let (mut st, accounts) = shadow_wallet_with(2);
    let [first, second, third] = accounts[..] else {
        unreachable!()
    };
    let fixture = source(b"fixture", 1);
    let received = recover_one(&mut st, first, fixture.clone(), 1);
    // The second account also holds the fixture's evidence, including an open page.
    let ws = watch(&st, second);
    let mut c = commit(&ws);
    c.revision = fixture.clone();
    c.opened_pages = vec![PageRequest {
        page: b"p".to_vec(),
        addresses: vec![external(&ws)],
        from: below_target(&ws, 1),
        through: ws.target.unwrap().height,
    }];
    apply(&mut st, c).unwrap();
    // The third account holds evidence from another source only.
    recover_one(&mut st, third, source(b"other", 1), 3);
    assert_eq!(reader_version(&st), 3);

    let ws = watch(&st, first);
    let before = count(&st, "tpir_receive_events");
    let mut c = commit(&ws);
    c.revision = fixture.clone();
    c.receives = vec![
        receive(9, external(&ws), 1_000, below_target(&ws, 1)),
        ReceiveEvent {
            mined_height: received.mined_height - 1,
            ..received.clone()
        },
    ];
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Integrity(IntegrityFailure::ReceivePlacement(received.outpoint))
    );

    // No submitted fact is applied, but the quarantine is durable.
    assert_eq!(count(&st, "tpir_receive_events"), before);
    assert_eq!(quarantined_accounts(&st), vec![first, second]);
    assert_eq!(count(&st, "tpir_quarantined_sources"), 1);
    assert_eq!(count(&st, "tpir_pending_pages"), 0);
    assert_eq!(reader_version(&st), 4);

    // Neither the source nor the accounts accept further commits, whatever their content.
    let ws = watch(&st, third);
    let mut c = commit(&ws);
    c.revision = fixture;
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Refused(RefusedCommit::SourceQuarantined)
    );
    let ws = watch(&st, second);
    let mut c = commit(&ws);
    c.revision = source(b"other", 1);
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Refused(RefusedCommit::AccountQuarantined)
    );
    // An unrelated account and source are unaffected.
    recover_one(&mut st, third, source(b"other", 1), 4);

    // Quarantine survives a rewind.
    let retained = watch(&st, first).target.unwrap().height - 5;
    st.truncate_to_height(retained);
    assert_eq!(quarantined_accounts(&st), vec![first, second]);
}

#[test]
fn a_failed_quarantine_write_aborts_the_commit() {
    let (mut st, account) = shadow_wallet();
    let received = recover_one(&mut st, account, revision(1, true), 1);
    conn(&st)
        .execute_batch(
            "CREATE TEMP TRIGGER fail_quarantine BEFORE INSERT ON tpir_quarantined_sources
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.receives = vec![
        receive(9, external(&ws), 1_000, below_target(&ws, 1)),
        ReceiveEvent {
            value: Zatoshis::const_from_u64(1),
            ..received
        },
    ];
    let before = production_dump(conn(&st));
    let events = count(&st, "tpir_receive_events");
    assert!(matches!(
        apply(&mut st, c),
        Err(SqliteClientError::DbError(_))
    ));
    assert_eq!(count(&st, "tpir_receive_events"), events);
    assert_eq!(count(&st, "tpir_quarantined_accounts"), 0);
    assert_eq!(reader_version(&st), 3);
    assert_eq!(production_dump(conn(&st)), before);
}
