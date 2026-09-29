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
}
