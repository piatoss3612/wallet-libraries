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
    recovered_transfer_revision(cross_account, receive_first, revision(1, true))
}

fn recovered_transfer_revision(
    cross_account: bool,
    receive_first: bool,
    rev: RecoveryRevision,
) -> (State, AccountUuid, AccountUuid, ReceiveEvent) {
    recovered_transfer_with_accounts(cross_account, receive_first, rev, u8::from(cross_account))
}

fn recovered_transfer_with_accounts(
    cross_account: bool,
    receive_first: bool,
    rev: RecoveryRevision,
    extra_accounts: u8,
) -> (State, AccountUuid, AccountUuid, ReceiveEvent) {
    let (mut donor, donor_accounts) = shadow_wallet_with(extra_accounts);
    let receiver = donor_accounts[usize::from(cross_account)];
    let to = external(&watch(&donor, receiver));
    let (txid, index) = pay_from_sapling(&mut donor, to, 50_000);
    let tx = donor.wallet().get_transaction(txid).unwrap().unwrap();

    let (mut st, accounts) = shadow_wallet_with(extra_accounts);
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
    cover(&mut st, receiver, &rev, vec![received.clone()]);
    if receive_first {
        st.scan_cached_blocks(height, 1);
    }
    for account in accounts.iter().filter(|account| **account != receiver) {
        cover(&mut st, *account, &rev, vec![]);
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

#[test]
fn owned_transfers_require_private_coverage_and_preserve_unknown_scope() {
    let (mut st, sender, receiver, event) = recovered_transfer(true, false);
    conn(&st).execute("UPDATE transparent_received_outputs SET address_id = (SELECT ad.id FROM addresses ad JOIN accounts a ON a.id = ad.account_id WHERE a.uuid = ?2 LIMIT 1) WHERE transaction_id = (SELECT id_tx FROM transactions WHERE txid = ?1)", rusqlite::params![event.outpoint.txid().as_ref(), sender.expose_uuid()]).unwrap();
    let entry = details(&st, sender, &event);
    assert_eq!(entry.owned_transparent_outputs[0].scope, None);
    assert_eq!(
        entry.owned_transparent_outputs[0].inferred_funding_account,
        Some(sender)
    );
    // Public placement of the output cannot substitute for private coverage.
    conn(&st).execute("DELETE FROM tpir_coverage WHERE account_id = (SELECT id FROM accounts WHERE uuid = ?1)", [receiver.expose_uuid()]).unwrap();
    assert_eq!(
        details(&st, sender, &event).owned_transparent_outputs[0].inferred_funding_account,
        None
    );
    cover(&mut st, receiver, &revision(1, true), vec![]);
    assert_eq!(
        details(&st, sender, &event).owned_transparent_outputs[0].inferred_funding_account,
        Some(sender)
    );
    conn(&st).execute("DELETE FROM tpir_coverage WHERE account_id = (SELECT id FROM accounts WHERE uuid = ?1)", [sender.expose_uuid()]).unwrap();
    assert_eq!(
        details(&st, sender, &event).owned_transparent_outputs[0].inferred_funding_account,
        None
    );
}

#[test]
fn owned_transfers_withdraw_on_revision_replacement_without_replay() {
    let (mut st, sender, receiver, event) =
        recovered_transfer_revision(true, false, revision(1, false));
    assert_eq!(
        details(&st, sender, &event).owned_transparent_outputs[0].inferred_funding_account,
        Some(sender)
    );
    let next = revision(2, false);
    qualify(&mut st, &next);
    cover(&mut st, receiver, &next, vec![]);
    assert!(
        details(&st, sender, &event)
            .owned_transparent_outputs
            .is_empty()
    );
}

#[test]
fn owned_transfers_remove_deleted_recipient_and_sender_independently() {
    let (mut st, sender, receiver, event) = recovered_transfer(true, false);
    st.wallet_mut().delete_account(receiver).unwrap();
    assert!(
        details(&st, sender, &event)
            .owned_transparent_outputs
            .is_empty()
    );
    let (mut st, sender, receiver, event) = recovered_transfer(true, false);
    st.wallet_mut().delete_account(sender).unwrap();
    let entry = details(&st, receiver, &event);
    assert!(entry.known_wallet_funders.is_empty());
    assert_eq!(
        entry.owned_transparent_outputs[0].inferred_funding_account,
        None
    );
}

#[test]
fn owned_transfers_retain_ambiguity_and_follow_current_funding_ownership() {
    let (st, sender, receiver, event) = recovered_transfer(true, false);
    // Add a second owned Sapling participant to the SQLite evidence fixture. This
    // is deliberately independent of transparent sent records and of output value.
    conn(&st).execute("INSERT INTO sapling_received_notes
        (transaction_id, output_index, account_id, diversifier, value, rcm, is_change,
         memo, commitment_tree_position, recipient_key_scope)
        SELECT n.transaction_id, n.output_index + 10, a.id, n.diversifier, n.value, n.rcm,
               n.is_change, n.memo, n.commitment_tree_position, n.recipient_key_scope
        FROM sapling_received_notes n JOIN sapling_received_note_spends s ON s.sapling_received_note_id = n.id
        JOIN accounts a ON a.uuid = ?2
        JOIN transactions t ON t.id_tx = s.transaction_id WHERE t.txid = ?1",
        rusqlite::params![event.outpoint.txid().as_ref(), receiver.expose_uuid()]).unwrap();
    conn(&st)
        .execute(
            "INSERT INTO sapling_received_note_spends (sapling_received_note_id, transaction_id)
        SELECT n.id, t.id_tx FROM sapling_received_notes n JOIN accounts a ON a.id = n.account_id
        JOIN transactions t ON t.txid = ?1 WHERE a.uuid = ?2 AND n.output_index >= 10",
            rusqlite::params![event.outpoint.txid().as_ref(), receiver.expose_uuid()],
        )
        .unwrap();
    let entry = details(&st, sender, &event);
    assert_eq!(entry.known_wallet_funders.len(), 2);
    assert_eq!(
        entry.owned_transparent_outputs[0].inferred_funding_account,
        None
    );
    assert_eq!(entry.aggregate_payment, AggregatePayment::Unknown);
    // Remove the first participant: there is no persisted guessed sender to undo.
    conn(&st).execute("DELETE FROM sapling_received_note_spends WHERE sapling_received_note_id IN
        (SELECT n.id FROM sapling_received_notes n JOIN accounts a ON a.id = n.account_id WHERE a.uuid = ?1)",
        [sender.expose_uuid()]).unwrap();
    let entry = details(&st, receiver, &event);
    assert_eq!(entry.known_wallet_funders, vec![receiver]);
    assert_eq!(
        entry.owned_transparent_outputs[0].inferred_funding_account,
        Some(receiver)
    );
    assert_eq!(entry.fee, FeeState::Unknown);
}

#[test]
fn owned_transfers_count_unresolved_funders_in_both_discovery_orders() {
    for parent_first in [false, true] {
        let (mut st, sender, _, event) =
            recovered_transfer_with_accounts(false, false, revision(1, true), 1);
        let other = conn(&st)
            .query_row(
                "SELECT uuid FROM accounts WHERE uuid != ?1",
                [sender.expose_uuid()],
                |row| row.get(0).map(AccountUuid),
            )
            .unwrap();
        let before = details(&st, sender, &event);
        assert_eq!(
            before.owned_transparent_outputs[0].inferred_funding_account,
            Some(sender)
        );
        let missing = receive(
            0xd1,
            external(&watch(&st, other)),
            10_000,
            event.mined_height - 1,
        );
        if parent_first {
            cover(&mut st, other, &revision(1, true), vec![missing.clone()]);
        }
        let provisional = revision(2, false);
        qualify(&mut st, &provisional);
        let mut payment = spend(0xd2, &missing, event.mined_height);
        payment.spending_txid = *event.outpoint.txid();
        let mut c = commit(&watch(&st, other));
        c.revision = provisional.clone();
        c.spends = vec![payment];
        c.coverage = full_coverage(&watch(&st, other));
        apply(&mut st, c).unwrap();

        let assert_ambiguous = |st: &State| {
            let entry = details(st, sender, &event);
            assert_eq!(entry.known_wallet_funders.len(), 2);
            assert!(entry.known_wallet_funders.contains(&sender));
            assert!(entry.known_wallet_funders.contains(&other));
            assert_eq!(
                entry.owned_transparent_outputs[0].inferred_funding_account,
                None
            );
            assert_eq!(entry.effects, before.effects);
            assert_eq!(entry.account_movement, before.account_movement);
            assert_eq!(entry.fee, before.fee);
            assert_eq!(entry.aggregate_payment, before.aggregate_payment);
            assert_eq!(entry.payment_details, before.payment_details);
        };
        assert_ambiguous(&st);
        if !parent_first {
            cover(&mut st, other, &revision(1, true), vec![missing]);
            assert_ambiguous(&st);
        }

        // Withdrawing the spend's provisional revision removes both its unresolved
        // participation and any canonical link subsequently recovered from its parent.
        let next = revision(3, false);
        qualify(&mut st, &next);
        cover(&mut st, other, &next, vec![]);
        let entry = details(&st, sender, &event);
        assert_eq!(entry.known_wallet_funders, vec![sender]);
        assert_eq!(
            entry.owned_transparent_outputs[0].inferred_funding_account,
            Some(sender)
        );
        assert_eq!(entry.account_movement, before.account_movement);
    }
}

#[test]
fn owned_transfers_exclude_inactive_or_unsupported_unresolved_funders() {
    let (mut st, sender, _, event) =
        recovered_transfer_with_accounts(false, false, revision(1, true), 1);
    let other = conn(&st)
        .query_row(
            "SELECT uuid FROM accounts WHERE uuid != ?1",
            [sender.expose_uuid()],
            |row| row.get(0).map(AccountUuid),
        )
        .unwrap();
    let missing = receive(
        0xd1,
        external(&watch(&st, other)),
        10_000,
        event.mined_height - 1,
    );
    let mut payment = spend(0xd2, &missing, event.mined_height);
    payment.spending_txid = *event.outpoint.txid();
    let mut c = commit(&watch(&st, other));
    c.revision = source(b"other-funder", 1);
    qualify(&mut st, &c.revision);
    c.spends = vec![payment];
    apply(&mut st, c).unwrap();
    let assert_single = |st: &State| {
        let entry = details(st, sender, &event);
        assert_eq!(entry.known_wallet_funders, vec![sender]);
        assert_eq!(
            entry.owned_transparent_outputs[0].inferred_funding_account,
            Some(sender)
        );
    };
    // Each mutation isolates a read eligibility boundary without fabricating sent notes.
    conn(&st).execute("DELETE FROM tpir_active_accounts WHERE account_id = (SELECT id FROM accounts WHERE uuid = ?1)", [other.expose_uuid()]).unwrap();
    assert_single(&st);
    conn(&st)
        .execute(
            "INSERT INTO tpir_active_accounts SELECT id FROM accounts WHERE uuid = ?1",
            [other.expose_uuid()],
        )
        .unwrap();
    conn(&st)
        .execute(
            "INSERT INTO tpir_quarantined_accounts SELECT id FROM accounts WHERE uuid = ?1",
            [other.expose_uuid()],
        )
        .unwrap();
    assert_single(&st);
    conn(&st)
        .execute("DELETE FROM tpir_quarantined_accounts", [])
        .unwrap();
    conn(&st)
        .execute(
            "INSERT INTO tpir_quarantined_sources (source) VALUES (?1)",
            [b"other-funder".as_slice()],
        )
        .unwrap();
    assert_single(&st);
    conn(&st)
        .execute("DELETE FROM tpir_quarantined_sources", [])
        .unwrap();
    conn(&st)
        .execute(
            "UPDATE tpir_spend_events SET mined_height = NULL WHERE spending_txid = ?1",
            [event.outpoint.txid().as_ref()],
        )
        .unwrap();
    assert_single(&st);
    conn(&st)
        .execute(
            "UPDATE tpir_spend_events SET mined_height = ?1 WHERE spending_txid = ?2",
            rusqlite::params![
                u32::from(event.mined_height),
                event.outpoint.txid().as_ref()
            ],
        )
        .unwrap();
    conn(&st).execute("DELETE FROM tpir_qualified_revisions WHERE revision_id IN (SELECT id FROM tpir_revisions WHERE source = ?1)", [b"other-funder".as_slice()]).unwrap();
    assert_single(&st);
}
