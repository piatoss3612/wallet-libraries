//! Activation: quarantine, qualification, promotion, projection, and input eligibility.

use zcash_client_backend::data_api::{
    AccountPurpose,
    transparent_ledger::{
        AccountLifecycle, ChainPoint, CommitRejection, IntegrityFailure, LastKnownSource,
        RecoveryBlocker, RefusedCommit, TransparentAuthority, TransparentLedgerSnapshot,
    },
    wallet::ConfirmationsPolicy,
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

/// A file-backed wallet like [`shadow_wallet`] with `extra` imported accounts, returned after
/// the test account.
fn shadow_wallet_with(extra: u8) -> (State, Vec<AccountUuid>) {
    let mut st = TestBuilder::new()
        .with_data_store_factory(TestDbFactory::file_backed())
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

fn snapshot(st: &State, account: AccountUuid) -> TransparentLedgerSnapshot<AccountUuid> {
    st.wallet()
        .db()
        .transparent_ledger_snapshot(account, ConfirmationsPolicy::MIN)
        .unwrap()
}

fn qualify(st: &mut State, revision: &RecoveryRevision) {
    st.wallet_mut()
        .db_mut()
        .qualify_transparent_revision(revision)
        .unwrap()
}

/// Recovers `account` completely through the target from `revision`, with one receive, and
/// covers any addresses its window growth adds.
fn recover_completely(
    st: &mut State,
    account: AccountUuid,
    revision: &RecoveryRevision,
    tag: u8,
) -> ReceiveEvent {
    let received = recover_one(st, account, revision.clone(), tag);
    let ws = watch(st, account);
    let mut c = commit(&ws);
    c.revision = revision.clone();
    c.coverage = full_coverage(&ws);
    apply(st, c).unwrap();
    assert_eq!(recovery(st, account).blockers, vec![]);
    received
}

fn chain_point(st: &State, height: BlockHeight) -> ChainPoint {
    ChainPoint {
        height,
        hash: crate::wallet::get_block_hash(conn(st), height)
            .unwrap()
            .unwrap(),
    }
}

#[test]
fn private_snapshots_report_candidate_coverage_and_an_unverified_total() {
    let (mut st, account) = shadow_wallet();
    let fixture = revision(1, true);
    recover_completely(&mut st, account, &fixture, 1);
    let target = watch(&st, account).target.unwrap();

    // Shadow keeps public authority and reports the candidate ledger alongside it.
    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Public);
    assert_eq!(s.covered_through, Some(target));
    assert_eq!(
        s.recovered_unverified,
        Some(Zatoshis::const_from_u64(40_000))
    );
    assert!(s.blockers.is_empty());

    // Under PrivateRequired the complete but unpromoted candidate is still no authority.
    set_policy(&mut st, PrivateRequired);
    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Unavailable);
    assert_eq!(s.authorized, None);
    assert_eq!(s.covered_through, Some(chain_point(&st, target.height)));
    assert_eq!(
        s.blockers,
        vec![
            RecoveryBlocker::NotActivated,
            RecoveryBlocker::UnqualifiedRevision
        ]
    );
    qualify(&mut st, &fixture);
    assert_eq!(
        snapshot(&st, account).blockers,
        vec![RecoveryBlocker::NotActivated]
    );

    // Public reports no candidate state.
    set_policy(&mut st, Public);
    let s = snapshot(&st, account);
    assert_eq!((s.covered_through, s.recovered_unverified), (None, None));
}

#[test]
fn a_legacy_output_the_candidate_lacks_is_a_discrepancy() {
    let (mut st, account) = shadow_wallet();
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    // Legacy public discovery recorded an output the private source never reports.
    let height = ws.target.unwrap().height;
    let utxo = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
        OutPoint::new([0xaa; 32], 0),
        transparent::bundle::TxOut::new(
            Zatoshis::const_from_u64(70_000),
            external(&ws).script().into(),
        ),
        Some(height),
        Some(account),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .db_mut()
        .put_received_transparent_utxo(&utxo)
        .unwrap();
    recover_completely(&mut st, account, &fixture, 1);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    let s = snapshot(&st, account);
    assert_eq!(
        s.blockers,
        vec![
            RecoveryBlocker::NotActivated,
            RecoveryBlocker::LegacyDiscrepancy
        ]
    );
    // The legacy amount is still shown as last-known evidence.
    let last_known = s.last_known.unwrap();
    assert_eq!(last_known.source, LastKnownSource::LegacyPublic);
    assert_eq!(
        last_known.balance.regular.total(),
        Zatoshis::const_from_u64(70_000)
    );
}

