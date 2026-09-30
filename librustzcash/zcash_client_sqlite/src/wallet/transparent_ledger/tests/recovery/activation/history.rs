//! History completeness: owned effects, payment details, fees, and classification.

use zcash_client_backend::data_api::transparent_ledger::{
    DetailCompleteness, EffectCompleteness, FeeState, HistoryClassification, PoolEffect,
    PrivateTransparentDetail, TransactionHistoryDetails,
};
use zcash_protocol::PoolType;

use super::*;

fn history(st: &State, account: AccountUuid, txid: TxId) -> TransactionHistoryDetails {
    let mut entries = st
        .wallet()
        .db()
        .transaction_history_details(account, &[txid])
        .unwrap();
    assert_eq!(entries.len(), 1, "expected one entry for {txid}");
    entries.remove(0)
}

fn effect(entry: &TransactionHistoryDetails, pool: PoolType) -> PoolEffect {
    *entry.effects.iter().find(|e| e.pool == pool).unwrap()
}

fn transparent(entry: &TransactionHistoryDetails) -> PoolEffect {
    effect(entry, PoolType::Transparent)
}

fn sapling(entry: &TransactionHistoryDetails) -> PoolEffect {
    effect(entry, PoolType::SAPLING)
}

fn zat(value: u64) -> Zatoshis {
    Zatoshis::const_from_u64(value)
}

/// Covers only `address` of `account` through the target, with `receives`.
fn cover_one(
    st: &mut State,
    account: AccountUuid,
    address: TransparentAddress,
    receives: Vec<ReceiveEvent>,
    spends: Vec<SpendEvent>,
) {
    let ws = watch(st, account);
    let mut c = commit(&ws);
    c.receives = receives;
    c.spends = spends;
    c.coverage = full_coverage(&ws)
        .into_iter()
        .filter(|range| range.address == address)
        .collect();
    apply(st, c).unwrap();
}

#[test]
fn every_supported_pool_has_an_entry_and_other_transactions_are_omitted() {
    let (st, account, unspent) = active_wallet();
    let unrelated = TxId::from_bytes([0xee; 32]);
    let entries = st
        .wallet()
        .db()
        .transaction_history_details(account, &[unrelated, *unspent.outpoint.txid()])
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]
            .effects
            .iter()
            .map(|e| e.pool)
            .collect::<Vec<_>>(),
        vec![
            PoolType::Transparent,
            PoolType::SAPLING,
            #[cfg(feature = "orchard")]
            PoolType::ORCHARD,
            #[cfg(feature = "orchard")]
            PoolType::IRONWOOD,
        ]
    );

    // Another account sees none of this account's transactions.
    let mut st = st;
    let other = import_account(&mut st, 9);
    assert_eq!(
        st.wallet()
            .db()
            .transaction_history_details(other, &[*unspent.outpoint.txid()])
            .unwrap(),
        vec![]
    );
}

#[test]
fn private_transparent_effects_follow_ledger_coverage() {
    let (mut st, account, unspent) = active_wallet();

    // A receive within the active ledger's coverage is complete.
    let entry = history(&st, account, *unspent.outpoint.txid());
    assert_eq!(entry.mined_height, Some(unspent.mined_height));
    assert_eq!(
        transparent(&entry),
        PoolEffect {
            pool: PoolType::Transparent,
            received: zat(40_000),
            spent: Zatoshis::ZERO,
            completeness: EffectCompleteness::Complete,
        }
    );
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert_eq!(entry.payment_details, DetailCompleteness::Complete);
    assert_eq!(entry.fee, FeeState::NotApplicable);
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);
    assert_eq!(entry.pending_private_details, vec![]);

    // A newer receive, while another watched address is not yet covered through its height, is
    // known but incomplete: the account may have spent in it too, so its fee is unknown.
    let ws = watch(&st, account);
    let address = external(&ws);
    let fresh = receive(5, address, 60_000, below_target(&ws, 0));
    cover_one(&mut st, account, address, vec![fresh.clone()], vec![]);
    let entry = history(&st, account, *fresh.outpoint.txid());
    assert_eq!(transparent(&entry).received, zat(60_000));
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Incomplete
    );
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);

    // Covering every address completes it, under the same transaction identity.
    cover(&mut st, account, &revision(1, true), vec![]);
    let entry = history(&st, account, *fresh.outpoint.txid());
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Complete
    );
    assert_eq!(entry.fee, FeeState::NotApplicable);
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);
}

