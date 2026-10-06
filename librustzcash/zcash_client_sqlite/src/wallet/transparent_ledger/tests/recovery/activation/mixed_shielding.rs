//! Private recovery of transparent-to-Ironwood shielding transactions.
//!
//! These fixtures exercise the whole private path for one account: transparent private recovery
//! (spend events with qualified transaction metadata), compact Ironwood scanning, Enhance PIR
//! record application, and the history read. Each owned fact is asserted separately so a
//! regression in one recovery step cannot hide behind an aggregate classification.
//!
//! The compact transactions and records here are synthetic. `zakura-pir-transparent`'s
//! `mixed_shielding` test qualifies the same paths with serialized transactions, records derived
//! from them by the publishers' rules, and both private services over real PIR.

use orchard::note_encryption::IronwoodNoteEncryption;
use zcash_client_backend::data_api::{
    enhance_pir::{
        EnhancePirBatchResult, EnhancePirRead as _, EnhancePirRequest, EnhancePirStoreResult,
        EnhancePirWork, EnhancePirWrite as _, EnhanceRecord, EnhanceRecordParts,
        EnhanceTransactionMetadata, EnhancementMode, TransactionEnhancementWork,
    },
    testing::{
        FakeCompactOutput, IronwoodFvk, orchard::OrchardPoolTester, pool::ShieldedPoolTester,
    },
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

/// The transparent shape flags of a shielding: transparent inputs, no transparent outputs.
const SHIELDING_FLAGS: i64 = 1;
/// Transparent inputs and transparent outputs.
const PAYING_FLAGS: i64 = 3;

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
    height: BlockHeight,
    /// The account's two recovered transparent outputs that the transaction spends.
    receives: [ReceiveEvent; 2],
    /// The qualified metadata transparent recovery publishes with the spends.
    metadata: Option<TransactionMetadata>,
}

/// A promoted `PrivateRequired` account under `PrivateIronwood` enhancement whose two recovered
/// transparent outputs fund `shape`'s shielding transaction: its Ironwood output to the account's
/// internal address is compact-scanned, and private transparent recovery publishes both spends
/// with `metadata`. Enhance PIR has not run yet.
fn shielding(shape: &Shape, metadata: Option<TransactionMetadata>) -> Shielding {
    padded_shielding(shape, metadata, None, false)
}

/// The seed of the second account that [`padded_shielding`] can fund.
const OTHER_SEED: u8 = 0x42;

/// Like [`shielding`], with `action0` (if any) as a first Ironwood action to a key the wallet
/// does not hold: a standard builder's zero-value padding, or another party's output. With
/// `other_account`, the wallet also holds a second account, which received a 50,000-zatoshi
/// Ironwood note before the shielding transaction.
fn padded_shielding(
    shape: &Shape,
    metadata: Option<TransactionMetadata>,
    action0: Option<u64>,
    other_account: bool,
) -> Shielding {
    let mut st = TestBuilder::new()
        .with_network(ironwood_network())
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    if other_account {
        import_account(&mut st, OTHER_SEED);
    }
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
    if other_account {
        let other_fvk = IronwoodFvk(
            zcash_keys::keys::UnifiedSpendingKey::from_seed(
                st.network(),
                &[OTHER_SEED; 32],
                zip32::AccountId::ZERO,
            )
            .unwrap()
            .to_unified_full_viewing_key()
            .orchard()
            .unwrap()
            .clone(),
        );
        let (funded, _, _) = st.generate_next_block_multi(&[FakeCompactOutput::new(
            &other_fvk,
            AddressType::DefaultExternal,
            zat(50_000),
        )]);
        st.scan_cached_blocks(funded, 1);
    }

    // The shielding transaction's only owned shielded effect: its Ironwood output to the
    // account's internal address. Compact scanning finds it without its memo.
    let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(&st));
    let foreign = IronwoodFvk(orchard::keys::FullViewingKey::from(
        &orchard::keys::SpendingKey::from_bytes([0x77; 32]).unwrap(),
    ));
    let mut outputs = vec![];
    if let Some(value) = action0 {
        outputs.push(FakeCompactOutput::new(
            &foreign,
            AddressType::DefaultExternal,
            zat(value),
        ));
    }
    outputs.push(FakeCompactOutput::new(
        &fvk,
        AddressType::Internal,
        zat(shape.shielded),
    ));
    let (height, _, _) = st.generate_next_block_multi(&outputs);
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
    follow_coinbase(&st, tx_ref);

    let mut case = Shielding {
        st,
        account,
        txid,
        tx_ref,
        height,
        receives,
        metadata,
    };
    publish_spends(&mut case);
    case
}

/// The fake block's only transaction occupies index 0, the coinbase position; a transaction
/// spending transparent inputs follows the coinbase. Scanning the block again resets it.
fn follow_coinbase(st: &State, tx_ref: i64) {
    conn(st)
        .execute(
            "UPDATE transactions SET tx_index = 1 WHERE id_tx = ?1",
            [tx_ref],
        )
        .unwrap();
}

