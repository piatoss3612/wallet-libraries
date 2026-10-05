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
    height: BlockHeight,
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
        height,
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
    assert_eq!(
        entry.account_movement.net(),
        i128::from(shape.shielded) - i128::from(shape.inputs[0] + shape.inputs[1])
    );
}

/// Queries the received action's memo privately and applies the service's record for it.
fn recover_memo(case: &mut Shielding, fee: Option<u64>) -> Vec<EnhancePirStoreResult> {
    let queries = private_queries(&case.st);
    assert_eq!(queries.len(), 1, "one memo query for the received action");
    let request = queries[0];
    assert_eq!(request.request_id().txid(), case.txid);
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
/// exact and agrees across both private sources, and the account's spent value is exactly its
/// shielded receipt plus that fee.
fn assert_reconstructed_shielding(case: &Shielding, shape: &Shape) {
    assert_owned_effects(case, shape);
    let entry = history(&case.st, case.account, case.txid);
    // The received memo is recovered, so the payment details are complete.
    assert_eq!(entry.payment_details, DetailCompleteness::Complete);
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);
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

/// Both reported shapes recover their memo and whole-transaction fee privately and reconstruct
/// as a shielding: the account's transparent inputs became its Ironwood output and the fee.
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
        // The memo and whole-transaction fee are stored; the transparent details stay
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
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
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
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
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
    ] {
        let mut case = shielding(shape, metadata);
        recover_memo(&mut case, record_fee);
        let entry = history(&case.st, case.account, case.txid);
        assert_eq!(entry.account_movement.net(), -i128::from(FEE));
        assert_eq!(entry.classification, HistoryClassification::Provisional);
        assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
        assert_eq!(entry.fee, FeeState::Unknown);
        assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
    }
}

/// A record without a fee still yields its authenticated memo, and no more.
#[test]
fn memo_only_recovery_does_not_complete_the_payment() {
    let shape = &CASES[1];
    let mut case = shielding(shape, Some(reported_metadata()));
    assert_eq!(
        recover_memo(&mut case, None),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (Some(2), None, Some(MEMO.to_vec()), false)
    );
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    let entry = history(&case.st, case.account, case.txid);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
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
    assert!(matches!(
        case.st
            .wallet_mut()
            .db_mut()
            .apply_ironwood_enhance_records(&[(request, record.clone())])
            .unwrap(),
        EnhancePirBatchResult::Rejected { .. }
    ));
    assert_eq!(
        stored(&case.st, case.tx_ref),
        (Some(0), Some(25_000), None, false)
    );
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
    assert!(matches!(
        case.st
            .wallet_mut()
            .db_mut()
            .apply_ironwood_enhance_records(&[(request, record.clone())])
            .unwrap(),
        EnhancePirBatchResult::Rejected { .. }
    ));
    assert_eq!(stored(&case.st, case.tx_ref), (Some(0), None, None, false));
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

/// A database whose earlier routing cleared the memo work of a route-2 transaction resumes it on
/// upgrade, privately and without a reset.
#[test]
fn an_existing_route_two_database_resumes_private_memo_recovery_after_upgrade() {
    use crate::wallet::init::{WalletMigrator, migrations::CURRENT_LEAF_MIGRATIONS};

    let shape = &CASES[1];
    let mut case = shielding(shape, Some(reported_metadata()));
    // The state the previous routing left behind: route 2, every queue cleared, memo and fee
    // unknown, recorded by a database that predates the retry migration.
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
    for leaf in CURRENT_LEAF_MIGRATIONS {
        conn(&case.st)
            .execute(
                "DELETE FROM schemer_migrations WHERE id = ?1",
                [leaf.as_bytes().to_vec()],
            )
            .unwrap();
    }
    assert_eq!(stored(&case.st, case.tx_ref), (Some(2), None, None, false));
    assert_eq!(queued(&case.st, case.tx_ref), 0);
    assert!(private_queries(&case.st).is_empty(), "stuck before upgrade");
    let rows = financial_rows(&case.st, case.tx_ref);

    // Upgrading, and reopening afterwards, queues the memo once.
    for _ in 0..2 {
        WalletMigrator::new()
            .init_or_migrate(case.st.wallet_mut().db_mut())
            .unwrap();
    }
    assert_eq!(queued(&case.st, case.tx_ref), 1);
    assert_eq!(stored(&case.st, case.tx_ref), (Some(2), None, None, false));
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);

    assert_eq!(
        recover_memo(&mut case, Some(FEE)),
        vec![EnhancePirStoreResult::PrivateDetailsUnsupported]
    );
    assert_eq!(financial_rows(&case.st, case.tx_ref), rows);
    assert_reconstructed_shielding(&case, shape);
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