#[test]
fn a_debit_with_change_and_no_known_recipient_stays_visible() {
    let (mut st, account, unspent) = active_wallet();
    // A seed-restored payment from the account's transparent funds: the ledger recovers the
    // spend and the change, but not the external recipient or the fee.
    let ws = watch(&st, account);
    let at = below_target(&ws, 0);
    let payment = spend(6, &unspent, at);
    let change = ReceiveEvent {
        outpoint: OutPoint::new([6; 32], 1),
        ..receive(6, external(&ws), 15_000, at)
    };
    let mut c = commit(&ws);
    c.receives = vec![change];
    c.spends = vec![payment.clone()];
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();
    cover(&mut st, account, &revision(1, true), vec![]);

    let entry = history(&st, account, payment.spending_txid);
    assert_eq!(
        transparent(&entry),
        PoolEffect {
            pool: PoolType::Transparent,
            received: zat(15_000),
            spent: zat(40_000),
            completeness: EffectCompleteness::Complete,
        }
    );
    // Every owned effect is known, but the recipients are not, and the fee is unknown rather
    // than zero. Nothing is queued, which does not make the details complete.
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
    assert_eq!(entry.pending_private_details, vec![]);
    assert_eq!(
        st.wallet()
            .db()
            .pending_private_transparent_details()
            .unwrap(),
        vec![]
    );
}

#[test]
fn a_spend_of_an_unrecovered_output_is_incomplete() {
    let (mut st, account, _) = active_wallet();
    let ws = watch(&st, account);
    // The spent output was never recovered, so the spend's value is unknown.
    let missing = receive(7, external(&ws), 10_000, below_target(&ws, 1));
    let payment = spend(8, &missing, below_target(&ws, 0));
    let change = ReceiveEvent {
        outpoint: OutPoint::new([8; 32], 1),
        ..receive(8, external(&ws), 5_000, below_target(&ws, 0))
    };
    let mut c = commit(&ws);
    c.receives = vec![change];
    c.spends = vec![payment.clone()];
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    let entry = history(&st, account, payment.spending_txid);
    assert_eq!(transparent(&entry).received, zat(5_000));
    assert_eq!(transparent(&entry).spent, Zatoshis::ZERO);
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Incomplete
    );
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn public_rows_are_unverified_and_incomplete_once_private_authority_applies() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let address = external(&watch(&st, account));
    let outpoint = OutPoint::new([0x42; 32], 0);
    super::super::super::put_public_utxo(&mut st, &address, outpoint.clone(), 70_000);
    let txid = *outpoint.txid();

    // Public discovery holds authority: the receive is treated as settled but not verified.
    let entry = history(&st, account, txid);
    assert_eq!(
        transparent(&entry),
        PoolEffect {
            pool: PoolType::Transparent,
            received: zat(70_000),
            spent: Zatoshis::ZERO,
            completeness: EffectCompleteness::PublicDiscovery,
        }
    );
    assert_eq!(entry.payment_details, DetailCompleteness::Complete);
    assert_eq!(entry.fee, FeeState::NotApplicable);
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);

    // Under `PrivateRequired`, the candidate account's legacy row is no longer authoritative.
    set_policy(&mut st, PrivateRequired);
    let entry = history(&st, account, txid);
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Incomplete
    );
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn local_intent_survives_discovery_of_the_same_transaction() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    // A shielded-funded payment to the account's own transparent address.
    let taddr = external(&watch(&st, account));
    let (txid, output_index) = pay_from_sapling(&mut st, taddr, 50_000);
    let fee: i64 = conn(&st)
        .query_row(
            "SELECT fee FROM transactions WHERE txid = ?1",
            [txid.as_ref()],
            |row| row.get(0),
        )
        .unwrap();
    let fee = Zatoshis::from_nonnegative_i64(fee).unwrap();

    let local = history(&st, account, txid);
    assert_eq!(local.mined_height, None);
    assert_eq!(local.classification, HistoryClassification::LocalIntent);
    assert_eq!(local.payment_details, DetailCompleteness::Complete);
    assert_eq!(local.fee, FeeState::Known(fee));
    assert!(
        local
            .effects
            .iter()
            .all(|e| e.completeness == EffectCompleteness::Complete)
    );
    assert_eq!(transparent(&local).received, zat(50_000));
    assert_eq!(sapling(&local).spent, zat(200_000));
    assert_eq!(
        (sapling(&local).received + zat(50_000) + fee).unwrap(),
        zat(200_000)
    );

    // The private ledger then recovers the output as mined, and the account is promoted.
    let fixture = revision(1, true);
    let ws = watch(&st, account);
    let recovered = ReceiveEvent {
        outpoint: OutPoint::new(*txid.as_ref(), output_index),
        address: taddr,
        value: zat(50_000),
        coinbase: false,
        mined_height: below_target(&ws, 0),
    };
    cover(&mut st, account, &fixture, vec![recovered.clone()]);
    qualify(&mut st, &fixture);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, account).unwrap();

    // The local record is kept: only the placement changed.
    let discovered = history(&st, account, txid);
    assert_eq!(
        discovered,
        TransactionHistoryDetails {
            mined_height: Some(recovered.mined_height),
            ..local
        }
    );
}