#[test]
fn a_mined_local_output_the_candidate_lacks_is_a_discrepancy() {
    let (mut st, account) = shadow_wallet();
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    // The wallet recorded a mined output of its own construction, such as the change of a
    // shielded-funded unshielding, that the private source never reports.
    let outpoint = OutPoint::new([0xab; 32], 0);
    let utxo = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
        outpoint.clone(),
        transparent::bundle::TxOut::new(
            Zatoshis::const_from_u64(70_000),
            external(&ws).script().into(),
        ),
        Some(ws.target.unwrap().height),
        Some(account),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .db_mut()
        .put_received_transparent_utxo(&utxo)
        .unwrap();
    conn(&st)
        .execute(
            "UPDATE tpir_output_origins SET origin = 1 WHERE origin = 0",
            [],
        )
        .unwrap();
    assert_eq!(super::super::output_origins(conn(&st), &outpoint), vec![1]);

    recover_completely(&mut st, account, &fixture, 1);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    assert_eq!(
        snapshot(&st, account).blockers,
        vec![
            RecoveryBlocker::NotActivated,
            RecoveryBlocker::LegacyDiscrepancy
        ]
    );
    assert!(blocked(promote(&mut st, account)).contains(&RecoveryBlocker::LegacyDiscrepancy));
}

#[test]
fn commits_capture_the_account_lifecycle() {
    let (mut st, account) = shadow_wallet();
    let ws = watch(&st, account);
    assert_eq!(ws.lifecycle, AccountLifecycle::Candidate);
    let mut c = commit(&ws);
    c.context.lifecycle = AccountLifecycle::Active;
    c.coverage = full_coverage(&ws);
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Stale(StaleCommit::LifecycleChanged)
    );
    assert_eq!(count(&st, "tpir_coverage"), 0);
}

#[test]
fn qualification_binds_to_the_exact_revision() {
    let (mut st, account) = shadow_wallet();
    let fixture = revision(1, true);
    recover_one(&mut st, account, fixture.clone(), 1);
    assert_eq!(reader_version(&st), 3);
    for other in [
        RecoveryRevision {
            sealed: false,
            ..fixture.clone()
        },
        RecoveryRevision {
            lineage: 2,
            ..fixture.clone()
        },
        RecoveryRevision {
            publication: PublicationAnchor {
                hash: BlockHash([8; 32]),
                ..fixture.publication
            },
            ..fixture.clone()
        },
    ] {
        assert!(matches!(
            st.wallet_mut()
                .db_mut()
                .qualify_transparent_revision(&other),
            Err(SqliteClientError::TransparentLedgerCommitRejected(
                CommitRejection::Integrity(IntegrityFailure::RevisionMismatch)
            ))
        ));
    }
    assert_eq!(count(&st, "tpir_qualified_revisions"), 0);
    qualify(&mut st, &fixture);
    qualify(&mut st, &fixture);
    assert_eq!(count(&st, "tpir_qualified_revisions"), 1);
    assert_eq!(reader_version(&st), 4);

    // A new revision is recorded as a commit would record it, superseding older provisional
    // revisions of its source.
    let provisional = RecoveryRevision {
        source: b"moving".to_vec(),
        ..revision(1, false)
    };
    recover_one(&mut st, account, provisional.clone(), 2);
    let coverage = count(&st, "tpir_coverage");
    qualify(
        &mut st,
        &RecoveryRevision {
            revision: b"next".to_vec(),
            lineage: 2,
            ..provisional.clone()
        },
    );
    assert!(count(&st, "tpir_coverage") < coverage);
    assert!(matches!(
        st.wallet_mut()
            .db_mut()
            .qualify_transparent_revision(&provisional),
        Err(SqliteClientError::TransparentLedgerCommitRejected(
            CommitRejection::Stale(StaleCommit::SupersededRevision)
        ))
    ));
}

fn promote(st: &mut State, account: AccountUuid) -> Result<(), SqliteClientError> {
    st.wallet_mut()
        .db_mut()
        .promote_transparent_account(account)
}

fn blocked(result: Result<(), SqliteClientError>) -> Vec<RecoveryBlocker> {
    match result {
        Err(SqliteClientError::TransparentPromotionBlocked(blockers)) => blockers,
        other => panic!("expected a blocked promotion, got {other:?}"),
    }
}

/// The watched external address with the highest derived index.
fn last_external(ws: &TransparentWatchSet<AccountUuid>) -> (TransparentAddress, u32) {
    ws.addresses
        .iter()
        .filter_map(|w| match w.origin {
            WatchOrigin::Derived { scope, index } if scope == TransparentKeyScope::EXTERNAL => {
                Some((w.address, index.index()))
            }
            _ => None,
        })
        .max_by_key(|(_, index)| *index)
        .unwrap()
}

fn lifecycle(st: &State, account: AccountUuid) -> AccountLifecycle {
    watch(st, account).lifecycle
}

