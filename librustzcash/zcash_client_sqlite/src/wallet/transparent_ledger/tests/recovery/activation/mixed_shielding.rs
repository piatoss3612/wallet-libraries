//! Private recovery of transparent-to-Ironwood shielding transactions.
//!
//! These fixtures exercise the whole private path for one account: transparent private recovery
//! (spend events with qualified transaction metadata), compact Ironwood scanning, Enhance PIR
//! record application, and the history read. Each owned fact is asserted separately so a
//! regression in one recovery step cannot hide behind an aggregate classification.

use orchard::note_encryption::IronwoodNoteEncryption;
use zcash_client_backend::data_api::{
    enhance_pir::{
        EnhancePirBatchResult, EnhancePirRead as _, EnhancePirRequest, EnhancePirStoreResult,
        EnhancePirWork, EnhancePirWrite as _, EnhanceRecord, EnhanceRecordParts,
        EnhanceTransactionMetadata, EnhancementMode, TransactionEnhancementWork,
    },
    testing::{IronwoodFvk, orchard::OrchardPoolTester, pool::ShieldedPoolTester},
    transparent_ledger::{
        AggregatePayment, DetailCompleteness, EffectCompleteness, FeeState, HistoryClassification,
        PoolEffect, PrivateTransparentDetail, TransactionHistoryDetails, TransactionMetadata,
        WholeTransactionFee,
    },
};
use zcash_protocol::{PoolType, local_consensus::LocalNetwork};

use super::*;

const FEE: u64 = 20_000;
const MEMO: [u8; 512] = [0xf6; 512];

/// One accounting shape from the reported mainnet transactions.
struct Shape {
    inputs: [u64; 2],
    shielded: u64,
}

/// Case A (mined at 3,498,120) and case B (mined at 3,506,624).
const CASES: [Shape; 2] = [
    Shape {
        inputs: [120_000, 80_000],
        shielded: 180_000,
    },
    Shape {
        inputs: [300_000, 120_000],
        shielded: 400_000,
    },
];

fn zat(value: u64) -> Zatoshis {
    Zatoshis::const_from_u64(value)
}

fn ironwood_network() -> LocalNetwork {
    let activation = BlockHeight::from_u32(100_000);
    LocalNetwork {
        nu6: Some(activation),
        nu6_1: Some(activation),
        nu6_2: Some(activation),
        nu6_3: Some(activation),
        ..TestBuilder::<(), ()>::DEFAULT_NETWORK
    }
}

/// The recovered state of one shielding transaction.
struct Shielding {
    st: State,
    account: AccountUuid,
    txid: TxId,
    tx_ref: i64,
    receives: [ReceiveEvent; 2],
}

