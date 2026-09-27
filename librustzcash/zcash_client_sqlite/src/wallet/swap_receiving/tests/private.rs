use super::apply::fixture;
use super::*;
use zcash_client_backend::data_api::WalletRead;

#[test]
fn private_recovery_retains_spend_history_and_does_not_queue_key_replay() {
    let (mut st, key, candidate, through, _) = fixture();
    let account = st.test_account().unwrap().id();
    st.wallet_mut()
        .db_mut()
        .enable_private_swap_recovery(account)
        .unwrap();
    st.wallet_mut()
        .db_mut()
        .watch_swap_receive_key(account, 9, candidate.height)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .block_fully_scanned()
            .unwrap()
            .unwrap()
            .block_height(),
        through.height
    );
    st.wallet_mut()
        .db_mut()
        .transactionally(|db| -> Result<(), SqliteClientError> {
            crate::wallet::prune_nullifier_map(db.conn.0, through.height + 101)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        st.wallet_mut()
            .db_mut()
            .swap_payment_spend_status(account, key.key_id(), &candidate, through)
            .unwrap(),
        SpendStatus::Unspent
    );
    assert!(
        st.wallet_mut()
            .db_mut()
            .mark_swap_directory_checked(account, key.key_id(), through)
            .is_err()
    );
    let other = KeyId::new(Purpose::Receive, 9);
    st.wallet_mut()
        .db_mut()
        .mark_swap_directory_checked(account, other, through)
        .unwrap();
    assert_eq!(
        st.wallet()
            .db()
            .swap_directory_check(account, other)
            .unwrap(),
        Some(through)
    );
}