fn spend_count(st: &State, outpoint: &OutPoint) -> i64 {
    conn(st)
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_output_spends s
             JOIN transparent_received_outputs o ON o.id = s.transparent_received_output_id
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = ?1 AND o.output_index = ?2",
            rusqlite::params![outpoint.hash(), outpoint.n()],
            |row| row.get(0),
        )
        .unwrap()
}

/// A shadow wallet whose account is completely recovered from a qualified fixture revision,
/// with an unspent receive at the last external address and a spent one, under a durable
/// `PrivateRequired` policy.
fn ready_wallet() -> (State, AccountUuid, ReceiveEvent, ReceiveEvent) {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let (last, _) = last_external(&ws);
    let unspent = receive(1, last, 40_000, below_target(&ws, 3));
    let spent = receive(2, external(&ws), 25_000, below_target(&ws, 4));
    let mut c = commit(&ws);
    c.revision = fixture.clone();
    c.receives = vec![unspent.clone(), spent.clone()];
    c.spends = vec![spend(3, &spent, below_target(&ws, 2))];
    c.coverage = full_coverage(&ws);
    assert!(apply(&mut st, c).unwrap().window_grew);
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.revision = fixture.clone();
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    assert_eq!(recovery(&st, account).blockers, vec![]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    (st, account, unspent, spent)
}

#[test]
fn promotion_projects_the_ledger_and_grants_private_authority() {
    let (mut st, account, unspent, spent) = ready_wallet();
    let windows = count(&st, "tpir_candidate_windows");
    assert!(windows > 0);
    let addresses = count(&st, "addresses");

    promote(&mut st, account).unwrap();
    assert_eq!(lifecycle(&st, account), AccountLifecycle::Active);
    assert_eq!(reader_version(&st), 4);

    // The window's addresses are now the wallet's own, and its candidate rows are gone.
    assert_eq!(count(&st, "tpir_candidate_windows"), 0);
    assert!(count(&st, "addresses") > addresses);
    assert!(
        watch(&st, account)
            .addresses
            .iter()
            .all(|w| !matches!(w.origin, WatchOrigin::CandidateWindow { .. }))
    );

    // Every placed event is projected with the ledger origin.
    assert_eq!(
        super::super::output_origins(conn(&st), &unspent.outpoint),
        vec![2]
    );
    assert_eq!(
        super::super::output_origins(conn(&st), &spent.outpoint),
        vec![2]
    );
    assert_eq!(
        super::super::spend_origins(conn(&st), &spent.outpoint),
        vec![2]
    );
    assert_eq!(spend_count(&st, &spent.outpoint), 1);
    assert_eq!(super::super::records_without_origin(conn(&st)), 0);

    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Private);
    assert_eq!(
        s.completion,
        zcash_client_backend::data_api::transparent_ledger::RecoveryCompletion::Complete
    );
    assert!(s.blockers.is_empty());
    assert_eq!(s.last_known, None);
    let authorized = s.authorized.unwrap();
    assert_eq!(
        authorized.regular.spendable_value(),
        Zatoshis::const_from_u64(40_000)
    );
    assert_eq!(authorized.coinbase.total(), Zatoshis::ZERO);

    // Promotion is idempotent.
    let before = production_dump(conn(&st));
    promote(&mut st, account).unwrap();
    assert_eq!(production_dump(conn(&st)), before);
}

#[test]
fn promotion_makes_a_legacy_receiver_without_an_address_row_the_wallets_own() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let (legacy, _) = crate::wallet::transparent::get_legacy_transparent_address(
        st.network(),
        conn(&st),
        account,
    )
    .unwrap()
    .unwrap();
    // The watch set includes the legacy receiver even when the wallet holds no row for it.
    let encoded = zcash_keys::encoding::AddressCodec::encode(&legacy, st.network());
    assert_eq!(
        conn(&st)
            .execute(
                "DELETE FROM addresses WHERE cached_transparent_receiver_address = ?1",
                [&encoded],
            )
            .unwrap(),
        1
    );
    let ws = watch(&st, account);
    assert!(ws.addresses.iter().any(|w| w.address == legacy));

    let fixture = revision(1, true);
    let received = receive(1, legacy, 30_000, below_target(&ws, 3));
    cover(&mut st, account, &fixture, vec![received.clone()]);
    assert_eq!(recovery(&st, account).blockers, vec![]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);

    promote(&mut st, account).unwrap();
    assert_eq!(
        super::super::output_origins(conn(&st), &received.outpoint),
        vec![2]
    );
    assert_eq!(
        snapshot(&st, account).authorized.unwrap().regular.total(),
        received.value
    );
}