/// A promoted `PrivateRequired` account under `PrivateIronwood` enhancement whose two recovered
/// transparent outputs fund `shape`'s shielding transaction: its Ironwood output to the account's
/// internal address is compact-scanned, and private transparent recovery publishes both spends
/// with `metadata`. Enhance PIR has not run yet.
fn shielding(shape: &Shape, metadata: Option<TransactionMetadata>) -> Shielding {
    let mut st = TestBuilder::new()
        .with_network(ironwood_network())
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    scan_new_blocks(&mut st, 10);
    set_policy(&mut st, PrivateShadow);
    let account = st.test_account().unwrap().id();
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let receives = [
        receive(0x51, external(&ws), shape.inputs[0], below_target(&ws, 4)),
        receive(0x52, external(&ws), shape.inputs[1], below_target(&ws, 3)),
    ];
    cover(&mut st, account, &fixture, receives.to_vec());
    assert_eq!(recovery(&st, account).blockers, vec![]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, account).unwrap();
    st.wallet_mut()
        .db_mut()
        .set_enhancement_mode(EnhancementMode::PrivateIronwood);

    // The shielding transaction's only owned shielded effect: its Ironwood output to the
    // account's internal address. Compact scanning finds it without its memo.
    let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(&st));
    let (height, _, _) = st.generate_next_block(&fvk, AddressType::Internal, zat(shape.shielded));
    st.scan_cached_blocks(height, 1);
    scan_new_blocks(&mut st, 2);
    let (tx_ref, txid): (i64, [u8; 32]) = conn(&st)
        .query_row(
            "SELECT id_tx, txid FROM transactions WHERE mined_height = ?1",
            [u32::from(height)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let txid = TxId::from_bytes(txid);
    // The fake block's only transaction occupies index 0, the coinbase position; a transaction
    // spending transparent inputs follows the coinbase.
    conn(&st)
        .execute(
            "UPDATE transactions SET tx_index = 1 WHERE id_tx = ?1",
            [tx_ref],
        )
        .unwrap();

    // Private transparent recovery publishes both owned inputs of the same transaction.
    let ws = watch(&st, account);
    let spends = receives
        .iter()
        .enumerate()
        .map(|(index, prevout)| SpendEvent {
            metadata,
            spending_txid: txid,
            input_index: u32::try_from(index).unwrap(),
            prevout: prevout.outpoint.clone(),
            prevout_address: prevout.address,
            mined_height: height,
        })
        .collect();
    let mut c = commit(&ws);
    c.revision = fixture;
    c.spends = spends;
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    Shielding {
        st,
        account,
        txid,
        tx_ref,
        receives,
    }
}

fn reported_metadata() -> TransactionMetadata {
    TransactionMetadata {
        fee: WholeTransactionFee::Exact(zat(FEE)),
        transparent_input_count: 2,
        has_shielded_components: true,
    }
}

fn history(st: &State, account: AccountUuid, txid: TxId) -> TransactionHistoryDetails {
    let mut entries = st
        .wallet()
        .db()
        .transaction_history_details(account, &[txid])
        .unwrap();
    assert_eq!(entries.len(), 1);
    entries.remove(0)
}

fn effect(entry: &TransactionHistoryDetails, pool: PoolType) -> PoolEffect {
    *entry.effects.iter().find(|e| e.pool == pool).unwrap()
}

/// The private queries the wallet currently asks for.
fn private_queries(st: &State) -> Vec<EnhancePirRequest> {
    st.wallet()
        .transaction_enhancement_work()
        .unwrap()
        .into_iter()
        .filter_map(|work| match work {
            TransactionEnhancementWork::Private(EnhancePirWork::Query(request)) => Some(request),
            TransactionEnhancementWork::Public(request) => {
                panic!(
                    "PrivateRequired exposed public payload work for {}",
                    request.txid()
                )
            }
            _ => None,
        })
        .collect()
}

/// The service's record for the account's received action: the authentic ciphertext of the
/// scanned note, the service's transparent shape flags, and its transaction metadata.
fn record(st: &State, request: EnhancePirRequest, fee: Option<u64>) -> EnhanceRecord {
    let pending =
        crate::wallet::enhance_pir::pending(st.wallet().conn(), st.network(), request.position())
            .unwrap()
            .expect("the received note awaits its memo");
    let encryptor = IronwoodNoteEncryption::new(None, pending.note, MEMO);
    EnhanceRecord::from_parts(EnhanceRecordParts {
        enc_ciphertext_suffix: encryptor.encrypt_note_plaintext()[52..].try_into().unwrap(),
        cv_net: [0; 32],
        out_ciphertext: [0; 80],
        has_transparent_inputs: true,
        has_transparent_outputs: false,
        metadata: EnhanceTransactionMetadata::new(0, fee).unwrap(),
    })
}

fn apply_records(
    st: &mut State,
    records: &[(EnhancePirRequest, EnhanceRecord)],
) -> Vec<EnhancePirStoreResult> {
    match st
        .wallet_mut()
        .db_mut()
        .apply_ironwood_enhance_records(records)
        .unwrap()
    {
        EnhancePirBatchResult::Committed(results) => results,
        rejected => panic!("unexpected batch rejection {rejected:?}"),
    }
}

/// Facts stored for the transaction: (route, fee, received memo, raw present).
fn stored(st: &State, tx_ref: i64) -> (Option<i64>, Option<i64>, Option<Vec<u8>>, bool) {
    st.wallet()
        .conn()
        .query_row(
            "SELECT (SELECT route FROM ironwood_enhance_routing WHERE transaction_id = t.id_tx),
                    t.fee,
                    (SELECT memo FROM ironwood_received_notes WHERE transaction_id = t.id_tx),
                    t.raw IS NOT NULL
             FROM transactions t WHERE t.id_tx = ?1",
            [tx_ref],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

/// Private work rows left for the transaction, across every Enhance PIR queue.
fn queued(st: &State, tx_ref: i64) -> i64 {
    st.wallet()
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM ironwood_memo_retrieval_queue q
                     JOIN ironwood_received_notes rn ON rn.id = q.received_note_id
                     WHERE rn.transaction_id = :tx)
                  + (SELECT COUNT(*) FROM ironwood_enhance_outgoing_queue WHERE transaction_id = :tx)
                  + (SELECT COUNT(*) FROM ironwood_enhance_metadata_queue WHERE transaction_id = :tx)
                  + (SELECT COUNT(*) FROM ironwood_enhance_discovery_queue WHERE transaction_id = :tx)",
            rusqlite::named_params![":tx": tx_ref],
            |row| row.get(0),
        )
        .unwrap()
}

/// Owned financial effects are recovered before, and independently of, enhancement.
fn assert_owned_effects(case: &Shielding, shape: &Shape) {
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(
        effect(&entry, PoolType::Transparent),
        PoolEffect {
            pool: PoolType::Transparent,
            received: Zatoshis::ZERO,
            spent: zat(shape.inputs[0] + shape.inputs[1]),
            completeness: EffectCompleteness::Complete,
        }
    );
    assert_eq!(
        effect(&entry, PoolType::IRONWOOD),
        PoolEffect {
            pool: PoolType::IRONWOOD,
            received: zat(shape.shielded),
            spent: Zatoshis::ZERO,
            completeness: EffectCompleteness::Complete,
        }
    );
    assert!(entry.account_movement.complete);
    assert_eq!(entry.account_movement.net(), -i128::from(FEE));
    let evidence = entry.transaction_metadata.as_ref().unwrap();
    assert_eq!(evidence.metadata, reported_metadata());
    assert_eq!(
        evidence.metadata.transparent_input_count,
        u32::try_from(case.receives.len()).unwrap()
    );
}

/// Reproduces the reported stopping point: the received memo, the whole-transaction fee, and the
/// shielding classification are all lost to the sticky private-details marker, while every owned
/// financial effect is recovered.
#[test]
fn reported_shielding_shapes_stop_at_private_details_unsupported() {
    for shape in &CASES {
        let mut case = shielding(shape, Some(reported_metadata()));
        assert_owned_effects(&case, shape);

        let queries = private_queries(&case.st);
        assert_eq!(queries.len(), 1, "one memo query for the received action");
        let request = queries[0];
        assert_eq!(request.request_id().txid(), case.txid);
        let record = record(&case.st, request, Some(FEE));
        assert_eq!(
            apply_records(&mut case.st, &[(request, record)]),
            vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
        );

        // Received memo, fee, and raw data are all missing; the queues are empty.
        assert_eq!(stored(&case.st, case.tx_ref), (Some(2), None, None, false));
        assert_eq!(queued(&case.st, case.tx_ref), 0);
        assert!(private_queries(&case.st).is_empty());

        let entry = history(&case.st, case.account, case.txid);
        // Owned financial effects survive.
        assert_owned_effects(&case, shape);
        // Sender linkage: no outgoing record of the account's own output is recovered.
        assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
        assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
        assert_eq!(entry.fee, FeeState::Unknown);
        assert_eq!(entry.classification, HistoryClassification::Provisional);
        assert_eq!(
            entry.pending_private_details,
            vec![PrivateTransparentDetail::MixedTransaction { txid: case.txid }]
        );
    }
}