#[test]
fn cross_account_transfers_report_each_side() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let (sender, recipient) = (accounts[0], accounts[1]);
    let to = external(&watch(&st, recipient));
    let (txid, _) = pay_from_sapling(&mut st, to, 30_000);

    let sent = history(&st, sender, txid);
    assert_eq!(sent.classification, HistoryClassification::LocalIntent);
    assert!(matches!(sent.fee, FeeState::Known(_)));
    assert_eq!(transparent(&sent).received, Zatoshis::ZERO);

    let received = history(&st, recipient, txid);
    assert_eq!(received.classification, HistoryClassification::LocalIntent);
    assert_eq!(transparent(&received).received, zat(30_000));
    assert_eq!(sapling(&received).spent, Zatoshis::ZERO);
    assert_eq!(received.fee, FeeState::NotApplicable);
}

#[test]
fn a_rewind_reopens_completeness_until_recovered_again() {
    let (mut st, account, unspent) = active_wallet();
    let txid = *unspent.outpoint.txid();
    assert_eq!(
        history(&st, account, txid).classification,
        HistoryClassification::Reconstructed
    );

    let floor = unspent.mined_height - 1;
    st.truncate_to_height(floor);
    let entry = history(&st, account, txid);
    assert_eq!(entry.mined_height, None);
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Incomplete
    );
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Incomplete);
    assert_eq!(entry.classification, HistoryClassification::Provisional);

    // Re-mined on the replacement chain and recovered, the transaction is complete again.
    scan_new_blocks(&mut st, 3);
    let remined = ReceiveEvent {
        mined_height: floor + 2,
        ..unspent.clone()
    };
    cover(&mut st, account, &revision(1, true), vec![remined.clone()]);
    let entry = history(&st, account, txid);
    assert_eq!(entry.mined_height, Some(remined.mined_height));
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);
}

#[test]
fn shielded_effects_follow_the_contiguously_scanned_height() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = st.test_account().cloned().unwrap();
    assert_eq!(account.id(), accounts[0]);
    let not_our_key = ExtendedSpendingKey::master(&[]).to_diversifiable_full_viewing_key();
    let dfvk = account.usk().sapling().to_diversifiable_full_viewing_key();

    // A gap block, then a block paying the account; only the second is scanned.
    let (gap, _, _) =
        st.generate_next_block(&not_our_key, AddressType::DefaultExternal, zat(10_000));
    let (paid, _, _) = st.generate_next_block(&dfvk, AddressType::DefaultExternal, zat(80_000));
    st.scan_cached_blocks(paid, 1);
    let txid: TxId = conn(&st)
        .query_row(
            "SELECT t.txid FROM sapling_received_notes n
             JOIN transactions t ON t.id_tx = n.transaction_id
             WHERE t.mined_height = ?1",
            [u32::from(paid)],
            |row| row.get::<_, [u8; 32]>(0).map(TxId::from_bytes),
        )
        .unwrap();

    let entry = history(&st, account.id(), txid);
    assert_eq!(sapling(&entry).received, zat(80_000));
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);

    // Scanning the gap makes the scanned chain contiguous through the payment. Compact scanning
    // does not retrieve the memo, so only the payment details stay incomplete.
    st.scan_cached_blocks(gap, 1);
    let entry = history(&st, account.id(), txid);
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert_eq!(entry.fee, FeeState::NotApplicable);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);
}