/// Private transparent recovery publishes both owned inputs of the transaction at its height,
/// and covers every watched address through the current target.
fn publish_spends(case: &mut Shielding) {
    let ws = watch(&case.st, case.account);
    let spends = case
        .receives
        .iter()
        .enumerate()
        .map(|(index, prevout)| SpendEvent {
            metadata: case.metadata,
            spending_txid: case.txid,
            input_index: u32::try_from(index).unwrap(),
            prevout: prevout.outpoint.clone(),
            prevout_address: prevout.address,
            mined_height: case.height,
        })
        .collect();
    let mut c = commit(&ws);
    c.revision = revision(1, true);
    c.spends = spends;
    c.coverage = full_coverage(&ws);
    apply(&mut case.st, c).unwrap();
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

/// The private queries the wallet currently asks for. `PrivateRequired` never exposes public
/// payload work, so a transaction-ID (`GetTransaction`) request would fail the test here.
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
/// scanned note, the service's transparent shape flags (transparent inputs, and transparent
/// outputs if `outputs`), and its transaction metadata. Built while the memo is still unknown;
/// the same record answers every later query of the action.
fn shaped_record(
    st: &State,
    request: EnhancePirRequest,
    fee: Option<u64>,
    outputs: bool,
) -> EnhanceRecord {
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
        has_transparent_outputs: outputs,
        metadata: EnhanceTransactionMetadata::new(0, fee).unwrap(),
    })
}

/// A shielding's record: transparent inputs and no transparent outputs.
fn record(st: &State, request: EnhancePirRequest, fee: Option<u64>) -> EnhanceRecord {
    shaped_record(st, request, fee, false)
}

/// `record` with the same ciphertext, flags and expiry but `fee`.
fn with_fee(record: &EnhanceRecord, fee: Option<u64>) -> EnhanceRecord {
    EnhanceRecord::from_parts(EnhanceRecordParts {
        enc_ciphertext_suffix: *record.enc_ciphertext_suffix(),
        cv_net: *record.cv_net(),
        out_ciphertext: *record.out_ciphertext(),
        has_transparent_inputs: record.has_transparent_inputs(),
        has_transparent_outputs: record.has_transparent_outputs(),
        metadata: EnhanceTransactionMetadata::new(record.metadata().expiry_height(), fee).unwrap(),
    })
}