#[test]
fn a_window_reaching_the_last_child_index_blocks_promotion() {
    let (mut st, account, _, _) = ready_wallet();
    let account_ref: i64 = conn(&st)
        .query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            [account.0],
            |row| row.get(0),
        )
        .unwrap();
    // Put the wallet's external addresses one short of the last non-hardened index, and the
    // candidate window at its end, so that the window's only address is that last index.
    let limit = 1u32 << 31;
    let index = |i| transparent::keys::NonHardenedChildIndex::from_index(i).unwrap();
    crate::wallet::transparent::generate_address_range(
        conn(&st),
        st.network(),
        crate::AccountRef(account_ref),
        TransparentKeyScope::EXTERNAL,
        zcash_keys::keys::UnifiedAddressRequest::unsafe_custom(
            zcash_keys::keys::ReceiverRequirement::Allow,
            zcash_keys::keys::ReceiverRequirement::Allow,
            zcash_keys::keys::ReceiverRequirement::Require,
        ),
        index(limit - 2)..index(limit - 1),
        false,
    )
    .unwrap();
    conn(&st)
        .execute(
            "INSERT OR REPLACE INTO tpir_candidate_windows (account_id, key_scope, end_index)
             VALUES (?1, 0, ?2)",
            rusqlite::params![account_ref, i64::from(limit)],
        )
        .unwrap();
    cover(&mut st, account, &revision(1, true), vec![]);

    // The wallet cannot store that address, so the account is blocked rather than promoted.
    assert!(
        recovery(&st, account)
            .blockers
            .contains(&CandidateBlocker::WindowUnderivable)
    );
    assert!(
        blocked(promote(&mut st, account)).contains(&RecoveryBlocker::Recovery(
            CandidateBlocker::WindowUnderivable
        ))
    );
    assert_eq!(lifecycle(&st, account), AccountLifecycle::Candidate);
}

#[test]
fn promotion_is_blocked_until_every_condition_holds() {
    let (mut st, account) = shadow_wallet();
    let unchanged = |st: &State, before: &Vec<(String, Vec<String>)>| {
        assert_eq!(&production_dump(conn(st)), before);
        assert_eq!(count(st, "tpir_active_accounts"), 0);
    };
    let before = production_dump(conn(&st));

    // Shadow cannot promote.
    assert!(matches!(
        promote(&mut st, account),
        Err(SqliteClientError::TransparentRecoveryNotEnabled)
    ));

    // Nothing recovered yet.
    set_policy(&mut st, PrivateRequired);
    assert_eq!(
        blocked(promote(&mut st, account)),
        vec![RecoveryBlocker::Recovery(
            CandidateBlocker::IncompleteCoverage
        )]
    );
    unchanged(&st, &before);

    // Complete, but from an unqualified revision.
    set_policy(&mut st, PrivateShadow);
    let fixture = revision(1, true);
    recover_completely(&mut st, account, &fixture, 1);
    set_policy(&mut st, PrivateRequired);
    assert_eq!(
        blocked(promote(&mut st, account)),
        vec![RecoveryBlocker::UnqualifiedRevision]
    );
    unchanged(&st, &before);
    qualify(&mut st, &fixture);

    // A quarantined account.
    conn(&st)
        .execute(
            "INSERT INTO tpir_quarantined_accounts SELECT id FROM accounts",
            [],
        )
        .unwrap();
    assert_eq!(
        blocked(promote(&mut st, account)),
        vec![RecoveryBlocker::Quarantined]
    );
    lift_quarantine(&st);

    // The contiguously scanned chain is behind the known tip.
    let not_our_key = ExtendedSpendingKey::master(&[]).to_diversifiable_full_viewing_key();
    let value = Zatoshis::const_from_u64(10_000);
    let (start, _, _) = st.generate_next_block(&not_our_key, AddressType::DefaultExternal, value);
    let (tip, _, _) = st.generate_next_block(&not_our_key, AddressType::DefaultExternal, value);
    st.wallet_mut().update_chain_tip(tip).unwrap();
    assert_eq!(
        blocked(promote(&mut st, account)),
        vec![RecoveryBlocker::ChainBehindTip]
    );
    // Scanning to the tip needs coverage through it.
    st.scan_cached_blocks(start, 2);
    assert_eq!(
        blocked(promote(&mut st, account)),
        vec![RecoveryBlocker::Recovery(
            CandidateBlocker::IncompleteCoverage
        )]
    );
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.revision = fixture;
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    // A handle weaker than the durable policy cannot promote either.
    let handle = st.wallet_mut().db_mut();
    handle.set_transparent_ledger_mode(PrivateShadow);
    assert!(promote(&mut st, account).is_err());
    st.wallet_mut()
        .db_mut()
        .set_transparent_ledger_mode(PrivateRequired);

    promote(&mut st, account).unwrap();
}