#[test]
fn pending_private_details_belong_to_their_transaction() {
    let (mut st, account, unspent) = active_wallet();
    let txid = *unspent.outpoint.txid();
    let id: i64 = conn(&st)
        .query_row(
            "SELECT id_tx FROM transactions WHERE txid = ?1",
            [txid.as_ref()],
            |row| row.get(0),
        )
        .unwrap();
    conn(&st)
        .execute(
            "INSERT INTO ironwood_enhance_routing (transaction_id, route) VALUES (?1, 2)",
            [id],
        )
        .unwrap();
    conn(&st)
        .execute(
            "INSERT INTO tx_retrieval_queue (txid, query_type, dependent_transaction_id)
             VALUES (?1, 1, ?2)",
            rusqlite::params![[9u8; 32], id],
        )
        .unwrap();

    let entry = history(&st, account, txid);
    assert_eq!(
        entry.pending_private_details,
        vec![
            PrivateTransparentDetail::ParentTransaction {
                txid: TxId::from_bytes([9; 32]),
            },
            PrivateTransparentDetail::MixedTransaction { txid },
        ]
    );
    // Another transaction has none of them.
    let ws = watch(&st, account);
    let fresh = receive(5, external(&ws), 60_000, below_target(&ws, 0));
    cover(&mut st, account, &revision(1, true), vec![fresh.clone()]);
    assert_eq!(
        history(&st, account, *fresh.outpoint.txid()).pending_private_details,
        vec![]
    );

    // Public authority withholds nothing.
    set_policy(&mut st, PrivateShadow);
    assert_eq!(history(&st, account, txid).pending_private_details, vec![]);
}

#[test]
fn history_requires_a_configured_handle_and_a_known_account() {
    let (mut st, account, unspent) = active_wallet();
    let txid = *unspent.outpoint.txid();
    assert!(matches!(
        st.wallet()
            .db()
            .transaction_history_details(AccountUuid::from_uuid(uuid::Uuid::nil()), &[txid]),
        Err(SqliteClientError::AccountUnknown)
    ));
    st.wallet_mut().db_mut().transparent_ledger_mode = None;
    assert!(matches!(
        st.wallet()
            .db()
            .transaction_history_details(account, &[txid]),
        Err(SqliteClientError::TransparentLedgerModeNotConfigured)
    ));
}

/// Pays an external transparent address from a fresh Sapling note, then erases the local
/// construction evidence, leaving what payload ingestion would store for a seed-restored send:
/// the full transaction, its fee, and the outputs the wallet can decrypt.
fn discovered_payment(st: &mut State) -> TxId {
    let external = TransparentAddress::PublicKeyHash([7; 20]);
    let (txid, _) = pay_from_sapling(st, external, 50_000);
    conn(st)
        .execute(
            "UPDATE transactions SET created = NULL, target_height = NULL WHERE txid = ?1",
            [txid.as_ref()],
        )
        .unwrap();
    txid
}

#[test]
fn full_data_completes_details_only_when_every_spent_unit_is_accounted_for() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let txid = discovered_payment(&mut st);

    // The spend, the change, the recovered payment, and the fee account for every unit.
    let entry = history(&st, account, txid);
    assert_eq!(sapling(&entry).spent, zat(200_000));
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert!(matches!(entry.fee, FeeState::Known(_)));
    assert_eq!(entry.classification, HistoryClassification::Reconstructed);

    // An output the wallet cannot decrypt, such as one sent with the outgoing viewing key
    // discarded, leaves a payment unknown although the full transaction is stored.
    conn(&st)
        .execute(
            "DELETE FROM sent_notes WHERE output_pool = 0
             AND transaction_id = (SELECT id_tx FROM transactions WHERE txid = ?1)",
            [txid.as_ref()],
        )
        .unwrap();
    let entry = history(&st, account, txid);
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert!(matches!(entry.fee, FeeState::Known(_)));
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn unmined_shielded_spends_are_incomplete_until_the_scanned_chain_reaches_the_tip() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let txid = discovered_payment(&mut st);
    assert_eq!(
        sapling(&history(&st, account, txid)).completeness,
        EffectCompleteness::Complete
    );

    // Unscanned blocks may hold notes the unmined transaction spends, which are not linked yet.
    let tip = st.wallet().chain_height().unwrap().unwrap();
    st.wallet_mut().update_chain_tip(tip + 5).unwrap();
    let entry = history(&st, account, txid);
    assert_eq!(entry.mined_height, None);
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Incomplete);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn creation_evidence_alone_does_not_certify_effects_or_details() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    // An outbox records only that the wallet created the transaction; its signed bytes, inputs,
    // and recipients live outside the wallet database.
    let outpoint = OutPoint::new([0x51; 32], 0);
    let tip = st.wallet().chain_height().unwrap().unwrap();
    crate::wallet::record_transaction_created(conn(&st), *outpoint.txid(), tip + 1).unwrap();
    // Discovery then records only its output to the account.
    let address = external(&watch(&st, account));
    super::super::super::put_public_utxo(&mut st, &address, outpoint.clone(), 20_000);

    let entry = history(&st, account, *outpoint.txid());
    assert_eq!(entry.classification, HistoryClassification::LocalIntent);
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::PublicDiscovery
    );
    // Every effect is settled, yet the wallet likely funded it through spends it has not
    // recorded, so it is not known to be a receipt.
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
}