/// `record` with the same ciphertext and metadata but the transparent output flag `outputs`.
fn with_outputs(record: &EnhanceRecord, outputs: bool) -> EnhanceRecord {
    EnhanceRecord::from_parts(EnhanceRecordParts {
        enc_ciphertext_suffix: *record.enc_ciphertext_suffix(),
        cv_net: *record.cv_net(),
        out_ciphertext: *record.out_ciphertext(),
        has_transparent_inputs: record.has_transparent_inputs(),
        has_transparent_outputs: outputs,
        metadata: record.metadata(),
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

fn rejects(st: &mut State, records: &[(EnhancePirRequest, EnhanceRecord)]) -> bool {
    matches!(
        st.wallet_mut()
            .db_mut()
            .apply_ironwood_enhance_records(records)
            .unwrap(),
        EnhancePirBatchResult::Rejected { .. }
    )
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

/// The recorded transparent shape flags and the height they were validated at.
fn recorded_shape(st: &State, tx_ref: i64) -> (Option<i64>, Option<u32>) {
    st.wallet()
        .conn()
        .query_row(
            "SELECT transparent_flags, transparent_flags_height
             FROM ironwood_enhance_routing WHERE transaction_id = ?1",
            [tx_ref],
            |row| Ok((row.get(0)?, row.get(1)?)),
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

/// Rows of the metadata queue for the transaction.
fn metadata_work(st: &State, tx_ref: i64) -> i64 {
    st.wallet()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ironwood_enhance_metadata_queue WHERE transaction_id = ?1",
            [tx_ref],
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
    assert_eq!(
        entry.account_movement.net(),
        i128::from(shape.shielded) - i128::from(shape.inputs[0] + shape.inputs[1])
    );
}

/// The private queries for the shielding transaction.
fn queries_for(case: &Shielding) -> Vec<EnhancePirRequest> {
    private_queries(&case.st)
        .into_iter()
        .filter(|request| request.request_id().txid() == case.txid)
        .collect()
}

/// The one private query for the received action.
fn the_query(case: &Shielding) -> EnhancePirRequest {
    let queries = queries_for(case);
    assert_eq!(queries.len(), 1, "one query for the received action");
    queries[0]
}

/// Queries the received action's memo privately and applies the service's record for it.
fn recover_memo(case: &mut Shielding, fee: Option<u64>) -> Vec<EnhancePirStoreResult> {
    let request = the_query(case);
    let record = record(&case.st, request, fee);
    apply_records(&mut case.st, &[(request, record)])
}

/// Spend links, notes, and fee facts that must never be duplicated or lost.
fn financial_rows(st: &State, tx_ref: i64) -> (i64, i64, i64, i64) {
    st.wallet()
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM ironwood_received_notes WHERE transaction_id = :tx),
                    (SELECT COUNT(*) FROM transparent_received_output_spends WHERE transaction_id = :tx),
                    (SELECT COUNT(*) FROM tpir_spend_events e
                     JOIN transactions t ON t.txid = e.spending_txid WHERE t.id_tx = :tx),
                    (SELECT COUNT(*) FROM sent_notes WHERE transaction_id = :tx)",
            rusqlite::named_params![":tx": tx_ref],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

/// The supported shape: every transparent input is the account's, the whole-transaction fee is
/// exact and agrees across both private sources, the Enhance PIR record reports no transparent
/// outputs, and the account's spent value is exactly its Ironwood receipt plus that fee.
fn assert_reconstructed_shielding(case: &Shielding, shape: &Shape) {
    assert_owned_effects(case, shape);
    let entry = history(&case.st, case.account, case.txid);
    // The received memo is recovered, so the payment details are complete.
    assert_eq!(entry.payment_details, DetailCompleteness::Complete);
    // The account's net movement is final; whether its debit was the fee is not proven.
    assert_eq!(
        entry.classification,
        HistoryClassification::NetReconstructed
    );
    // The whole-transaction fee is exact; the account's share of it is not attributed.
    assert_eq!(
        entry.transaction_metadata.as_ref().unwrap().metadata.fee,
        WholeTransactionFee::Exact(zat(FEE))
    );
    assert_eq!(entry.fee, FeeState::Unknown);
    // No exact payment is fabricated: no outgoing record exists, only the balance.
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
    // The full transaction remains unavailable under PrivateRequired.
    assert_eq!(
        entry.pending_private_details,
        vec![PrivateTransparentDetail::MixedTransaction { txid: case.txid }]
    );
}

/// The account's movement is known, but nothing about its payment or fee is attributed.
fn assert_provisional(case: &Shielding) {
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
}

/// Both reported shapes recover their memo, shape and whole-transaction fee privately and
/// reconstruct as a net shielding: the account's transparent inputs became its Ironwood output
/// and the fee.
#[test]
fn reported_shielding_shapes_reconstruct_from_private_evidence() {
    for shape in &CASES {
        let mut case = shielding(shape, Some(reported_metadata()));
        assert_owned_effects(&case, shape);
        let before = history(&case.st, case.account, case.txid);
        assert_eq!(before.classification, HistoryClassification::Provisional);
        assert_eq!(before.payment_details, DetailCompleteness::Incomplete);
        let rows = financial_rows(&case.st, case.tx_ref);

        assert_eq!(
            recover_memo(&mut case, Some(FEE)),
            vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
        );
        // The memo, shape and whole-transaction fee are stored; the transparent details stay
        // unsupported and nothing is queued, publicly or privately.
        assert_eq!(
            stored(&case.st, case.tx_ref),
            (
                Some(2),
                Some(i64::try_from(FEE).unwrap()),
                Some(MEMO.to_vec()),
                false
            )
        );
        assert_eq!(
            recorded_shape(&case.st, case.tx_ref),
            (Some(SHIELDING_FLAGS), Some(u32::from(case.height)))
        );
        assert_eq!(queued(&case.st, case.tx_ref), 0);
        assert!(private_queries(&case.st).is_empty());
        // Sender linkage: the account's own output has no outgoing record, which only the
        // full transaction could provide. Nothing else changed.
        assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
        assert_eq!(rows.3, 0);

        assert_reconstructed_shielding(&case, shape);
    }
}

/// Another party funded a transparent input: the balance alone could hide a payment to them.
#[test]
fn another_transparent_funder_leaves_payment_and_fee_unattributed() {
    let shape = &CASES[0];
    let mut case = shielding(
        shape,
        Some(TransactionMetadata {
            transparent_input_count: 3,
            ..reported_metadata()
        }),
    );
    recover_memo(&mut case, Some(FEE));
    // The independently recoverable details are kept ...
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (
            Some(2),
            Some(i64::try_from(FEE).unwrap()),
            Some(MEMO.to_vec()),
            false
        )
    );
    // ... but the account's debit equals the fee only by the balance, so nothing is attributed.
    assert_provisional(&case);
}

/// The account's funds paid someone else besides the fee: an owned shielded output does not make
/// the transaction a shielding.
#[test]
fn an_external_payment_is_not_reconstructed_as_shielding() {
    let shape = Shape {
        inputs: [120_000, 80_000],
        shielded: 150_000,
    };
    let mut case = shielding(&shape, Some(reported_metadata()));
    recover_memo(&mut case, Some(FEE));
    assert_owned_effects(&case, &shape);
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.account_movement.net(), -50_000);
    assert_provisional(&case);
}

/// The counterexample to treating a balanced fee-sized debit as a shielding. Another party spends
/// a 50,000-zatoshi Ironwood note of its own (action 0 here, undecryptable to the wallet), and the
/// transaction pays 50,000 zatoshis to an external transparent output. The account's side is
/// exactly the reported shape: its two transparent inputs (200,000) became its 180,000 Ironwood
/// receipt and the 20,000 fee, it owns every transparent input, and both fee sources agree. Only
/// the Enhance PIR record's transparent-output flag tells the two apart.
#[test]
fn foreign_ironwood_funding_of_an_external_transparent_output_is_not_shielding() {
    let shape = &CASES[0];
    let mut case = padded_shielding(shape, Some(reported_metadata()), Some(0), false);
    let request = the_query(&case);
    let record = shaped_record(&case.st, request, Some(FEE), true);
    assert_eq!(
        apply_records(&mut case.st, &[(request, record)]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    // The memo, the fee and the shape are recorded as recovered ...
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (
            Some(2),
            Some(i64::try_from(FEE).unwrap()),
            Some(MEMO.to_vec()),
            false
        )
    );
    assert_eq!(
        recorded_shape(&case.st, case.tx_ref),
        (Some(PAYING_FLAGS), Some(u32::from(case.height)))
    );
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    // ... with the same owned effects as a shielding, which they do not make one.
    assert_owned_effects(&case, shape);
    assert_provisional(&case);
}

/// A shape that is unknown never completes the history, and a contradicting one is rejected
/// without any effect.
#[test]
fn an_unknown_or_contradicting_shape_never_completes_and_is_rejected_atomically() {
    let shape = &CASES[0];

    // Unknown: a wallet that recovered the memo and fee before shapes were recorded.
    let case = shielding(shape, Some(reported_metadata()));
    conn(&case.st)
        .execute_batch(&format!(
            "UPDATE ironwood_enhance_routing SET route = 2 WHERE transaction_id = {tx};
             UPDATE transactions SET fee = {FEE} WHERE id_tx = {tx};
             UPDATE ironwood_received_notes SET memo = x'{memo}' WHERE transaction_id = {tx};
             DELETE FROM ironwood_memo_retrieval_queue;
             DELETE FROM ironwood_enhance_metadata_queue WHERE transaction_id = {tx};",
            tx = case.tx_ref,
            memo = hex::encode(MEMO),
        ))
        .unwrap();
    assert_eq!(recorded_shape(&case.st, case.tx_ref), (None, None));
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);

    // Contradicting: a memo-only response records the shielding shape; a later response
    // reporting transparent outputs is rejected before any write, even though it carries the
    // missing fee.
    let mut case = shielding(shape, Some(reported_metadata()));
    let request = the_query(&case);
    let shielding_record = record(&case.st, request, None);
    assert_eq!(
        apply_records(&mut case.st, &[(request, shielding_record.clone())]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    let recovered = (
        stored(&case.st, case.tx_ref),
        recorded_shape(&case.st, case.tx_ref),
        queued(&case.st, case.tx_ref),
    );
    assert_eq!(
        recovered,
        (
            (Some(2), None, Some(MEMO.to_vec()), false),
            (Some(SHIELDING_FLAGS), Some(u32::from(case.height))),
            1
        )
    );
    assert_eq!(private_queries(&case.st), vec![request]);
    let contradicting = with_outputs(&with_fee(&shielding_record, Some(FEE)), true);
    assert!(rejects(&mut case.st, &[(request, contradicting)]));
    assert_eq!(
        (
            stored(&case.st, case.tx_ref),
            recorded_shape(&case.st, case.tx_ref),
            queued(&case.st, case.tx_ref),
        ),
        recovered
    );
    assert_eq!(private_queries(&case.st), vec![request]);
    assert_provisional(&case);

    // The agreeing response still completes it.
    assert_eq!(
        apply_records(
            &mut case.st,
            &[(request, with_fee(&shielding_record, Some(FEE)))]
        ),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_reconstructed_shielding(&case, shape);
}

/// Another account of the same wallet spent an Ironwood note in the transaction. The account's
/// own side is still the reported shape, but the wallet knows the transaction had another funder.
#[test]
fn known_funding_by_another_account_is_not_shielding() {
    let shape = &CASES[0];
    let mut case = padded_shielding(shape, Some(reported_metadata()), None, true);
    // Scanning linked the other account's note to the shielding transaction as spent.
    conn(&case.st)
        .execute(
            "INSERT INTO ironwood_received_note_spends (ironwood_received_note_id, transaction_id)
             SELECT rn.id, ?1 FROM ironwood_received_notes rn
             JOIN accounts a ON a.id = rn.account_id WHERE a.uuid != ?2",
            rusqlite::params![case.tx_ref, case.account.expose_uuid()],
        )
        .unwrap();
    recover_memo(&mut case, Some(FEE));
    assert_eq!(
        recorded_shape(&case.st, case.tx_ref),
        (Some(SHIELDING_FLAGS), Some(u32::from(case.height)))
    );
    assert_owned_effects(&case, shape);
    assert_provisional(&case);
}

/// The account's debit equals the network fee, but the evidence that it was the only funder, or
/// that the fee is that exact amount, is missing or contradicted: the ambiguity is retained.
#[test]
fn a_fee_sized_debit_without_sole_funding_evidence_stays_ambiguous() {
    let shape = &CASES[0];
    for (metadata, record_fee) in [
        // No qualified transaction metadata at all.
        (None, Some(FEE)),
        // The transparent publisher could not establish the fee.
        (
            Some(TransactionMetadata {
                fee: WholeTransactionFee::Unknown,
                ..reported_metadata()
            }),
            Some(FEE),
        ),
        // The two private sources disagree about the fee.
        (
            Some(TransactionMetadata {
                fee: WholeTransactionFee::Exact(zat(15_000)),
                ..reported_metadata()
            }),
            Some(FEE),
        ),
        // Enhance PIR supplies no fee, as its publisher does for every transaction with
        // transparent data: the transparent fee alone is not cross-checked.
        (Some(reported_metadata()), None),
    ] {
        let mut case = shielding(shape, metadata);
        recover_memo(&mut case, record_fee);
        let entry = history(&case.st, case.account, case.txid);
        assert_eq!(entry.account_movement.net(), -i128::from(FEE));
        assert_provisional(&case);
    }
}

/// A record without a fee yields its authenticated memo and shape, and no more. The fee stays
/// retryable by querying the same note position again; a later record with the fee completes the
/// history once, and replaying it changes nothing.
#[test]
fn memo_only_recovery_retains_fee_work_until_a_fee_completes_it_once() {
    let shape = &CASES[1];
    let mut case = shielding(shape, Some(reported_metadata()));
    let rows = financial_rows(&case.st, case.tx_ref);
    let request = the_query(&case);
    let memo_only = record(&case.st, request, None);
    assert_eq!(
        apply_records(&mut case.st, &[(request, memo_only.clone())]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (Some(2), None, Some(MEMO.to_vec()), false)
    );
    assert_eq!(
        recorded_shape(&case.st, case.tx_ref),
        (Some(SHIELDING_FLAGS), Some(u32::from(case.height)))
    );
    // Only the metadata work is left, at the same authenticated position.
    assert_eq!(queued(&case.st, case.tx_ref), 1);
    assert_eq!(metadata_work(&case.st, case.tx_ref), 1);
    assert_eq!(private_queries(&case.st), vec![request]);
    assert_provisional(&case);

    // The service answers again without a fee: nothing changes and the work stays.
    assert_eq!(
        apply_records(&mut case.st, &[(request, memo_only.clone())]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(private_queries(&case.st), vec![request]);
    assert_provisional(&case);

    // A later answer with the fee completes the metadata work.
    let with_fee = with_fee(&memo_only, Some(FEE));
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee.clone())]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (
            Some(2),
            Some(i64::try_from(FEE).unwrap()),
            Some(MEMO.to_vec()),
            false
        )
    );
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    assert!(private_queries(&case.st).is_empty());
    assert_reconstructed_shielding(&case, shape);

    // Replaying either answer is stale and changes nothing.
    for replay in [memo_only, with_fee] {
        assert_eq!(
            apply_records(&mut case.st, &[(request, replay)]),
            vec![EnhancePirStoreResult::AlreadyResolved]
        );
    }
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
    assert_reconstructed_shielding(&case, shape);
}

/// A response that contradicts stored facts, or no longer matches pending work, changes nothing.
#[test]
fn conflicting_or_stale_responses_leave_recovered_facts_intact() {
    let shape = &CASES[0];
    let mut case = shielding(shape, Some(reported_metadata()));
    let request = private_queries(&case.st)[0];
    let record = record(&case.st, request, Some(FEE));

    // A known fee that the response contradicts rejects the whole response.
    conn(&case.st)
        .execute(
            "UPDATE transactions SET fee = 25000 WHERE id_tx = ?1",
            [case.tx_ref],
        )
        .unwrap();
    assert!(rejects(&mut case.st, &[(request, record.clone())]));
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (Some(0), Some(25_000), None, false)
    );
    assert_eq!(recorded_shape(&case.st, case.tx_ref), (None, None));
    assert_eq!(private_queries(&case.st), vec![request]);

    // So does a conflicting displayed expiry.
    conn(&case.st)
        .execute_batch(&format!(
            "UPDATE transactions SET fee = NULL WHERE id_tx = {tx};
             UPDATE ironwood_enhance_routing SET history_expiry_height = 7
             WHERE transaction_id = {tx};",
            tx = case.tx_ref
        ))
        .unwrap();
    assert!(rejects(&mut case.st, &[(request, record.clone())]));
    assert_eq!(stored(&case.st, case.tx_ref), (Some(0), None, None, false));
    assert_eq!(recorded_shape(&case.st, case.tx_ref), (None, None));
    conn(&case.st)
        .execute(
            "UPDATE ironwood_enhance_routing SET history_expiry_height = NULL
             WHERE transaction_id = ?1",
            [case.tx_ref],
        )
        .unwrap();

    // The consistent response applies once; replaying it is stale and changes nothing.
    let rows = financial_rows(&case.st, case.tx_ref);
    assert_eq!(
        apply_records(&mut case.st, &[(request, record.clone())]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    let recovered = stored(&case.st, case.tx_ref);
    assert_eq!(
        apply_records(&mut case.st, &[(request, record)]),
        vec![EnhancePirStoreResult::AlreadyResolved]
    );
    assert_eq!(stored(&case.st, case.tx_ref), recovered);
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
    assert_reconstructed_shielding(&case, shape);
}

/// Returns a migrated wallet to the schema and leaf state that predates the shape columns, as a
/// database written by an earlier build has it.
fn predate_details_retry(st: &State) {
    use crate::wallet::init::migrations::CURRENT_LEAF_MIGRATIONS;
    conn(st)
        .execute_batch(
            "ALTER TABLE ironwood_enhance_routing DROP COLUMN transparent_flags_height;
             ALTER TABLE ironwood_enhance_routing DROP COLUMN transparent_flags;",
        )
        .unwrap();
    for leaf in CURRENT_LEAF_MIGRATIONS {
        conn(st)
            .execute(
                "DELETE FROM schemer_migrations WHERE id = ?1",
                [leaf.as_bytes().to_vec()],
            )
            .unwrap();
    }
}

/// Upgrades, then reopens, the wallet.
fn upgrade_and_reopen(st: &mut State) {
    use crate::wallet::init::WalletMigrator;
    for _ in 0..2 {
        WalletMigrator::new()
            .init_or_migrate(st.wallet_mut().db_mut())
            .unwrap();
    }
}

/// A database whose earlier routing cleared the memo work of a route-2 transaction resumes it on
/// upgrade, privately and without a reset.
#[test]
fn an_existing_route_two_database_resumes_private_memo_recovery_after_upgrade() {
    let shape = &CASES[1];
    let mut case = shielding(shape, Some(reported_metadata()));
    // The state the previous routing left behind: route 2, every queue cleared, memo and fee
    // unknown, recorded by a database that predates the retry migrations.
    conn(&case.st)
        .execute_batch(&format!(
            "UPDATE ironwood_enhance_routing SET route = 2 WHERE transaction_id = {tx};
             DELETE FROM ironwood_memo_retrieval_queue;
             DELETE FROM ironwood_enhance_outgoing_queue WHERE transaction_id = {tx};
             DELETE FROM ironwood_enhance_metadata_queue WHERE transaction_id = {tx};
             DELETE FROM ironwood_enhance_discovery_queue WHERE transaction_id = {tx};",
            tx = case.tx_ref
        ))
        .unwrap();
    predate_details_retry(&case.st);
    assert_eq!(stored(&case.st, case.tx_ref), (Some(2), None, None, false));
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    assert!(private_queries(&case.st).is_empty(), "stuck before upgrade");
    let rows = financial_rows(&case.st, case.tx_ref);

    // Upgrading, and reopening afterwards, queues the memo and the metadata once, both at the
    // received note's position.
    upgrade_and_reopen(&mut case.st);
    assert_eq!(queued(&case.st, case.tx_ref), 2);
    assert_eq!(metadata_work(&case.st, case.tx_ref), 1);
    assert_eq!(private_queries(&case.st).len(), 1);
    assert_eq!(stored(&case.st, case.tx_ref), (Some(2), None, None, false));
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);

    assert_eq!(
        recover_memo(&mut case, Some(FEE)),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
    assert_reconstructed_shielding(&case, shape);
}

/// A database that recovered the memo, or the memo and the fee, before fee retry and shapes
/// existed has no private work left for the transaction. Upgrading and reopening queues its
/// metadata once at the authenticated note position, keeps every recovered fact and the
/// unsupported marker, and the next answer completes it.
#[test]
fn an_upgraded_database_retries_a_known_memo_with_a_missing_fee_or_shape() {
    let shape = &CASES[0];
    for known_fee in [None, Some(FEE)] {
        let mut case = shielding(shape, Some(reported_metadata()));
        let request = the_query(&case);
        let answer = record(&case.st, request, Some(FEE));
        conn(&case.st)
            .execute_batch(&format!(
                "UPDATE ironwood_enhance_routing SET route = 2 WHERE transaction_id = {tx};
                 UPDATE transactions SET fee = {fee} WHERE id_tx = {tx};
                 UPDATE ironwood_received_notes SET memo = x'{memo}' WHERE transaction_id = {tx};
                 DELETE FROM ironwood_memo_retrieval_queue;
                 DELETE FROM ironwood_enhance_outgoing_queue WHERE transaction_id = {tx};
                 DELETE FROM ironwood_enhance_metadata_queue WHERE transaction_id = {tx};
                 DELETE FROM ironwood_enhance_discovery_queue WHERE transaction_id = {tx};",
                tx = case.tx_ref,
                fee = known_fee.map_or("NULL".to_owned(), |fee| fee.to_string()),
                memo = hex::encode(MEMO),
            ))
            .unwrap();
        predate_details_retry(&case.st);
        let before = stored(&case.st, case.tx_ref);
        let rows = financial_rows(&case.st, case.tx_ref);
        assert!(private_queries(&case.st).is_empty(), "stuck before upgrade");

        upgrade_and_reopen(&mut case.st);
        assert_eq!(queued(&case.st, case.tx_ref), 1);
        assert_eq!(metadata_work(&case.st, case.tx_ref), 1);
        assert_eq!(private_queries(&case.st), vec![request]);
        assert_eq!(stored(&case.st, case.tx_ref), before);
        assert_eq!(recorded_shape(&case.st, case.tx_ref), (None, None));
        assert_provisional(&case);

        assert_eq!(
            apply_records(&mut case.st, &[(request, answer)]),
            vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
        );
        assert_eq!(queued(&case.st, case.tx_ref), 0);
        assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
        assert_reconstructed_shielding(&case, shape);
        // Reopening again finds nothing to retry.
        upgrade_and_reopen(&mut case.st);
        assert_eq!(queued(&case.st, case.tx_ref), 0);
    }
}

/// Recovery interrupted before its write commits leaves the work queued; retrying applies it
/// once, and replayed scans do not requeue it or duplicate any fact.
#[test]
fn interrupted_recovery_retries_without_duplicates() {
    let shape = &CASES[0];
    let mut case = shielding(shape, Some(reported_metadata()));
    let rows = financial_rows(&case.st, case.tx_ref);
    // The response was fetched but never applied.
    let interrupted = private_queries(&case.st);
    assert_eq!(private_queries(&case.st), interrupted);
    assert_eq!(
        recover_memo(&mut case, Some(FEE)),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    // A replayed scan of the transaction finds nothing more to queue.
    crate::wallet::enhance_pir::route_transparent_details(
        conn(&case.st),
        Some(PrivateRequired),
        crate::TxRef(case.tx_ref),
    )
    .unwrap();
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
    assert_reconstructed_shielding(&case, shape);
}

/// Restoring public authority makes the unsupported transaction ordinary public work and drops
/// its private metadata work; returning to `PrivateRequired` withholds the public request again
/// and requeues the metadata work, privately.
#[test]
fn policy_transitions_drop_and_restore_private_fee_work() {
    let shape = &CASES[0];
    let mut case = shielding(shape, Some(reported_metadata()));
    let request = the_query(&case);
    let memo_only = record(&case.st, request, None);
    apply_records(&mut case.st, &[(request, memo_only.clone())]);
    assert_eq!(metadata_work(&case.st, case.tx_ref), 1);

    set_policy(&mut case.st, Public);
    assert_eq!(stored(&case.st, case.tx_ref).0, Some(1));
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    let work = case.st.wallet().transaction_enhancement_work().unwrap();
    assert!(
        work.iter()
            .all(|w| !matches!(w, TransactionEnhancementWork::Private(_)))
    );
    assert!(work.iter().any(|w| matches!(
        w,
        TransactionEnhancementWork::Public(r) if r.txid() == case.txid
    )));
    // A response captured under the earlier policy is stale.
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee(&memo_only, Some(FEE)))]),
        vec![EnhancePirStoreResult::AlreadyResolved]
    );

    set_policy(&mut case.st, PrivateShadow);
    set_policy(&mut case.st, PrivateRequired);
    promote(&mut case.st, case.account).unwrap();
    assert_eq!(stored(&case.st, case.tx_ref).0, Some(2));
    assert_eq!(metadata_work(&case.st, case.tx_ref), 1);
    assert_eq!(private_queries(&case.st), vec![request]);
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee(&memo_only, Some(FEE)))]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_reconstructed_shielding(&case, shape);
}

/// A rewind unmines the transaction: its captured work is stale and nothing is dispatched for it.
/// Re-mining it where it was restores the binding; a shape recorded at another placement is
/// unknown, so the history waits until the record is retrieved again at the new placement.
#[test]
fn rewinds_and_re_mining_reconsider_the_recorded_shape() {
    let shape = &CASES[0];
    let mut case = shielding(shape, Some(reported_metadata()));
    let request = the_query(&case);
    let memo_only = record(&case.st, request, None);
    apply_records(&mut case.st, &[(request, memo_only.clone())]);

    // Unmined: the captured request is stale, and no work is dispatched.
    case.st.truncate_to_height_retaining_cache(case.height - 1);
    assert!(private_queries(&case.st).is_empty());
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee(&memo_only, Some(FEE)))]),
        vec![EnhancePirStoreResult::AlreadyResolved]
    );
    assert_eq!(stored(&case.st, case.tx_ref).1, None);
    assert_eq!(
        history(&case.st, case.account, case.txid).classification,
        HistoryClassification::Provisional
    );

    // Re-mined at the same placement: the binding and the recorded shape are current again once
    // transparent recovery republishes the spends the rewind cleared.
    case.st.scan_cached_blocks(case.height, 3);
    follow_coinbase(&case.st, case.tx_ref);
    publish_spends(&mut case);
    assert_eq!(private_queries(&case.st), vec![request]);
    assert_eq!(
        recorded_shape(&case.st, case.tx_ref),
        (Some(SHIELDING_FLAGS), Some(u32::from(case.height)))
    );
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee(&memo_only, Some(FEE)))]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_reconstructed_shielding(&case, shape);

    // A shape recorded where the transaction was mined before a re-mining elsewhere (the
    // transaction keeps its current placement here; its recorded shape names the other one) is
    // stale: the history is provisional again, and the next scan of the transaction requeues the
    // metadata work at the current placement.
    conn(&case.st)
        .execute(
            "UPDATE ironwood_enhance_routing SET transparent_flags_height = ?2
             WHERE transaction_id = ?1",
            rusqlite::params![case.tx_ref, u32::from(case.height) - 1],
        )
        .unwrap();
    let entry = history(&case.st, case.account, case.txid);
    assert!(entry.account_movement.complete);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    crate::wallet::enhance_pir::route_transparent_details(
        conn(&case.st),
        Some(PrivateRequired),
        crate::TxRef(case.tx_ref),
    )
    .unwrap();
    assert_eq!(metadata_work(&case.st, case.tx_ref), 1);
    let request = the_query(&case);
    assert_eq!(
        apply_records(&mut case.st, &[(request, with_fee(&memo_only, Some(FEE)))]),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(
        recorded_shape(&case.st, case.tx_ref),
        (Some(SHIELDING_FLAGS), Some(u32::from(case.height)))
    );
    assert_reconstructed_shielding(&case, shape);
}