#[test]
fn a_failed_promotion_changes_nothing_across_a_reopen() {
    let (mut st, account, _, _) = ready_wallet();
    conn(&st)
        .execute_batch(
            "CREATE TEMP TRIGGER fail_activation BEFORE INSERT ON tpir_active_accounts
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let before = production_dump(conn(&st));
    let windows = count(&st, "tpir_candidate_windows");
    assert!(matches!(
        promote(&mut st, account),
        Err(SqliteClientError::DbError(_))
    ));
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(count(&st, "tpir_candidate_windows"), windows);
    assert_eq!(reader_version(&st), 4, "qualification required version 4");

    // A fresh connection sees the same pre-promotion state.
    let reopened = crate::WalletDb::for_path(
        st.wallet().data_file_path(),
        *st.network(),
        crate::testing::db::test_clock(),
        crate::testing::db::test_rng(),
    )
    .unwrap()
    .with_transparent_ledger_mode(PrivateRequired);
    assert_eq!(production_dump(&reopened.conn), before);
    assert_eq!(
        reopened.transparent_watch_set(account).unwrap().lifecycle,
        AccountLifecycle::Candidate
    );
}

/// Covers every watched address of `account` through the target from `revision`, adding
/// `receives`, and repeats while the window grows.
fn cover(
    st: &mut State,
    account: AccountUuid,
    revision: &RecoveryRevision,
    receives: Vec<ReceiveEvent>,
) {
    let mut receives = Some(receives);
    loop {
        let ws = watch(st, account);
        let mut c = commit(&ws);
        c.revision = revision.clone();
        c.receives = receives.take().unwrap_or_default();
        c.coverage = full_coverage(&ws);
        if !apply(st, c).unwrap().window_grew {
            break;
        }
    }
}

/// A shadow wallet whose test account made a shielded-funded payment to its own transparent
/// address, stored with its raw bytes. Returns the transaction and that output's index.
fn local_payment_to_self() -> (
    State,
    AccountUuid,
    TransparentAddress,
    zcash_primitives::transaction::TxId,
    u32,
) {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = st.test_account().cloned().unwrap();
    assert_eq!(account.id(), accounts[0]);

    // A shielded-funded payment to the account's own transparent address.
    let dfvk = account.usk().sapling().to_diversifiable_full_viewing_key();
    let (height, _, _) = st.generate_next_block(
        &dfvk,
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(200_000),
    );
    st.scan_cached_blocks(height, 1);
    let taddr = external(&watch(&st, account.id()));
    let request = zip321::TransactionRequest::new(vec![zip321::Payment::without_memo(
        zcash_keys::address::Address::Transparent(taddr).to_zcash_address(st.network()),
        Zatoshis::const_from_u64(50_000),
    )])
    .unwrap();
    let change = zcash_client_backend::data_api::testing::single_output_change_strategy(
        zcash_client_backend::fees::StandardFeeRule::Zip317,
        None,
        zcash_protocol::ShieldedPool::Sapling,
    );
    let proposal = st
        .propose_transfer_with_policy(
            account.id(),
            &zcash_client_backend::data_api::wallet::input_selection::GreedyInputSelector::new(),
            &change,
            request,
            ConfirmationsPolicy::MIN,
            &Default::default(),
        )
        .unwrap();
    let txid = st
        .create_proposed_transactions::<std::convert::Infallible, _, std::convert::Infallible, _>(
            account.usk(),
            zcash_client_backend::wallet::OvkPolicy::Sender,
            &proposal,
        )
        .unwrap()[0];
    let output_index = st
        .wallet()
        .get_transaction(txid)
        .unwrap()
        .unwrap()
        .transparent_bundle()
        .unwrap()
        .vout
        .iter()
        .position(|out| out.script_pubkey() == &taddr.script().into())
        .unwrap() as u32;
    (st, account.id(), taddr, txid, output_index)
}

#[test]
fn projection_joins_a_local_transaction_and_keeps_its_details() {
    let (mut st, account, taddr, txid, output_index) = local_payment_to_self();
    let details = |st: &State| -> (bool, Option<i64>, Option<String>) {
        conn(st)
            .query_row(
                "SELECT raw IS NOT NULL, fee, created FROM transactions WHERE txid = ?1",
                [txid.as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    };
    let before = details(&st);
    assert!(before.0 && before.1.is_some() && before.2.is_some());

    // The private source reports the output as mined.
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let recovered = ReceiveEvent {
        outpoint: OutPoint::new(*txid.as_ref(), output_index),
        address: taddr,
        value: Zatoshis::const_from_u64(50_000),
        coinbase: false,
        mined_height: below_target(&ws, 0),
    };
    cover(&mut st, account, &fixture, vec![recovered.clone()]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, account).unwrap();

    // One transaction row, its local details intact, now placed; the output has both origins.
    assert_eq!(details(&st), before);
    assert_eq!(
        st.wallet().get_tx_height(txid).unwrap(),
        Some(recovered.mined_height)
    );
    assert_eq!(
        super::super::output_origins(conn(&st), &recovered.outpoint),
        vec![1, 2]
    );
}

/// Promotes `account` after a qualified fixture recovers `receives` and `spends` completely,
/// returning the promotion's integrity failure.
fn promotion_integrity_failure(
    st: &mut State,
    account: AccountUuid,
    receives: Vec<ReceiveEvent>,
    spends: Vec<SpendEvent>,
) -> IntegrityFailure {
    let fixture = revision(1, true);
    let ws = watch(st, account);
    let mut c = commit(&ws);
    c.receives = receives;
    c.spends = spends;
    c.coverage = full_coverage(&ws);
    apply(st, c).unwrap();
    cover(st, account, &fixture, vec![]);
    qualify(st, &fixture);
    set_policy(st, PrivateRequired);
    match promote(st, account) {
        Err(SqliteClientError::TransparentLedgerCommitRejected(CommitRejection::Integrity(
            failure,
        ))) => failure,
        other => panic!("expected an integrity failure, got {other:?}"),
    }
}

#[test]
fn a_receive_the_stored_transaction_lacks_is_an_integrity_failure() {
    let (mut st, account, taddr, txid, output_index) = local_payment_to_self();
    let ws = watch(&st, account);
    // The stored transaction has no transparent output after the payment's.
    let phantom = ReceiveEvent {
        outpoint: OutPoint::new(*txid.as_ref(), output_index + 1),
        address: taddr,
        value: Zatoshis::const_from_u64(50_000),
        coinbase: false,
        mined_height: below_target(&ws, 0),
    };
    assert_eq!(
        promotion_integrity_failure(&mut st, account, vec![phantom.clone()], vec![]),
        IntegrityFailure::ProjectionContent(phantom.outpoint)
    );
    assert_eq!(lifecycle(&st, account), AccountLifecycle::Candidate);
}

#[test]
fn a_spend_the_stored_transaction_lacks_is_an_integrity_failure() {
    let (mut st, account, taddr, txid, output_index) = local_payment_to_self();
    let ws = watch(&st, account);
    let paid = ReceiveEvent {
        outpoint: OutPoint::new(*txid.as_ref(), output_index),
        address: taddr,
        value: Zatoshis::const_from_u64(50_000),
        coinbase: false,
        mined_height: below_target(&ws, 1),
    };
    // A shielded-funded transaction has no transparent inputs, so it cannot spend the payment.
    let phantom = SpendEvent {
        spending_txid: txid,
        input_index: 0,
        prevout: paid.outpoint.clone(),
        prevout_address: taddr,
        mined_height: below_target(&ws, 1),
    };
    assert_eq!(
        promotion_integrity_failure(&mut st, account, vec![paid], vec![phantom]),
        IntegrityFailure::SpendContent {
            spending_txid: txid,
            input_index: 0,
        }
    );
}

#[test]
fn coinbase_receives_keep_their_maturity_rule() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let coinbase = ReceiveEvent {
        coinbase: true,
        ..receive(1, external(&ws), 90_000, below_target(&ws, 3))
    };
    cover(&mut st, account, &fixture, vec![coinbase.clone()]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, account).unwrap();

    let tx_index: Option<u32> = conn(&st)
        .query_row(
            "SELECT tx_index FROM transactions WHERE txid = ?1",
            [coinbase.outpoint.hash()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tx_index, Some(0));
    let authorized = snapshot(&st, account).authorized.unwrap();
    assert_eq!(authorized.regular.total(), Zatoshis::ZERO);
    assert_eq!(
        authorized.coinbase.total(),
        Zatoshis::const_from_u64(90_000)
    );
    assert_eq!(authorized.coinbase.spendable_value(), Zatoshis::ZERO);
}

#[test]
fn unmined_legacy_outputs_do_not_enter_the_authorized_balance() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let taddr = external(&ws);
    // Public discovery saw an unmined output; the private ledger cannot vouch for it.
    let mempool = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
        OutPoint::new([0xbb; 32], 0),
        transparent::bundle::TxOut::new(Zatoshis::const_from_u64(33_000), taddr.script().into()),
        None,
        Some(account),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .db_mut()
        .put_received_transparent_utxo(&mempool)
        .unwrap();
    let recovered = receive(1, taddr, 40_000, below_target(&ws, 3));
    cover(&mut st, account, &fixture, vec![recovered]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, account).unwrap();

    let authorized = snapshot(&st, account).authorized.unwrap();
    assert_eq!(authorized.regular.total(), Zatoshis::const_from_u64(40_000));
    assert_eq!(
        super::super::output_origins(conn(&st), &mempool.outpoint().clone()),
        vec![0]
    );
}

/// Promotes a [`ready_wallet`] and scans two more blocks, leaving coverage one commit behind.
fn active_wallet() -> (State, AccountUuid, ReceiveEvent) {
    let (mut st, account, unspent, _) = ready_wallet();
    promote(&mut st, account).unwrap();
    scan_new_blocks(&mut st, 2);
    (st, account, unspent)
}

#[test]
fn active_commits_project_their_events_in_the_same_transaction() {
    let (mut st, account, unspent) = active_wallet();
    // Authority lapses while coverage lags the tip, keeping the ledger's amount as last-known.
    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Unavailable);
    assert_eq!(
        s.blockers,
        vec![RecoveryBlocker::Recovery(
            CandidateBlocker::IncompleteCoverage
        )]
    );
    let last_known = s.last_known.unwrap();
    assert_eq!(last_known.source, LastKnownSource::PrivateLedger);
    // The amount is evaluated at the tip, past the covered point, so it is not anchored there.
    assert!(s.covered_through.is_some());
    assert_eq!(last_known.at, None);
    assert_eq!(
        last_known.balance.regular.total(),
        Zatoshis::const_from_u64(40_000)
    );

    let ws = watch(&st, account);
    assert_eq!(ws.lifecycle, AccountLifecycle::Active);
    let fresh = receive(5, external(&ws), 60_000, below_target(&ws, 0));
    let mut c = commit(&ws);
    c.receives = vec![fresh.clone()];
    c.spends = vec![spend(6, &unspent, below_target(&ws, 1))];
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    assert_eq!(
        super::super::output_origins(conn(&st), &fresh.outpoint),
        vec![2]
    );
    assert_eq!(spend_count(&st, &unspent.outpoint), 1);
    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Private);
    assert_eq!(
        s.authorized.unwrap().regular.total(),
        Zatoshis::const_from_u64(60_000)
    );
}