#[test]
fn an_unresolved_spend_alone_makes_the_account_a_party() {
    let (mut st, account, _) = active_wallet();
    let ws = watch(&st, account);
    // A payment with no change, recovered before the output it spends.
    let missing = receive(7, external(&ws), 10_000, below_target(&ws, 1));
    let payment = spend(8, &missing, below_target(&ws, 0));
    let mut c = commit(&ws);
    c.spends = vec![payment.clone()];
    c.coverage = full_coverage(&ws);
    apply(&mut st, c).unwrap();

    let entry = history(&st, account, payment.spending_txid);
    assert_eq!(
        transparent(&entry),
        PoolEffect {
            pool: PoolType::Transparent,
            received: Zatoshis::ZERO,
            spent: Zatoshis::ZERO,
            completeness: EffectCompleteness::Incomplete,
        }
    );
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn a_candidate_ledger_spend_stays_out_of_history() {
    let (mut st, account) = shadow_wallet();
    let ws = watch(&st, account);
    let missing = receive(7, external(&ws), 10_000, below_target(&ws, 1));
    let payment = spend(8, &missing, below_target(&ws, 0));
    let mut c = commit(&ws);
    c.spends = vec![payment.clone()];
    apply(&mut st, c).unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .transaction_history_details(account, &[payment.spending_txid])
            .unwrap(),
        vec![]
    );
}

#[test]
fn a_constructed_payment_to_an_own_shielded_address_is_incomplete_until_scanned() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = st.test_account().cloned().unwrap();
    assert_eq!(account.id(), accounts[0]);
    let own = account
        .usk()
        .sapling()
        .to_diversifiable_full_viewing_key()
        .default_address()
        .1;
    let txid =
        pay_address_from_sapling(&mut st, zcash_keys::address::Address::Sapling(own), 50_000);

    // Local construction defers the receipt until scanning finds it, so the Sapling receipts are
    // not yet all known.
    let entry = history(&st, account.id(), txid);
    assert_eq!(entry.classification, HistoryClassification::LocalIntent);
    assert_eq!(entry.payment_details, DetailCompleteness::Complete);
    assert_eq!(sapling(&entry).spent, zat(200_000));
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Incomplete);
    let change = sapling(&entry).received;

    // Once mined and scanned, the payment is an owned receipt and the pool is complete.
    let (height, _) = st.generate_next_block_including(txid);
    st.scan_cached_blocks(height, 1);
    let entry = history(&st, account.id(), txid);
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert_eq!(sapling(&entry).received, (change + zat(50_000)).unwrap());
}