/// Classification follows its evidence: losing coverage or a block reverts it to provisional,
/// while the recovered memo and fee are retained for when the evidence returns.
#[test]
fn coverage_loss_and_reorg_reevaluate_the_classification() {
    let shape = &CASES[0];
    let mut case = shielding(shape, Some(reported_metadata()));
    recover_memo(&mut case, Some(FEE));
    assert_reconstructed_shielding(&case, shape);

    // A quarantined source no longer covers the account or qualifies its metadata.
    conn(&case.st)
        .execute(
            "INSERT INTO tpir_quarantined_sources (source) VALUES (?1)",
            [b"fixture".to_vec()],
        )
        .unwrap();
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.transaction_metadata, None);
    super::super::lift_quarantine(&case.st);
    assert_reconstructed_shielding(&case, shape);

    // A reorg unmines the transaction: no private work is dispatched for it, its recovered memo
    // and fee are retained, and the history is provisional.
    case.st.truncate_to_height(case.height - 1);
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.mined_height, None);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert!(private_queries(&case.st).is_empty());
    let (route, fee, memo, raw) = stored(&case.st, case.tx_ref);
    assert_eq!(
        (route, fee, memo, raw),
        (
            Some(2),
            Some(i64::try_from(FEE).unwrap()),
            Some(MEMO.to_vec()),
            false
        )
    );
}

