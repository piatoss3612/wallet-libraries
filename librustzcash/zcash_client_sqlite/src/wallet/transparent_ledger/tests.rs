use std::convert::Infallible;

use rusqlite::Connection;
use sapling::zip32::ExtendedSpendingKey;
use transparent::{
    address::TransparentAddress,
    bundle::{OutPoint, TxOut},
    keys::TransparentKeyScope,
};
use zcash_client_backend::{
    data_api::{
        Account as _, WalletRead as _, WalletWrite as _,
        testing::{AddressType, TestBuilder, TestState, single_output_change_strategy},
        wallet::{
            ConfirmationsPolicy,
            input_selection::{GreedyInputSelector, SpendPolicy, TransparentSpendPolicy},
        },
    },
    fees::{StandardFeeRule, TransparentChangePolicy},
    wallet::{OvkPolicy, WalletTransparentOutput},
};
use zcash_keys::{address::Address, keys::UnifiedAddressRequest};
use zcash_primitives::block::BlockHash;
use zcash_protocol::{ShieldedPool, local_consensus::LocalNetwork, value::Zatoshis};
use zip321::{Payment, TransactionRequest};

use crate::testing::{BlockCache, db::TestDbFactory};

const LEGACY_PUBLIC: i64 = 0;
const LOCAL_CONSTRUCTION: i64 = 1;

type State = TestState<BlockCache, crate::testing::db::TestDb, LocalNetwork>;

/// A wallet with one account whose only funds are a publicly discovered transparent UTXO.
fn funded_wallet() -> (State, TransparentAddress, OutPoint) {
    let mut st = TestBuilder::new()
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    let account = st.test_account().cloned().unwrap();
    let taddr = *st
        .wallet()
        .get_last_generated_address_matching(account.id(), UnifiedAddressRequest::AllAvailableKeys)
        .unwrap()
        .unwrap()
        .transparent()
        .unwrap();

    let not_our_key = ExtendedSpendingKey::master(&[]).to_diversifiable_full_viewing_key();
    let not_our_value = Zatoshis::const_from_u64(10_000);
    let (start, _, _) =
        st.generate_next_block(&not_our_key, AddressType::DefaultExternal, not_our_value);
    for _ in 1..10 {
        st.generate_next_block(&not_our_key, AddressType::DefaultExternal, not_our_value);
    }
    st.scan_cached_blocks(start, 10);

    let outpoint = OutPoint::fake();
    put_public_utxo(&mut st, &taddr, outpoint.clone(), 100_000);
    (st, taddr, outpoint)
}

