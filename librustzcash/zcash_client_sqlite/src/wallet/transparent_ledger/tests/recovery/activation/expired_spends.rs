//! Spend discovery resumes after a locally recorded spender expires without mining.

use zcash_client_backend::data_api::{
    InputSource as _, TransactionDataRequest, wallet::decrypt_and_store_transaction,
};
use zcash_primitives::transaction::Transaction;

use super::public_fixtures::{
    EXTERNAL, expiring_transaction, external_of, funding, outpoint, public_wallet, store,
    transaction,
};
use super::*;

fn tip(st: &State) -> BlockHeight {
    st.wallet().chain_height().unwrap().unwrap()
}

fn spendable(st: &State, outpoint: &OutPoint) -> bool {
    st.wallet()
        .db()
        .get_unspent_transparent_output(outpoint, (tip(st) + 1).into())
        .unwrap()
        .is_some()
}

fn store_unmined(st: &mut State, tx: &Transaction) {
    let network = *st.network();
    decrypt_and_store_transaction(&network, st.wallet_mut(), tx, None).unwrap();
}

#[cfg(not(feature = "spend-index"))]
fn spend_searches(st: &State, address: TransparentAddress) -> Vec<(BlockHeight, BlockHeight)> {
    st.wallet()
        .transaction_data_requests()
        .unwrap()
        .into_iter()
        .filter_map(|request| match request {
            TransactionDataRequest::TransactionsInvolvingAddress(req)
                if req.address() == address =>
            {
                Some((req.block_range_start(), req.block_range_end().unwrap()))
            }
            _ => None,
        })
        .collect()
}

#[cfg(not(feature = "spend-index"))]
#[test]
fn a_spend_that_expired_resumes_the_search_for_the_outputs_spend() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let parent = funding(0xd2, address, 1_000_000);
    store(&mut st, &parent);
    let received = outpoint(&parent, 0);
    let mined = tip(&st);

    // The wallet's own spend of the output, never mined. Meanwhile, nothing is searched for.
    let withheld =
        expiring_transaction(vec![received.clone()], vec![(EXTERNAL, 990_000)], mined + 2);
    store_unmined(&mut st, &withheld);
    assert_eq!(spend_searches(&st, address), vec![]);

    // Once it expires, another transaction may have spent the output: the search resumes.
    st.wallet_mut().update_chain_tip(mined + 5).unwrap();
    assert_eq!(spend_searches(&st, address), vec![(mined, mined + 6)]);
    let actual = transaction(vec![received.clone()], vec![(EXTERNAL, 990_000)]);
    store(&mut st, &actual);
    assert_eq!(spend_searches(&st, address), vec![]);
    assert!(!spendable(&st, &received));
}

#[cfg(not(feature = "spend-index"))]
#[test]
fn completing_a_search_after_expiry_advances_its_frontier() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let parent = funding(0xd4, address, 1_000_000);
    store(&mut st, &parent);
    let received = outpoint(&parent, 0);
    let mined = tip(&st);
    let withheld = expiring_transaction(vec![received], vec![(EXTERNAL, 990_000)], mined + 2);
    store_unmined(&mut st, &withheld);
    st.wallet_mut().update_chain_tip(mined + 5).unwrap();
    let requests = st.wallet().transaction_data_requests().unwrap();
    let request = requests
        .into_iter()
        .find_map(|request| match request {
            TransactionDataRequest::TransactionsInvolvingAddress(req)
                if req.address() == address =>
            {
                Some(req)
            }
            _ => None,
        })
        .unwrap();
    st.wallet_mut()
        .notify_address_checked(request, mined + 5)
        .unwrap();
    assert_eq!(spend_searches(&st, address), vec![]);
    st.wallet_mut().update_chain_tip(mined + 100).unwrap();
    assert_eq!(spend_searches(&st, address), vec![(mined + 6, mined + 47)]);
}