#[test]
fn active_window_growth_uses_the_wallets_own_addresses() {
    let (mut st, account, _) = active_wallet();
    let ws = watch(&st, account);
    let (last, index) = last_external(&ws);
    let addresses = count(&st, "addresses");
    let mut c = commit(&ws);
    c.receives = vec![receive(5, last, 60_000, below_target(&ws, 0))];
    assert!(apply(&mut st, c).unwrap().window_grew);
    assert!(count(&st, "addresses") > addresses);
    assert_eq!(count(&st, "tpir_candidate_windows"), 0);
    let (_, grown) = last_external(&watch(&st, account));
    assert!(grown > index);
}

#[test]
fn an_active_account_accepts_only_qualified_revisions() {
    let (mut st, account, _) = active_wallet();
    let ws = watch(&st, account);
    let next = revision(2, true);
    let mut c = commit(&ws);
    c.revision = next.clone();
    c.receives = vec![receive(5, external(&ws), 60_000, below_target(&ws, 0))];
    c.coverage = full_coverage(&ws);
    let before = production_dump(conn(&st));
    let revisions = count(&st, "tpir_revisions");
    assert_eq!(
        rejection(apply(&mut st, c.clone())),
        CommitRejection::Refused(RefusedCommit::UnqualifiedRevision)
    );
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(count(&st, "tpir_revisions"), revisions);

    qualify(&mut st, &next);
    apply(&mut st, c).unwrap();
    assert_eq!(
        snapshot(&st, account).authority,
        TransparentAuthority::Private
    );
}

