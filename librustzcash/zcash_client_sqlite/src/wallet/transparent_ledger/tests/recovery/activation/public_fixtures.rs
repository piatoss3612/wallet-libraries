//! Transparent-only wallets and transactions under the public policy, shared by the history,
//! funding-attribution, spend-evidence and rewind tests.
#![allow(dead_code)]

use transparent::{
    address::Script,
    bundle::{Authorized, Bundle, TxIn, TxOut},
};
use zcash_client_backend::data_api::{
    transparent_ledger::TransactionHistoryDetails, wallet::decrypt_and_store_transaction,
};
use zcash_primitives::transaction::{Transaction, TransactionData, TxVersion};
use zcash_protocol::consensus::BranchId;

use super::*;

/// An address that belongs to no wallet account.
pub(super) const EXTERNAL: TransparentAddress = TransparentAddress::PublicKeyHash([7; 20]);

/// A file-backed wallet under the public policy, holding the test account and `extra` imported
/// accounts.
pub(super) fn public_wallet(extra: u8) -> (State, Vec<AccountUuid>) {
    let mut st = wallet_state(TestDbFactory::file_backed());
    let mut accounts = vec![st.test_account().unwrap().id()];
    for seed in 0..extra {
        accounts.push(import_account(&mut st, 7 + seed));
    }
    scan_new_blocks(&mut st, 10);
    set_policy(&mut st, Public);
    (st, accounts)
}

/// The address of `account` derived at `index` in `scope`.
pub(super) fn derived(
    st: &State,
    account: AccountUuid,
    scope: TransparentKeyScope,
    index: u32,
) -> TransparentAddress {
    watch(st, account)
        .addresses
        .iter()
        .find(|w| {
            matches!(
                w.origin,
                WatchOrigin::Derived { scope: s, index: i } if s == scope && i.index() == index
            )
        })
        .unwrap()
        .address
}

pub(super) fn external_of(st: &State, account: AccountUuid) -> TransparentAddress {
    derived(st, account, TransparentKeyScope::EXTERNAL, 0)
}

pub(super) fn internal_of(st: &State, account: AccountUuid) -> TransparentAddress {
    derived(st, account, TransparentKeyScope::INTERNAL, 0)
}

/// A transparent-only transaction spending `inputs` to `outputs`.
pub(super) fn transaction(
    inputs: Vec<OutPoint>,
    outputs: Vec<(TransparentAddress, u64)>,
) -> Transaction {
    expiring_transaction(inputs, outputs, BlockHeight::from_u32(1_000_000))
}

/// A transparent-only transaction spending `inputs` to `outputs` that expires after `expiry`.
pub(super) fn expiring_transaction(
    inputs: Vec<OutPoint>,
    outputs: Vec<(TransparentAddress, u64)>,
    expiry: BlockHeight,
) -> Transaction {
    TransactionData::<zcash_primitives::transaction::Authorized>::from_parts(
        TxVersion::V5,
        BranchId::Nu5,
        0,
        expiry,
        Some(Bundle {
            vin: inputs
                .into_iter()
                .map(|p| TxIn::from_parts(p, Script::default(), u32::MAX))
                .collect(),
            vout: outputs
                .into_iter()
                .map(|(address, value)| {
                    TxOut::new(Zatoshis::const_from_u64(value), address.script().into())
                })
                .collect(),
            authorization: Authorized,
        }),
        None,
        None,
        None,
    )
    .freeze()
    .unwrap()
}

/// A transaction from an outside party paying `value` to `to`.
pub(super) fn funding(tag: u8, to: TransparentAddress, value: u64) -> Transaction {
    transaction(vec![OutPoint::new([tag; 32], 0)], vec![(to, value)])
}

pub(super) fn outpoint(tx: &Transaction, index: u32) -> OutPoint {
    OutPoint::new(*tx.txid().as_ref(), index)
}

/// Stores `tx` as payload retrieval does, mined at the chain tip.
pub(super) fn store(st: &mut State, tx: &Transaction) {
    let network = *st.network();
    let tip = st.wallet().chain_height().unwrap().unwrap();
    decrypt_and_store_transaction(&network, st.wallet_mut(), tx, Some(tip)).unwrap();
}

pub(super) fn history(
    st: &State,
    account: AccountUuid,
    tx: &Transaction,
) -> TransactionHistoryDetails<AccountUuid> {
    let mut entries = st
        .wallet()
        .db()
        .transaction_history_details(account, &[tx.txid()])
        .unwrap();
    assert_eq!(entries.len(), 1);
    entries.remove(0)
}

/// The sent outputs recorded for `tx`: sending account, output index, receiving account and value.
pub(super) fn sent_outputs(
    st: &State,
    tx: &Transaction,
) -> Vec<(AccountUuid, u32, Option<AccountUuid>, u64)> {
    conn(st)
        .prepare(
            "SELECT f.uuid, s.output_index, r.uuid, s.value
             FROM sent_notes s
             JOIN transactions t ON t.id_tx = s.transaction_id
             JOIN accounts f ON f.id = s.from_account_id
             LEFT JOIN accounts r ON r.id = s.to_account_id
             WHERE t.txid = ?1
             ORDER BY s.output_pool, s.output_index",
        )
        .unwrap()
        .query_map([tx.txid().as_ref()], |row| {
            let uuid =
                |bytes: Vec<u8>| AccountUuid::from_uuid(uuid::Uuid::from_slice(&bytes).unwrap());
            Ok((
                uuid(row.get(0)?),
                row.get(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?.map(uuid),
                row.get(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

pub(super) fn zat(value: u64) -> Zatoshis {
    Zatoshis::const_from_u64(value)
}