#[cfg(not(feature = "spend-index"))]
#[test]
fn completing_an_old_range_uses_expiry_at_the_current_tip() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let parent = funding(0xd6, address, 1_000_000);
    store(&mut st, &parent);
    let mined = tip(&st);
    let withheld = expiring_transaction(
        vec![outpoint(&parent, 0)],
        vec![(EXTERNAL, 990_000)],
        mined + 80,
    );
    store_unmined(&mut st, &withheld);
    st.wallet_mut().update_chain_tip(mined + 100).unwrap();
    let request = st
        .wallet()
        .transaction_data_requests()
        .unwrap()
        .into_iter()
        .find_map(|request| match request {
            TransactionDataRequest::TransactionsInvolvingAddress(req)
                if req.address() == address =>
            {
                Some(req)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(request.block_range_end(), Some(mined + 41));
    st.wallet_mut()
        .notify_address_checked(request, mined + 40)
        .unwrap();
    assert_eq!(spend_searches(&st, address), vec![(mined + 41, mined + 82)]);
}

#[cfg(not(feature = "spend-index"))]
#[test]
fn a_later_search_does_not_skip_an_earlier_output_at_the_same_address() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let earlier_parent = funding(0xd7, address, 1_000_000);
    store(&mut st, &earlier_parent);
    let earlier = outpoint(&earlier_parent, 0);
    let mined = tip(&st);
    let withheld =
        expiring_transaction(vec![earlier.clone()], vec![(EXTERNAL, 990_000)], mined + 2);
    store_unmined(&mut st, &withheld);
    scan_new_blocks(&mut st, 5);
    let later_parent = funding(0xd8, address, 2_000_000);
    store(&mut st, &later_parent);
    let later_received = tip(&st);
    scan_new_blocks(&mut st, 1);
    let checked = tip(&st);
    let request = st
        .wallet()
        .transaction_data_requests()
        .unwrap()
        .into_iter()
        .find_map(|request| match request {
            TransactionDataRequest::TransactionsInvolvingAddress(req)
                if req.address() == address && req.block_range_start() == later_received =>
            {
                Some(req)
            }
            _ => None,
        })
        .unwrap();
    st.wallet_mut()
        .notify_address_checked(request, checked)
        .unwrap();
    // Completing the later output's range leaves the earlier output's entire search pending.
    assert_eq!(spend_searches(&st, address), vec![(mined, checked + 1)]);
    let actual = transaction(vec![earlier.clone()], vec![(EXTERNAL, 990_000)]);
    store(&mut st, &actual);
    assert_eq!(spend_searches(&st, address), vec![]);
    assert!(!spendable(&st, &earlier));
}

#[cfg(feature = "spend-index")]
fn spending_outpoints(st: &State) -> Vec<OutPoint> {
    st.wallet()
        .transaction_data_requests()
        .unwrap()
        .into_iter()
        .filter_map(|request| match request {
            TransactionDataRequest::GetSpendingTx(outpoint) => Some(outpoint),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "spend-index")]
#[test]
fn per_outpoint_search_resumes_and_completes_after_expiry() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let parent = funding(0xd9, address, 1_000_000);
    store(&mut st, &parent);
    let received = outpoint(&parent, 0);
    let mined = tip(&st);
    let withheld =
        expiring_transaction(vec![received.clone()], vec![(EXTERNAL, 990_000)], mined + 2);
    store_unmined(&mut st, &withheld);
    assert_eq!(spending_outpoints(&st), vec![]);
    scan_new_blocks(&mut st, 3);
    assert_eq!(spending_outpoints(&st), vec![received.clone()]);
    let checked = tip(&st);
    st.wallet_mut()
        .notify_output_verified_unspent(received.clone(), checked)
        .unwrap();
    assert_eq!(spending_outpoints(&st), vec![]);
    scan_new_blocks(&mut st, 1);
    assert_eq!(spending_outpoints(&st), vec![received.clone()]);
    let actual = transaction(vec![received.clone()], vec![(EXTERNAL, 990_000)]);
    store(&mut st, &actual);
    assert_eq!(spending_outpoints(&st), vec![]);
    assert!(!spendable(&st, &received));
}

#[cfg(feature = "spend-index")]
#[test]
fn per_outpoint_completion_does_not_advance_another_outputs_search() {
    let (mut st, accounts) = public_wallet(0);
    let address = external_of(&st, accounts[0]);
    let earlier_parent = funding(0xda, address, 1_000_000);
    let later_parent = funding(0xdb, address, 2_000_000);
    store(&mut st, &earlier_parent);
    store(&mut st, &later_parent);
    let earlier = outpoint(&earlier_parent, 0);
    let later = outpoint(&later_parent, 0);
    let mined = tip(&st);
    let withheld = expiring_transaction(
        vec![earlier.clone(), later.clone()],
        vec![(EXTERNAL, 2_990_000)],
        mined + 2,
    );
    store_unmined(&mut st, &withheld);
    scan_new_blocks(&mut st, 3);
    let pending = spending_outpoints(&st);
    assert_eq!(pending.len(), 2);
    assert!(pending.contains(&earlier));
    assert!(pending.contains(&later));
    let checked = tip(&st);
    st.wallet_mut()
        .notify_output_verified_unspent(later.clone(), checked)
        .unwrap();
    assert_eq!(spending_outpoints(&st), vec![earlier.clone()]);
    let actual = transaction(vec![earlier.clone()], vec![(EXTERNAL, 990_000)]);
    store(&mut st, &actual);
    assert_eq!(spending_outpoints(&st), vec![]);
    assert!(!spendable(&st, &earlier));
    assert!(spendable(&st, &later));
}
