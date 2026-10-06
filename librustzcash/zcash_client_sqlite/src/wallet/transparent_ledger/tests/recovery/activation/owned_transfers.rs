//! Recovered owned outputs reconcile with compact-scanned funding, without sent-note writes.
use super::*;
use zcash_client_backend::data_api::transparent_ledger::{
    AggregatePayment, FeeState, TransactionHistoryDetails, TransparentOutputScope,
};

/// A real Sapling-funded transaction built in a donor wallet. The recovered wallet scans the
/// same funding and spending transactions but never stores the spending payload or sent notes.
fn recovered_transfer(
    cross_account: bool,
    receive_first: bool,
) -> (State, AccountUuid, AccountUuid, ReceiveEvent) {
    let (mut donor, donor_accounts) = shadow_wallet_with(u8::from(cross_account));
    let receiver = donor_accounts[usize::from(cross_account)];
    let to = external(&watch(&donor, receiver));
    let (txid, index) = pay_from_sapling(&mut donor, to, 50_000);
    let tx = donor.wallet().get_transaction(txid).unwrap().unwrap();

    let (mut st, accounts) = shadow_wallet_with(u8::from(cross_account));
    let sender = accounts[0];
    let receiver = accounts[usize::from(cross_account)];
    let dfvk = st
        .test_account()
        .unwrap()
        .usk()
        .sapling()
        .to_diversifiable_full_viewing_key();
    let (funding_height, _, _) = st.generate_next_block(
        &dfvk,
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(200_000),
    );
    st.scan_cached_blocks(funding_height, 1);
    let (height, _) = st.generate_next_block_from_tx(1, &tx);
    let received = ReceiveEvent {
        metadata: None,
        outpoint: OutPoint::new(*txid.as_ref(), index),
        address: to,
        value: Zatoshis::const_from_u64(50_000),
        coinbase: false,
        mined_height: height,
    };
    st.scan_cached_blocks(height, 1);
    if receive_first {
        // Model a delayed spend-link recovery on an already accepted chain. Re-scanning
        // the real compact transaction below restores the link through production scanning.
        conn(&st)
            .execute(
                "DELETE FROM sapling_received_note_spends WHERE transaction_id IN
            (SELECT id_tx FROM transactions WHERE txid = ?1)",
                [txid.as_ref()],
            )
            .unwrap();
    }
    let rev = revision(1, true);
    cover(&mut st, receiver, &rev, vec![received.clone()]);
    if receive_first {
        st.scan_cached_blocks(height, 1);
    }
    if cross_account {
        cover(&mut st, sender, &rev, vec![]);
    }
    qualify(&mut st, &rev);
    set_policy(&mut st, PrivateRequired);
    for account in &accounts {
        promote(&mut st, *account).unwrap();
    }
    (st, sender, receiver, received)
}

fn details(
    st: &State,
    account: AccountUuid,
    event: &ReceiveEvent,
) -> TransactionHistoryDetails<AccountUuid> {
    st.wallet()
        .db()
        .transaction_history_details(account, &[*event.outpoint.txid()])
        .unwrap()
        .remove(0)
}

#[test]
fn owned_transfers_reconcile_both_discovery_orders_without_financial_attribution() {
    for cross in [false, true] {
        for receive_first in [false, true] {
            let (st, sender, receiver, event) = recovered_transfer(cross, receive_first);
            let before = conn(&st).total_changes();
            let entry = details(&st, sender, &event);
            assert_eq!(
                conn(&st).total_changes(),
                before,
                "history reconciliation writes nothing"
            );
            assert_eq!(entry.known_wallet_funders, vec![sender]);
            assert_eq!(entry.owned_transparent_outputs.len(), 1);
            let output = &entry.owned_transparent_outputs[0];
            assert_eq!(output.outpoint, event.outpoint);
            assert_eq!(output.recipient_account, receiver);
            assert_eq!(output.scope, Some(TransparentOutputScope::External));
            assert_eq!(output.inferred_funding_account, Some(sender));
            assert_eq!(entry.fee, FeeState::Unknown);
            assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
            assert_eq!(
                entry.payment_details,
                zcash_client_backend::data_api::transparent_ledger::DetailCompleteness::Incomplete
            );
            assert_eq!(
                details(&st, receiver, &event).owned_transparent_outputs,
                entry.owned_transparent_outputs
            );
            let (raw, sent): (bool, i64) = conn(&st).query_row(
                "SELECT raw IS NOT NULL, (SELECT COUNT(*) FROM sent_notes s WHERE s.transaction_id = t.id_tx AND s.output_pool = 0) FROM transactions t WHERE txid = ?1",
                [event.outpoint.txid().as_ref()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert!(!raw);
            assert_eq!(sent, 0);
        }
    }
}

#[test]
fn owned_transfers_withdraw_on_rewind_and_account_deletion() {
    let (mut st, sender, receiver, event) = recovered_transfer(true, false);
    assert_eq!(
        details(&st, sender, &event).owned_transparent_outputs[0].inferred_funding_account,
        Some(sender)
    );
    st.truncate_to_height(event.mined_height - 1);
    let entry = details(&st, sender, &event);
    assert!(entry.owned_transparent_outputs.is_empty());
    st.wallet_mut().delete_account(receiver).unwrap();
    assert!(
        details(&st, sender, &event)
            .owned_transparent_outputs
            .is_empty()
    );
}
