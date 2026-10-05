//! Funding attribution: which account a transaction's sent outputs are recorded for, whatever
//! order the wallet discovers a transaction and the outputs it spends in.

use zcash_primitives::transaction::Transaction;

use super::public_fixtures::*;
use super::*;

/// `(account_balance_delta, total_spent, total_received)` of `account`'s row for `tx`.
fn movement(st: &State, account: AccountUuid, tx: &Transaction) -> (i64, i64, i64) {
    conn(st)
        .query_row(
            "SELECT account_balance_delta, total_spent, total_received FROM v_transactions
             WHERE txid = ?1 AND account_uuid = ?2",
            rusqlite::params![tx.txid().as_ref(), account.expose_uuid().as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

/// A send of 600_000 to an outside address with 390_000 of change, spending a 1_000_000 receive.
fn send_with_change(st: &State, account: AccountUuid) -> (Transaction, Transaction) {
    let parent = funding(0xa0, external_of(st, account), 1_000_000);
    let send = transaction(
        vec![outpoint(&parent, 0)],
        vec![(EXTERNAL, 600_000), (internal_of(st, account), 390_000)],
    );
    (parent, send)
}

#[test]
fn a_send_with_own_change_moves_the_account_once() {
    let (mut st, accounts) = public_wallet(0);
    let account = accounts[0];
    let (parent, send) = send_with_change(&st, account);
    store(&mut st, &parent);
    store(&mut st, &send);

    // The change is an output the account both sent and received; it must not multiply the
    // send's rows in `v_transactions`.
    assert_eq!(
        sent_outputs(&st, &send),
        vec![
            (account, 0, None, 600_000),
            (account, 1, Some(account), 390_000)
        ]
    );
    assert_eq!(
        movement(&st, account, &send),
        (-610_000, 1_000_000, 390_000)
    );
}

#[test]
fn a_transfer_to_another_account_moves_each_account_once() {
    let (mut st, accounts) = public_wallet(1);
    let (sender, recipient) = (accounts[0], accounts[1]);
    let parent = funding(0xa2, external_of(&st, sender), 1_000_000);
    let transfer = transaction(
        vec![outpoint(&parent, 0)],
        vec![
            (external_of(&st, recipient), 400_000),
            (internal_of(&st, sender), 590_000),
        ],
    );
    store(&mut st, &parent);
    store(&mut st, &transfer);

    assert_eq!(
        sent_outputs(&st, &transfer),
        vec![
            (sender, 0, Some(recipient), 400_000),
            (sender, 1, Some(sender), 590_000)
        ]
    );
    assert_eq!(
        movement(&st, sender, &transfer),
        (-410_000, 1_000_000, 590_000)
    );
    assert_eq!(movement(&st, recipient, &transfer), (400_000, 0, 400_000));
}