#[test]
fn a_projection_conflict_is_an_integrity_failure() {
    let (mut st, account, _) = active_wallet();
    let ws = watch(&st, account);
    let fresh = receive(5, external(&ws), 60_000, below_target(&ws, 0));
    // The wallet already holds the transaction at another height.
    conn(&st)
        .execute(
            "INSERT INTO transactions (txid, mined_height, min_observed_height)
             VALUES (?1, ?2, ?2)",
            rusqlite::params![fresh.outpoint.hash(), u32::from(below_target(&ws, 4))],
        )
        .unwrap();
    let before = production_dump(conn(&st));
    let events = count(&st, "tpir_receive_events");
    let mut c = commit(&ws);
    c.receives = vec![fresh.clone()];
    c.coverage = full_coverage(&ws);
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Integrity(IntegrityFailure::TransactionPlacement(
            *fresh.outpoint.txid()
        ))
    );
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(count(&st, "tpir_receive_events"), events);
    assert_eq!(quarantined_accounts(&st), vec![account]);
    assert!(
        snapshot(&st, account)
            .blockers
            .contains(&RecoveryBlocker::Quarantined)
    );
}

#[test]
fn a_spend_conflicting_with_a_mined_wallet_spend_is_an_integrity_failure() {
    let (mut st, account, unspent) = active_wallet();
    let ws = watch(&st, account);
    // The wallet already holds another mined transaction spending the ledger output.
    conn(&st)
        .execute_batch(&format!(
            "INSERT INTO transactions (txid, mined_height, min_observed_height)
             VALUES (x'{txid}', {height}, {height});
             INSERT INTO transparent_received_output_spends (transparent_received_output_id, transaction_id)
             SELECT o.id, (SELECT id_tx FROM transactions WHERE txid = x'{txid}')
             FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = x'{prevout}' AND o.output_index = {index};",
            txid = hex::encode([0xcd; 32]),
            height = u32::from(below_target(&ws, 2)),
            prevout = hex::encode(unspent.outpoint.hash()),
            index = unspent.outpoint.n(),
        ))
        .unwrap();
    assert_eq!(spend_count(&st, &unspent.outpoint), 1);
    let before = production_dump(conn(&st));
    let mut c = commit(&ws);
    c.spends = vec![spend(7, &unspent, below_target(&ws, 1))];
    c.coverage = full_coverage(&ws);
    assert_eq!(
        rejection(apply(&mut st, c)),
        CommitRejection::Integrity(IntegrityFailure::ConflictingSpends(
            unspent.outpoint.clone()
        ))
    );
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(quarantined_accounts(&st), vec![account]);
}