#[test]
fn an_account_without_a_full_viewing_key_never_completes_shielded_effects() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let viewer = accounts[1];
    // The imported account's Sapling key, from `import_account(st, 7)`.
    let dfvk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
        st.network(),
        &[7; 32],
        zip32::AccountId::ZERO,
    )
    .unwrap()
    .sapling()
    .to_diversifiable_full_viewing_key();
    let (height, _, _) = st.generate_next_block(&dfvk, AddressType::DefaultExternal, zat(30_000));
    st.scan_cached_blocks(height, 1);
    let txid: TxId = conn(&st)
        .query_row(
            "SELECT t.txid FROM transactions t WHERE t.mined_height = ?1",
            [u32::from(height)],
            |row| row.get::<_, [u8; 32]>(0).map(TxId::from_bytes),
        )
        .unwrap();
    let entry = history(&st, viewer, txid);
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Complete);
    assert_eq!(entry.fee, FeeState::NotApplicable);

    // With only an incoming viewing key, the account's spends are never detected, so the receipt
    // may have been funded by its own notes.
    conn(&st)
        .execute(
            "UPDATE accounts SET ufvk = NULL WHERE uuid = ?1",
            [viewer.expose_uuid()],
        )
        .unwrap();
    let entry = history(&st, viewer, txid);
    assert_eq!(sapling(&entry).received, zat(30_000));
    assert_eq!(sapling(&entry).completeness, EffectCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

/// Records that `child` has a transparent input spending `parent`'s first output, which the
/// wallet does not hold, and queues `parent` for retrieval on behalf of `dependent`.
fn queue_parent(st: &State, child: TxId, parent: [u8; 32], dependent: TxId) {
    let id = |txid: TxId| -> i64 {
        conn(st)
            .query_row(
                "SELECT id_tx FROM transactions WHERE txid = ?1",
                [txid.as_ref()],
                |row| row.get(0),
            )
            .unwrap()
    };
    conn(st)
        .execute(
            "INSERT INTO transparent_spend_map
                 (spending_transaction_id, prevout_txid, prevout_output_index)
             VALUES (?1, ?2, 0)",
            rusqlite::params![id(child), parent],
        )
        .unwrap();
    conn(st)
        .execute(
            "INSERT INTO tx_retrieval_queue (txid, query_type, dependent_transaction_id)
             VALUES (?1, 1, ?2)
             ON CONFLICT (txid, query_type) DO UPDATE
             SET dependent_transaction_id = excluded.dependent_transaction_id",
            rusqlite::params![parent, id(dependent)],
        )
        .unwrap();
}

#[test]
fn a_queued_parent_keeps_public_transparent_effects_open() {
    let (mut st, accounts) = shadow_wallet_with(0);
    let account = accounts[0];
    let address = external(&watch(&st, account));
    let outpoint = OutPoint::new([0x42; 32], 0);
    super::super::super::put_public_utxo(&mut st, &address, outpoint.clone(), 70_000);
    let txid = *outpoint.txid();
    assert_eq!(
        history(&st, account, txid).classification,
        HistoryClassification::Reconstructed
    );

    // One of its inputs spends an output of a parent that public discovery has yet to retrieve,
    // which may be the account's own.
    queue_parent(&st, txid, [0x43; 32], txid);
    let entry = history(&st, account, txid);
    assert_eq!(
        transparent(&entry).completeness,
        EffectCompleteness::Incomplete
    );
    assert_eq!(entry.fee, FeeState::Unknown);
    assert_eq!(entry.classification, HistoryClassification::Provisional);
}

#[test]
fn a_parent_shared_by_two_transactions_is_pending_for_both() {
    let (mut st, account, unspent) = active_wallet();
    let first = *unspent.outpoint.txid();
    let ws = watch(&st, account);
    let fresh = receive(5, external(&ws), 60_000, below_target(&ws, 0));
    cover(&mut st, account, &revision(1, true), vec![fresh.clone()]);
    let second = *fresh.outpoint.txid();

    // The queue records only the latest dependent of the shared parent.
    queue_parent(&st, first, [0x44; 32], first);
    queue_parent(&st, second, [0x44; 32], second);
    let parent = PrivateTransparentDetail::ParentTransaction {
        txid: TxId::from_bytes([0x44; 32]),
    };
    assert_eq!(
        history(&st, account, first).pending_private_details,
        vec![parent]
    );
    assert_eq!(
        history(&st, account, second).pending_private_details,
        vec![parent]
    );
}

#[test]
fn deleting_the_funding_account_removes_the_construction_evidence() {
    let (mut st, accounts) = shadow_wallet_with(1);
    let (sender, recipient) = (accounts[0], accounts[1]);
    let to = external(&watch(&st, recipient));
    let (txid, _) = pay_from_sapling(&mut st, to, 30_000);
    assert_eq!(
        history(&st, recipient, txid).payment_details,
        DetailCompleteness::Complete
    );

    // The sender's recorded outputs go with it; the recipient's receipt stays.
    st.wallet_mut().delete_account(sender).unwrap();
    let entry = history(&st, recipient, txid);
    assert_eq!(transparent(&entry).received, zat(30_000));
    assert_eq!(entry.classification, HistoryClassification::LocalIntent);
    assert_eq!(entry.payment_details, DetailCompleteness::Incomplete);
    assert_eq!(entry.fee, FeeState::Unknown);
}
