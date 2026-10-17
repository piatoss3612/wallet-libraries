use zcash_client_backend::proto::compact_formats::CompactTx;

use super::*;

/// The journal's entries as `(height, tx_index, txid, ironwood start position)`.
fn entries(st: &State) -> Vec<(u32, u16, Vec<u8>, u64)> {
    rows(
        &st.wallet().db().conn,
        "SELECT block_height, tx_index, txid, ironwood_start_position
         FROM ironwood_spend_journal ORDER BY block_height, tx_index",
        [],
    )
}

/// The nullifiers journaled for the transaction at `height`, in action order.
fn journaled_nullifiers(st: &State, height: BlockHeight) -> Vec<[u8; 32]> {
    column(
        &st.wallet().db().conn,
        "SELECT nf FROM ironwood_spend_journal_nullifiers
         WHERE block_height = ?1 ORDER BY tx_index, action_index",
        [u32::from(height)],
    )
}

/// The test account's covered ranges as `(start, end)`, end exclusive.
fn coverage(st: &State) -> Vec<(u32, u32)> {
    let account = st.test_account().unwrap().id();
    rows(
        &st.wallet().db().conn,
        "SELECT c.start_height, c.end_height FROM ironwood_journal_coverage c
         JOIN accounts a ON a.id = c.account_id
         WHERE a.uuid = ?1 ORDER BY c.start_height",
        [account.expose_uuid()],
    )
}

/// Mines a transaction that reveals a nullifier the wallet doesn't know and pays change
/// back to the test account, then returns its height and that nullifier. The wallet
/// recognizes it only by its change output.
fn spend_unknown_note(st: &mut State) -> (BlockHeight, [u8; 32]) {
    let stranger = IronwoodFvk(derive(st, KeyId::new(Purpose::Refund, 9_999)));
    let value = Zatoshis::const_from_u64(50_000);
    let (_, _, nf) = st.generate_next_block(&stranger, AddressType::DefaultExternal, value);
    let ordinary = IronwoodFvk(parent(st));
    let paid = Zatoshis::const_from_u64(20_000);
    let to = unified(foreign_receiver());
    let (height, _) = st.generate_next_block_spending(&ordinary, (nf, value), to, paid);
    (height, nf.to_bytes())
}

/// An account with no one-time state journals each transaction its plain scan
/// recognizes, with every Ironwood nullifier, including one it can't match to a note.
#[test]
fn a_recognized_transaction_is_journaled_with_every_nullifier() {
    let mut st = ironwood_wallet();
    let start = st.sapling_activation_height();
    let (height, nf) = spend_unknown_note(&mut st);
    st.scan_cached_blocks(start, 2);

    let journaled = entries(&st);
    assert_eq!(journaled.len(), 1);
    let (entry_height, _, txid, position) = &journaled[0];
    assert_eq!(*entry_height, u32::from(height));
    let prior_size: u64 = value(
        &st.wallet().db().conn,
        "SELECT ironwood_commitment_tree_size FROM blocks WHERE height = ?1",
        [u32::from(height) - 1],
    );
    assert_eq!(*position, prior_size);
    assert_eq!(journaled_nullifiers(&st, height)[0], nf);
    let compact: Vec<u8> = value(
        &st.wallet().db().conn,
        "SELECT compact_tx FROM ironwood_spend_journal WHERE block_height = ?1",
        [u32::from(height)],
    );
    let compact = CompactTx::decode(&compact[..]).unwrap();
    assert_eq!(compact.txid().as_ref(), &txid[..]);
    assert_eq!(compact.ironwood_actions[0].nullifier, nf);
    assert_eq!(coverage(&st), [(u32::from(start), u32::from(height) + 1)]);
}

/// A transaction the wallet doesn't recognize isn't journaled, but its block is covered.
#[test]
fn unrelated_transactions_are_covered_but_not_journaled() {
    let mut st = ironwood_wallet();
    let start = st.sapling_activation_height();
    let stranger = IronwoodFvk(derive(&st, KeyId::new(Purpose::Refund, 9_999)));
    let value = Zatoshis::const_from_u64(50_000);
    st.generate_next_block(&stranger, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(start, 1);
    assert!(entries(&st).is_empty());
    assert_eq!(coverage(&st), [(u32::from(start), u32::from(start) + 1)]);
}

/// Batches scanned out of order merge into one covered range once they meet, and a
/// block scanned twice keeps one entry.
#[test]
fn coverage_merges_and_rescans_replace_entries() {
    let mut st = ironwood_wallet();
    let start = st.sapling_activation_height();
    let (height, _) = spend_unknown_note(&mut st);
    st.generate_empty_block();
    st.scan_cached_blocks(height + 1, 1);
    st.scan_cached_blocks(height, 1);
    let actions = journaled_nullifiers(&st, height).len();
    assert_eq!(coverage(&st), [(u32::from(height), u32::from(height) + 2)]);
    st.scan_cached_blocks(start, 2);
    assert_eq!(coverage(&st), [(u32::from(start), u32::from(height) + 2)]);
    assert_eq!(entries(&st).len(), 1);
    assert_eq!(journaled_nullifiers(&st, height).len(), actions);
}

/// A rewind removes the entries and coverage above its floor, and survives a reopen.
#[test]
fn a_rewind_removes_entries_and_coverage_above_its_floor() {
    let mut st = ironwood_wallet();
    let start = st.sapling_activation_height();
    let (first, _) = spend_unknown_note(&mut st);
    let (second, _) = spend_unknown_note(&mut st);
    st.scan_cached_blocks(start, 4);
    assert_eq!(entries(&st).len(), 2);

    st.truncate_to_height(second - 1);
    reopen(&mut st);
    let kept = entries(&st);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].0, u32::from(first));
    assert!(journaled_nullifiers(&st, second).is_empty());
    assert_eq!(coverage(&st), [(u32::from(start), u32::from(second))]);
}

/// An account added after blocks were scanned isn't covered for them, and deleting an
/// account drops its coverage. (An account born below scanned blocks rewinds them, which
/// clears their coverage for every account.)
#[test]
fn coverage_is_per_account() {
    let mut st = ironwood_wallet();
    let start = st.sapling_activation_height();
    st.generate_empty_block();
    st.scan_cached_blocks(start, 1);

    let seed = SecretVec::new(st.test_seed().unwrap().expose_secret().clone());
    // Born after the scanned block, so adding it rewinds nothing.
    let prior = st.latest_cached_block().unwrap().chain_state().clone();
    let birthday = zcash_client_backend::data_api::AccountBirthday::from_parts(prior, None);
    let (added, _) = st
        .wallet_mut()
        .db_mut()
        .create_account("added", &seed, &birthday, None)
        .unwrap();
    st.generate_empty_block();
    st.scan_cached_blocks(start + 1, 1);

    let ranges = |st: &State, account: AccountUuid| -> Vec<(u32, u32)> {
        rows(
            &st.wallet().db().conn,
            "SELECT c.start_height, c.end_height FROM ironwood_journal_coverage c
             JOIN accounts a ON a.id = c.account_id WHERE a.uuid = ?1",
            [account.expose_uuid()],
        )
    };
    let next = u32::from(start) + 1;
    assert_eq!(ranges(&st, added), [(next, next + 1)]);
    assert_eq!(coverage(&st), [(u32::from(start), next + 1)]);

    st.wallet_mut().db_mut().delete_account(added).unwrap();
    let left: i64 = value(
        &st.wallet().db().conn,
        "SELECT COUNT(*) FROM ironwood_journal_coverage",
        [],
    );
    assert_eq!(left, 1);
}