#[test]
fn a_superseded_provisional_revision_withdraws_its_projection() {
    let (mut st, account, unspent) = active_wallet();
    let provisional = revision(2, false);
    qualify(&mut st, &provisional);
    let ws = watch(&st, account);
    let fresh = receive(5, external(&ws), 60_000, below_target(&ws, 0));
    let also_public = receive(7, external(&ws), 5_000, below_target(&ws, 0));
    let mut c = commit(&ws);
    c.revision = provisional;
    c.receives = vec![fresh.clone(), also_public.clone()];
    c.spends = vec![spend(6, &unspent, below_target(&ws, 1))];
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    assert_eq!(
        super::super::output_origins(conn(&st), &fresh.outpoint),
        vec![2]
    );
    assert_eq!(spend_count(&st, &unspent.outpoint), 1);
    // Public discovery recorded one of the outputs as well.
    conn(&st)
        .execute(
            "INSERT INTO tpir_output_origins (output_id, origin)
             SELECT o.id, 0 FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = ?1",
            [also_public.outpoint.hash()],
        )
        .unwrap();

    // A higher provisional lineage replaces it and reports neither event.
    let replacement = revision(3, false);
    qualify(&mut st, &replacement);
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.revision = replacement;
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    // The ledger-only output and spend link are withdrawn; the sealed revision's receive stays.
    assert_eq!(
        super::super::output_origins(conn(&st), &fresh.outpoint),
        Vec::<i64>::new()
    );
    let outputs: i64 = conn(&st)
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = ?1",
            [fresh.outpoint.hash()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outputs, 0);
    assert_eq!(spend_count(&st, &unspent.outpoint), 0);
    assert_eq!(
        super::super::spend_origins(conn(&st), &unspent.outpoint),
        Vec::<i64>::new()
    );
    // An output with another origin stays, as that evidence alone.
    assert_eq!(
        super::super::output_origins(conn(&st), &also_public.outpoint),
        vec![0]
    );
    assert_eq!(super::super::records_without_origin(conn(&st)), 0);
    let s = snapshot(&st, account);
    assert_eq!(s.authority, TransparentAuthority::Private);
    assert_eq!(s.authorized.unwrap().regular.total(), unspent.value);
}

#[test]
fn a_failed_projection_write_rolls_back_the_commit() {
    let (mut st, account, _) = active_wallet();
    conn(&st)
        .execute_batch(
            "CREATE TEMP TRIGGER fail_projection BEFORE INSERT ON tpir_output_origins
             WHEN NEW.origin = 2
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let ws = watch(&st, account);
    let mut c = commit(&ws);
    c.receives = vec![receive(5, external(&ws), 60_000, below_target(&ws, 0))];
    c.coverage = full_coverage(&ws);
    let before = production_dump(conn(&st));
    let (events, coverage) = (
        count(&st, "tpir_receive_events"),
        count(&st, "tpir_coverage"),
    );
    assert!(matches!(
        apply(&mut st, c),
        Err(SqliteClientError::DbError(_))
    ));
    assert_eq!(production_dump(conn(&st)), before);
    assert_eq!(
        (
            count(&st, "tpir_receive_events"),
            count(&st, "tpir_coverage")
        ),
        (events, coverage)
    );
    assert_eq!(count(&st, "tpir_quarantined_accounts"), 0);
}