/// The standard padded shape and the adversarial one have identical evidence. Both spend the
/// account's two transparent inputs (200,000), have no transparent outputs, a 20,000 fee, and
/// two Ironwood actions with the account's 180,000 output at action 1. In the pure shielding,
/// action 0 is the builder's zero-value padding (outgoing ciphertext encrypted to no key, spends
/// enabled by default flags). In the other, another party spends 100,000 of its own shielded
/// funds into action 0's output, leaving the pool's net inflow at 180,000. Neither action 0 is
/// decryptable or OVK-recoverable by the wallet, so neither is even queued for recovery; the
/// transparent metadata, Enhance PIR record, and owned effects agree, and so does the history.
/// The result is a net movement in both cases, never a proven self-transfer.
#[test]
fn foreign_self_balanced_shielded_participation_is_indistinguishable() {
    let shape = &CASES[0];
    let details = [Some(0), Some(100_000)].map(|action0| {
        let mut case = padded_shielding(shape, Some(reported_metadata()), action0, false);
        // Action 0 is no outgoing candidate: the account spent no Ironwood note.
        let outgoing: i64 = conn(&case.st)
            .query_row(
                "SELECT COUNT(*) FROM ironwood_enhance_outgoing_queue WHERE transaction_id = ?1",
                [case.tx_ref],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outgoing, 0);
        assert_eq!(
            recover_memo(&mut case, Some(FEE)),
            vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
        );
        let entry = history(&case.st, case.account, case.txid);
        assert_eq!(
            entry.classification,
            HistoryClassification::NetReconstructed
        );
        assert_eq!(entry.fee, FeeState::Unknown);
        assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
        TransactionHistoryDetails {
            txid: TxId::from_bytes([0; 32]),
            mined_height: None,
            ..entry
        }
    });
    assert_eq!(details[0], details[1]);
}