fn put_public_utxo(st: &mut State, taddr: &TransparentAddress, outpoint: OutPoint, value: u64) {
    let account = st.test_account().cloned().unwrap();
    let height = st.wallet().chain_height().unwrap().unwrap();
    let utxo = WalletTransparentOutput::from_parts(
        outpoint,
        TxOut::new(Zatoshis::const_from_u64(value), taddr.script().into()),
        Some(height),
        Some(account.id()),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .put_received_transparent_utxo(&utxo)
        .unwrap();
}

fn conn(st: &State) -> &Connection {
    &st.wallet().db().conn
}

fn output_origins(conn: &Connection, outpoint: &OutPoint) -> Vec<i64> {
    conn.prepare(
        "SELECT oo.origin
         FROM tpir_output_origins oo
         JOIN transparent_received_outputs o ON o.id = oo.output_id
         JOIN transactions t ON t.id_tx = o.transaction_id
         WHERE t.txid = ?1 AND o.output_index = ?2
         ORDER BY oo.origin",
    )
    .unwrap()
    .query_map(rusqlite::params![outpoint.hash(), outpoint.n()], |row| {
        row.get(0)
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn spend_origins(conn: &Connection, outpoint: &OutPoint) -> Vec<i64> {
    conn.prepare(
        "SELECT origin FROM tpir_spend_origins
         WHERE prevout_txid = ?1 AND prevout_output_index = ?2
         ORDER BY origin",
    )
    .unwrap()
    .query_map(rusqlite::params![outpoint.hash(), outpoint.n()], |row| {
        row.get(0)
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// Returns the number of transparent outputs and spends that lack any projection origin.
pub(super) fn records_without_origin(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM transparent_received_outputs o
             WHERE NOT EXISTS (SELECT 1 FROM tpir_output_origins WHERE output_id = o.id))
          + (SELECT COUNT(*) FROM transparent_received_output_spends s
             JOIN transparent_received_outputs o ON o.id = s.transparent_received_output_id
             JOIN transactions prevout_tx ON prevout_tx.id_tx = o.transaction_id
             WHERE NOT EXISTS (
                 SELECT 1 FROM tpir_spend_origins so
                 WHERE so.spending_transaction_id = s.transaction_id
                 AND so.prevout_txid = prevout_tx.txid
                 AND so.prevout_output_index = o.output_index))
          + (SELECT COUNT(*) FROM transparent_spend_map m
             WHERE NOT EXISTS (
                 SELECT 1 FROM tpir_spend_origins so
                 WHERE so.spending_transaction_id = m.spending_transaction_id
                 AND so.prevout_txid = m.prevout_txid
                 AND so.prevout_output_index = m.prevout_output_index))",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn public_and_local_writes_record_their_origins() {
    let (mut st, _, funded) = funded_wallet();
    assert_eq!(output_origins(conn(&st), &funded), vec![LEGACY_PUBLIC]);

    // Rediscovering the same output is idempotent.
    let taddr = *st
        .wallet()
        .get_last_generated_address_matching(
            st.test_account().unwrap().id(),
            UnifiedAddressRequest::AllAvailableKeys,
        )
        .unwrap()
        .unwrap()
        .transparent()
        .unwrap();
    put_public_utxo(&mut st, &taddr, funded.clone(), 100_000);
    assert_eq!(output_origins(conn(&st), &funded), vec![LEGACY_PUBLIC]);

    // A locally constructed t->t payment spends the UTXO and creates transparent change.
    let account = st.test_account().cloned().unwrap();
    let request = TransactionRequest::new(vec![Payment::without_memo(
        Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]))
            .to_zcash_address(st.network()),
        Zatoshis::const_from_u64(40_000),
    )])
    .unwrap();
    let change_strategy =
        single_output_change_strategy(StandardFeeRule::Zip317, None, ShieldedPool::Sapling)
            .with_transparent_change_policy(TransparentChangePolicy::TransparentChangeAllowed);
    let proposal = st
        .propose_transfer_with_policy(
            account.id(),
            &GreedyInputSelector::new(),
            &change_strategy,
            request,
            ConfirmationsPolicy::MIN,
            &SpendPolicy::default().with_transparent(TransparentSpendPolicy::any_account_addr()),
        )
        .unwrap();
    let txid = st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap()
        .head;

    assert_eq!(spend_origins(conn(&st), &funded), vec![LOCAL_CONSTRUCTION]);
    let tx = st.wallet().get_transaction(txid).unwrap().unwrap();
    let vout = &tx.transparent_bundle().unwrap().vout;
    let recipient_script: transparent::address::Script =
        TransparentAddress::PublicKeyHash([7; 20]).script().into();
    let change_index = vout
        .iter()
        .position(|out| out.script_pubkey() != &recipient_script)
        .unwrap();
    let change = OutPoint::new(txid.into(), u32::try_from(change_index).unwrap());
    assert_eq!(output_origins(conn(&st), &change), vec![LOCAL_CONSTRUCTION]);

    // A later public observation of the local change adds legacy provenance and keeps the
    // local origin.
    let change_out = vout[change_index].clone();
    let height = st.wallet().chain_height().unwrap().unwrap();
    let observed = WalletTransparentOutput::from_parts(
        change.clone(),
        change_out,
        Some(height),
        Some(account.id()),
        Some(TransparentKeyScope::INTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .put_received_transparent_utxo(&observed)
        .unwrap();
    assert_eq!(
        output_origins(conn(&st), &change),
        vec![LEGACY_PUBLIC, LOCAL_CONSTRUCTION]
    );
    assert_eq!(records_without_origin(conn(&st)), 0);

    // Origins are removed with the records they describe.
    st.wallet_mut().delete_account(account.id()).unwrap();
    let remaining: i64 = conn(&st)
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tpir_output_origins)
                  + (SELECT COUNT(*) FROM tpir_spend_origins)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn origin_write_failure_rolls_back_the_output() {
    let (mut st, taddr, _) = funded_wallet();
    conn(&st)
        .execute_batch(
            "CREATE TEMP TRIGGER inject_origin_failure BEFORE INSERT ON tpir_output_origins
             BEGIN SELECT RAISE(ABORT, 'injected storage failure'); END;",
        )
        .unwrap();

    let account = st.test_account().cloned().unwrap();
    let height = st.wallet().chain_height().unwrap().unwrap();
    let outpoint = OutPoint::new([0x42; 32], 0);
    let utxo = WalletTransparentOutput::from_parts(
        outpoint.clone(),
        TxOut::new(Zatoshis::const_from_u64(5_000), taddr.script().into()),
        Some(height),
        Some(account.id()),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    assert!(
        st.wallet_mut()
            .put_received_transparent_utxo(&utxo)
            .is_err()
    );

    let stored: i64 = conn(&st)
        .query_row(
            "SELECT COUNT(*) FROM transactions WHERE txid = ?1",
            [outpoint.hash()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 0, "no output may be stored without its origin");
    assert_eq!(records_without_origin(conn(&st)), 0);
}

#[test]
fn outbox_creation_evidence_adds_local_origins_in_either_order() {
    use zcash_client_backend::data_api::status::TransactionStatusWrite as _;
    let (mut st, taddr, _) = funded_wallet();
    let height = st.wallet().chain_height().unwrap().unwrap();

    // Creation evidence recorded before the output is projected publicly.
    let before = OutPoint::new([0x51; 32], 0);
    st.wallet_mut()
        .db_mut()
        .record_transaction_created(
            zcash_primitives::transaction::TxId::from_bytes([0x51; 32]),
            height,
        )
        .unwrap();
    put_public_utxo(&mut st, &taddr, before.clone(), 6_000);
    assert_eq!(
        output_origins(conn(&st), &before),
        vec![LEGACY_PUBLIC, LOCAL_CONSTRUCTION]
    );

    // Creation evidence recorded after the output is projected publicly.
    let after = OutPoint::new([0x52; 32], 0);
    put_public_utxo(&mut st, &taddr, after.clone(), 7_000);
    assert_eq!(output_origins(conn(&st), &after), vec![LEGACY_PUBLIC]);
    st.wallet_mut()
        .db_mut()
        .record_transaction_created(
            zcash_primitives::transaction::TxId::from_bytes([0x52; 32]),
            height,
        )
        .unwrap();
    assert_eq!(
        output_origins(conn(&st), &after),
        vec![LEGACY_PUBLIC, LOCAL_CONSTRUCTION]
    );
    assert_eq!(records_without_origin(conn(&st)), 0);
}

#[test]
fn creation_evidence_and_local_origins_commit_together() {
    use zcash_client_backend::data_api::status::TransactionStatusWrite as _;
    let (mut st, taddr, _) = funded_wallet();
    let height = st.wallet().chain_height().unwrap().unwrap();
    let outpoint = OutPoint::new([0x61; 32], 0);
    put_public_utxo(&mut st, &taddr, outpoint.clone(), 8_000);
    conn(&st)
        .execute_batch(
            "CREATE TEMP TRIGGER inject_origin_failure BEFORE INSERT ON tpir_output_origins
             BEGIN SELECT RAISE(ABORT, 'injected storage failure'); END;",
        )
        .unwrap();
    assert!(
        st.wallet_mut()
            .db_mut()
            .record_transaction_created(
                zcash_primitives::transaction::TxId::from_bytes([0x61; 32]),
                height,
            )
            .is_err()
    );
    let target: Option<u32> = conn(&st)
        .query_row(
            "SELECT target_height FROM transactions WHERE txid = ?1",
            [outpoint.hash()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        target, None,
        "creation evidence must not outlive a failed origin write"
    );
    assert_eq!(output_origins(conn(&st), &outpoint), vec![LEGACY_PUBLIC]);
}

#[test]
fn conflicting_output_content_is_refused() {
    let (mut st, taddr, funded) = funded_wallet();
    let account = st.test_account().cloned().unwrap();
    let height = st.wallet().chain_height().unwrap().unwrap();
    let conflicting = WalletTransparentOutput::from_parts(
        funded.clone(),
        TxOut::new(Zatoshis::const_from_u64(99_999), taddr.script().into()),
        Some(height),
        Some(account.id()),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    assert!(
        st.wallet_mut()
            .put_received_transparent_utxo(&conflicting)
            .is_err()
    );
    let value: i64 = conn(&st)
        .query_row(
            "SELECT o.value_zat FROM transparent_received_outputs o
             JOIN transactions t ON t.id_tx = o.transaction_id
             WHERE t.txid = ?1 AND o.output_index = ?2",
            rusqlite::params![funded.hash(), funded.n()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, 100_000);
mod handles {
    use std::convert::Infallible;

    use tempfile::NamedTempFile;
    use transparent::{address::TransparentAddress, bundle::OutPoint};
    use zcash_client_backend::{
        data_api::{
            Account as _, CoinbaseFilter, InputSource as _, TargetValue, WalletRead as _,
            testing::{AddressType, single_output_change_strategy},
            transparent_ledger::{
                ChainPoint, CommitOutcome, CommitRejection, LastKnownSource, LedgerLifecycle,
                Placed, PromotionContext, PromotionOutcome, PromotionRejection, PublicationStatus,
                ReceiveEvent, RecoveryBlocker, RecoveryCompletion, RevisionId, SourceId,
                SourceRevision, TransparentAuthority, TransparentLedgerCommit,
                TransparentLedgerContext, TransparentLedgerMode, TransparentLedgerRead as _,
                TransparentLedgerWrite as _,
            },
            wallet::{
                ConfirmationsPolicy, TargetHeight,
                input_selection::{
                    GreedyInputSelector, LockFilter, LockedInputPolicy, SpendPolicy,
                    TransparentSpendPolicy,
                },
            },
        },
        fees::{StandardFeeRule, TransparentChangePolicy},
        wallet::OvkPolicy,
    };
    use zcash_keys::address::Address;
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::{
        ShieldedPool,
        consensus::{BlockHeight, Network},
        value::Zatoshis,
    };
    use zip321::{Payment, TransactionRequest};

    use super::{State, conn, funded_wallet};
    use crate::{
        AccountUuid, WalletDb,
        error::SqliteClientError,
        testing::db::{test_clock, test_rng},
        wallet::{
            init::WalletMigrator,
            transparent_ledger::{check_transparent_authority, set_durable_policy_for_testing},
        },
    };

    use TransparentLedgerMode::{PrivateRequired, PrivateShadow, Public};

    fn point() -> ChainPoint {
        ChainPoint {
            height: BlockHeight::from(1),
            hash: BlockHash([0; 32]),
        }
    }

    fn commit(
        mode: TransparentLedgerMode,
        policy_generation: u64,
    ) -> TransparentLedgerCommit<AccountUuid> {
        TransparentLedgerCommit {
            context: TransparentLedgerContext {
                mode,
                policy_generation,
                target: point(),
                lifecycle: LedgerLifecycle::Candidate,
                accounts: vec![],
            },
            source: SourceRevision {
                source: SourceId::new(b"source".to_vec()).unwrap(),
                revision: RevisionId::new(b"revision".to_vec()).unwrap(),
                status: PublicationStatus::Provisional,
                anchor: point(),
            },
            receives: vec![Placed {
                event: ReceiveEvent {
                    outpoint: OutPoint::new([5; 32], 0),
                    script: Default::default(),
                    value: Zatoshis::const_from_u64(1),
                    is_coinbase: false,
                },
                mined: point(),
            }],
            spends: vec![],
            coverage: vec![],
            pages: Default::default(),
        }
    }

    fn promotion(account: AccountUuid, policy_generation: u64) -> PromotionContext<AccountUuid> {
        PromotionContext {
            account,
            policy_generation,
            watch_generation: 0,
            decision_point: point(),
        }
    }

    fn ledger_rows(st: &State) -> i64 {
        conn(st)
            .query_row(
                "SELECT (SELECT COUNT(*) FROM tpir_account_state)
                      + (SELECT COUNT(*) FROM tpir_scripts)
                      + (SELECT COUNT(*) FROM tpir_receive_events)
                      + (SELECT COUNT(*) FROM tpir_spend_events)
                      + (SELECT COUNT(*) FROM tpir_event_placements)
                      + (SELECT COUNT(*) FROM tpir_event_observations)
                      + (SELECT COUNT(*) FROM tpir_coverage)
                      + (SELECT COUNT(*) FROM tpir_pending_pages)",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn meta(st: &State) -> (i64, i64) {
        conn(st)
            .query_row(
                "SELECT applied_mode, policy_generation FROM tpir_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    fn set_mode(st: &mut State, mode: TransparentLedgerMode) {
        st.wallet_mut().db_mut().set_transparent_ledger_mode(mode);
    }

    fn account_taddr(st: &State) -> (AccountUuid, TransparentAddress) {
        let account = st.test_account().unwrap().id();
        let taddr = *st
            .wallet()
            .get_last_generated_address_matching(
                account,
                zcash_keys::keys::UnifiedAddressRequest::AllAvailableKeys,
            )
            .unwrap()
            .unwrap()
            .transparent()
            .unwrap();
        (account, taddr)
    }

    /// Runs every transparent selector and returns their errors, if any.
    fn selector_errors(st: &State, outpoint: &OutPoint) -> Vec<Option<SqliteClientError>> {
        let (account, taddr) = account_taddr(st);
        let db = st.wallet().db();
        let target = TargetHeight::from(st.wallet().chain_height().unwrap().unwrap() + 1);
        let lock = || LockFilter::Policy(&LockedInputPolicy::Exclude);
        vec![
            db.get_unspent_transparent_output(outpoint, target).err(),
            db.get_spendable_transparent_outputs(
                &taddr,
                target,
                ConfirmationsPolicy::MIN,
                CoinbaseFilter::AllTransparentOutputs,
                lock(),
            )
            .err(),
            db.get_spendable_transparent_outputs_for_addresses(
                &[taddr],
                target,
                ConfirmationsPolicy::MIN,
                CoinbaseFilter::AllTransparentOutputs,
                lock(),
            )
            .err(),
            db.select_spendable_transparent_outputs(
                account,
                target,
                ConfirmationsPolicy::MIN,
                CoinbaseFilter::AllTransparentOutputs,
                None,
                TargetValue::AtLeast(Zatoshis::const_from_u64(1)),
                10,
                &StandardFeeRule::Zip317,
                lock(),
            )
            .err(),
        ]
    }

    #[test]
    fn unconfigured_handles_are_rejected_even_when_empty() {
        let file = NamedTempFile::new().unwrap();
        let mut db =
            WalletDb::for_path(file.path(), Network::TestNetwork, test_clock(), test_rng())
                .unwrap();
        WalletMigrator::new().init_or_migrate(&mut db).unwrap();

        let account = AccountUuid::from_uuid(uuid::Uuid::nil());
        assert!(matches!(
            db.transparent_ledger_mode(),
            Err(SqliteClientError::TransparentLedgerModeNotConfigured)
        ));
        assert!(matches!(
            db.transparent_ledger_snapshot(account, ConfirmationsPolicy::MIN),
            Err(SqliteClientError::TransparentLedgerModeNotConfigured)
        ));
        assert!(matches!(
            db.apply_transparent_ledger_commit(commit(Public, 0)),
            Err(SqliteClientError::TransparentLedgerModeNotConfigured)
        ));
        assert!(matches!(
            db.promote_transparent_ledger_account(promotion(account, 0)),
            Err(SqliteClientError::TransparentLedgerModeNotConfigured)
        ));
    }

    #[test]
    fn transactional_handles_inherit_and_reopened_handles_do_not() {
        let file = NamedTempFile::new().unwrap();
        let mut db =
            WalletDb::for_path(file.path(), Network::TestNetwork, test_clock(), test_rng())
                .unwrap()
                .with_transparent_ledger_mode(PrivateRequired);
        WalletMigrator::new().init_or_migrate(&mut db).unwrap();

        let inner = db
            .transactionally(|wdb| wdb.transparent_ledger_mode())
            .unwrap();
        assert_eq!(inner, PrivateRequired);
        let inner = db
            .transactionally_with_extension(|wdb, _| wdb.transparent_ledger_mode())
            .unwrap();
        assert_eq!(inner, PrivateRequired);

        let reopened =
            WalletDb::for_path(file.path(), Network::TestNetwork, test_clock(), test_rng())
                .unwrap();
        assert!(matches!(
            reopened.transparent_ledger_mode(),
            Err(SqliteClientError::TransparentLedgerModeNotConfigured)
        ));
    }

    #[test]
    fn snapshot_reports_authority_by_mode() {
        let (mut st, _, _) = funded_wallet();
        let (account, _) = account_taddr(&st);
        let snapshot = |st: &State| {
            st.wallet()
                .db()
                .transparent_ledger_snapshot(account, ConfirmationsPolicy::MIN)
                .unwrap()
        };

        for mode in [Public, PrivateShadow] {
            set_mode(&mut st, mode);
            let s = snapshot(&st);
            assert_eq!(s.mode, mode);
            assert_eq!(s.authority, TransparentAuthority::Public);
            let authorized = s.authorized.unwrap();
            assert_eq!(
                authorized.regular.spendable_value(),
                Zatoshis::const_from_u64(100_000)
            );
            assert_eq!(authorized.coinbase.total(), Zatoshis::ZERO);
            assert_eq!(s.last_known, None);
            assert_eq!(s.completion, RecoveryCompletion::NotApplicable);
            assert!(s.blockers.is_empty());
            assert_eq!(
                (s.target, s.covered_through, s.settled_through),
                (None, None, None)
            );
            assert_eq!(s.recovered_net, None);
        }

        // Private authority is unavailable: the public amount is shown as last-known legacy
        // evidence only, with no verified anchor, never as an authorized balance.
        set_mode(&mut st, PrivateRequired);
        let s = snapshot(&st);
        assert_eq!(s.authority, TransparentAuthority::Unavailable);
        assert_eq!(s.authorized, None);
        let last_known = s.last_known.unwrap();
        assert_eq!(last_known.source, LastKnownSource::LegacyPublic);
        assert_eq!(last_known.at, None);
        assert_eq!(
            last_known.balance.regular.spendable_value(),
            Zatoshis::const_from_u64(100_000)
        );
        assert_eq!(s.completion, RecoveryCompletion::Blocked);
        assert_eq!(
            s.blockers,
            vec![RecoveryBlocker::PrivateRecoveryUnavailable]
        );
        assert_eq!((s.covered_through, s.recovered_net), (None, None));
    }

    #[test]
    fn private_required_blocks_transparent_inputs_but_not_shielded_spends() {
        let (mut st, _, funded) = funded_wallet();
        assert!(selector_errors(&st, &funded).iter().all(Option::is_none));

        // A transparent payment proposed while public authority applied.
        let account = st.test_account().cloned().unwrap();
        let t2t = TransactionRequest::new(vec![Payment::without_memo(
            Address::Transparent(TransparentAddress::PublicKeyHash([7; 20]))
                .to_zcash_address(st.network()),
            Zatoshis::const_from_u64(40_000),
        )])
        .unwrap();
        let change_strategy =
            single_output_change_strategy(StandardFeeRule::Zip317, None, ShieldedPool::Sapling)
                .with_transparent_change_policy(TransparentChangePolicy::TransparentChangeAllowed);
        let stale_proposal = st
            .propose_transfer_with_policy(
                account.id(),
                &GreedyInputSelector::new(),
                &change_strategy,
                t2t,
                ConfirmationsPolicy::MIN,
                &SpendPolicy::default()
                    .with_transparent(TransparentSpendPolicy::any_account_addr()),
            )
            .unwrap();

        set_mode(&mut st, PrivateRequired);
        for error in selector_errors(&st, &funded) {
            assert!(matches!(
                error,
                Some(SqliteClientError::TransparentAuthorityUnavailable)
            ));
        }

        // Consuming the stale proposal is rejected before anything is stored.
        let transactions = |st: &State| -> i64 {
            conn(st)
                .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))
                .unwrap()
        };
        let before = transactions(&st);
        assert!(
            st.create_proposed_transactions::<Infallible, _, Infallible, _>(
                account.usk(),
                OvkPolicy::Sender,
                &stale_proposal,
            )
            .is_err()
        );
        assert_eq!(transactions(&st), before);

        // Shielded funds remain spendable, including to the wallet's own transparent address.
        let dfvk = account.usk().sapling().to_diversifiable_full_viewing_key();
        let (height, _, _) = st.generate_next_block(
            &dfvk,
            AddressType::DefaultExternal,
            Zatoshis::const_from_u64(200_000),
        );
        st.scan_cached_blocks(height, 1);
        let (_, taddr) = account_taddr(&st);
        let unshield = TransactionRequest::new(vec![Payment::without_memo(
            Address::Transparent(taddr).to_zcash_address(st.network()),
            Zatoshis::const_from_u64(50_000),
        )])
        .unwrap();
        let change_strategy =
            single_output_change_strategy(StandardFeeRule::Zip317, None, ShieldedPool::Sapling);
        let proposal = st
            .propose_transfer_with_policy(
                account.id(),
                &GreedyInputSelector::new(),
                &change_strategy,
                unshield,
                ConfirmationsPolicy::MIN,
                &SpendPolicy::default(),
            )
            .unwrap();
        st.create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();
    }

    #[test]
    fn commits_and_promotion_are_rejected_without_writes() {
        let (mut st, _, _) = funded_wallet();
        let (account, _) = account_taddr(&st);
        let origins_before: i64 = conn(&st)
            .query_row("SELECT COUNT(*) FROM tpir_output_origins", [], |row| {
                row.get(0)
            })
            .unwrap();

        for mode in [Public, PrivateShadow, PrivateRequired] {
            set_mode(&mut st, mode);
            let db = st.wallet_mut().db_mut();
            let other = if mode == Public {
                PrivateShadow
            } else {
                Public
            };
            assert_eq!(
                db.apply_transparent_ledger_commit(commit(other, 0))
                    .unwrap(),
                CommitOutcome::Rejected(CommitRejection::ModeMismatch)
            );
            assert_eq!(
                db.apply_transparent_ledger_commit(commit(mode, 5)).unwrap(),
                CommitOutcome::Rejected(CommitRejection::StalePolicy)
            );
            assert_eq!(
                db.apply_transparent_ledger_commit(commit(mode, 0)).unwrap(),
                CommitOutcome::Rejected(CommitRejection::Unavailable)
            );
            assert_eq!(
                db.promote_transparent_ledger_account(promotion(account, 1))
                    .unwrap(),
                PromotionOutcome::Rejected(PromotionRejection::StalePolicy)
            );
            assert_eq!(
                db.promote_transparent_ledger_account(promotion(account, 0))
                    .unwrap(),
                PromotionOutcome::Rejected(PromotionRejection::Unavailable)
            );
        }

        assert_eq!(ledger_rows(&st), 0);
        assert_eq!(meta(&st), (0, 0));
        let origins_after: i64 = conn(&st)
            .query_row("SELECT COUNT(*) FROM tpir_output_origins", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(origins_after, origins_before);
    }

    #[test]
    fn durable_private_policy_is_never_weakened() {
        let (mut st, _, funded) = funded_wallet();
        let (account, _) = account_taddr(&st);
        set_durable_policy_for_testing(conn(&st), PrivateRequired, 1).unwrap();

        for mode in [Public, PrivateShadow] {
            set_mode(&mut st, mode);
            let conflict = |e: &SqliteClientError| {
                matches!(
                    e,
                    SqliteClientError::TransparentLedgerPolicyConflict {
                        configured: Some(m),
                        applied: PrivateRequired,
                    } if *m == mode
                )
            };
            let db = st.wallet().db();
            assert!(conflict(&db.transparent_ledger_mode().unwrap_err()));
            assert!(conflict(
                &db.transparent_ledger_snapshot(account, ConfirmationsPolicy::MIN)
                    .unwrap_err()
            ));
            for error in selector_errors(&st, &funded) {
                assert!(conflict(&error.unwrap()));
            }
            assert!(conflict(
                &st.wallet_mut()
                    .db_mut()
                    .apply_transparent_ledger_commit(commit(mode, 1))
                    .unwrap_err()
            ));
        }
        // An unconfigured handle is blocked too.
        assert!(matches!(
            check_transparent_authority(conn(&st), None),
            Err(SqliteClientError::TransparentLedgerPolicyConflict {
                configured: None,
                applied: PrivateRequired,
            })
        ));

        // A matching handle operates, with transparent inputs still unavailable.
        set_mode(&mut st, PrivateRequired);
        assert_eq!(
            st.wallet().db().transparent_ledger_mode().unwrap(),
            PrivateRequired
        );
        assert!(matches!(
            selector_errors(&st, &funded)[0],
            Some(SqliteClientError::TransparentAuthorityUnavailable)
        ));

        // Nothing weakened the stored policy.
        assert_eq!(meta(&st), (2, 1));
    }
}
