//! Reopening an active private wallet after a writer that does not maintain ledger facts.

use super::*;
use crate::wallet::init::WalletMigrator;

fn initialize(
    st: &mut State,
) -> Result<(), schemerz::MigratorError<uuid::Uuid, crate::wallet::init::WalletMigrationError>> {
    WalletMigrator::new().init_or_migrate(st.wallet_mut().db_mut())
}

fn assert_refused(st: &mut State, account: AccountUuid, context: &str) {
    assert!(
        matches!(
            initialize(st),
            Err(schemerz::MigratorError::Adapter(crate::wallet::init::WalletMigrationError::CorruptedData(message)))
                if message == "private transparent projections disagree with retained ledger facts"
        ),
        "{context}"
    );
    assert_eq!(count(st, "tpir_legacy_writes"), 1);
    assert_eq!(
        conn(st)
            .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(matches!(
        st.wallet()
            .db()
            .transparent_ledger_snapshot(account, ConfirmationsPolicy::MIN),
        Err(SqliteClientError::LegacyWritesUnreconciled)
    ));
    // Ignoring the initialization error or retrying cannot restore financial authority.
    assert!(initialize(st).is_err());
    assert_eq!(count(st, "tpir_legacy_writes"), 1);
}

#[test]
fn legacy_utxo_overwrite_cannot_retain_private_authority() {
    let (mut st, account, unspent, _) = ready_wallet();
    promote(&mut st, account).unwrap();
    initialize(&mut st).unwrap();
    assert_eq!(
        snapshot(&st, account).authority,
        TransparentAuthority::Private
    );
    let before = snapshot(&st, account).authorized.unwrap();

    // rc7's put_transparent_output upsert overwrites the value of an existing outpoint,
    // retaining its row ID and private origins, without updating zip318_kind.
    conn(&st)
        .execute(
            "UPDATE transparent_received_outputs SET value_zat = 900000
         WHERE transaction_id = (SELECT id_tx FROM transactions WHERE txid = ?1)
         AND output_index = ?2",
            rusqlite::params![unspent.outpoint.hash(), unspent.outpoint.n()],
        )
        .unwrap();
    assert_eq!(count(&st, "tpir_legacy_writes"), 0);
    assert_refused(&mut st, account, "legacy value overwrite");
    assert_eq!(
        super::super::super::output_origins(conn(&st), &unspent.outpoint),
        vec![2]
    );

    // Restoring the verified value makes the retained projection usable again; neither
    // coverage nor origins were fabricated, discarded, or rewritten by the refusal.
    conn(&st)
        .execute(
            "UPDATE transparent_received_outputs SET value_zat = ?1
         WHERE transaction_id = (SELECT id_tx FROM transactions WHERE txid = ?2)
         AND output_index = ?3",
            rusqlite::params![
                i64::try_from(u64::from(unspent.value)).unwrap(),
                unspent.outpoint.hash(),
                unspent.outpoint.n()
            ],
        )
        .unwrap();
    initialize(&mut st).unwrap();
    assert_eq!(count(&st, "tpir_legacy_writes"), 0);
    assert_eq!(snapshot(&st, account).authorized.unwrap(), before);
}

#[test]
fn legacy_changes_to_private_content_placement_or_spends_are_refused() {
    for mutation in [
        "UPDATE transparent_received_outputs SET script = X'00'",
        "UPDATE transparent_received_outputs SET account_id = (SELECT MAX(id) FROM accounts)",
        "UPDATE transactions SET mined_height = mined_height + 1, block = NULL",
        "UPDATE transactions SET mined_height = NULL, block = NULL, tx_index = NULL",
        "UPDATE transactions SET tx_index = 0",
        "DELETE FROM tpir_output_origins WHERE origin = 2",
        "DELETE FROM transparent_received_outputs",
        "DELETE FROM transparent_received_output_spends; DELETE FROM transparent_spend_map",
        "DELETE FROM tpir_spend_origins WHERE origin = 2",
    ] {
        let (mut st, account, _, _) = ready_wallet_with(1);
        promote(&mut st, account).unwrap();
        conn(&st).execute_batch(mutation).unwrap();
        assert_eq!(count(&st, "tpir_legacy_writes"), 0, "{mutation}");
        assert_refused(&mut st, account, mutation);
    }
}

#[test]
fn unresolved_private_spends_keep_their_pending_projection_on_reopen() {
    let (mut st, account, _, _) = ready_wallet();
    promote(&mut st, account).unwrap();
    let ws = watch(&st, account);
    let unknown = receive(8, external(&ws), 1_000, below_target(&ws, 4));
    let mut c = commit(&ws);
    c.spends = vec![spend(9, &unknown, below_target(&ws, 2))];
    apply(&mut st, c).unwrap();
    assert_eq!(count(&st, "transparent_spend_map"), 1);
    initialize(&mut st).unwrap();
    assert_eq!(count(&st, "tpir_legacy_writes"), 0);
    assert_eq!(count(&st, "transparent_spend_map"), 1);
    assert_eq!(
        snapshot(&st, account).authority,
        TransparentAuthority::Unavailable
    );
}

#[test]
fn private_coinbase_classification_must_survive_reopen() {
    let (mut st, account, unspent, _) = ready_wallet();
    promote(&mut st, account).unwrap();
    conn(&st)
        .execute(
            "UPDATE tpir_receive_events SET coinbase = 1 WHERE txid = ?1",
            [unspent.outpoint.hash()],
        )
        .unwrap();
    conn(&st)
        .execute(
            "UPDATE transactions SET tx_index = 0 WHERE txid = ?1",
            [unspent.outpoint.hash()],
        )
        .unwrap();
    initialize(&mut st).unwrap();
    conn(&st)
        .execute(
            "UPDATE transactions SET tx_index = NULL WHERE txid = ?1",
            [unspent.outpoint.hash()],
        )
        .unwrap();
    assert_refused(&mut st, account, "coinbase classification removed");
}

#[test]
fn unchanged_private_projections_survive_a_marked_legacy_store() {
    let (mut st, account, _, _) = ready_wallet();
    promote(&mut st, account).unwrap();
    let before = snapshot(&st, account).authorized.unwrap();
    conn(&st)
        .execute("UPDATE transactions SET zip318_kind = 0", [])
        .unwrap();
    assert_eq!(count(&st, "tpir_legacy_writes"), 1);
    initialize(&mut st).unwrap();
    assert_eq!(count(&st, "tpir_legacy_writes"), 0);
    assert_eq!(snapshot(&st, account).authorized.unwrap(), before);
}
