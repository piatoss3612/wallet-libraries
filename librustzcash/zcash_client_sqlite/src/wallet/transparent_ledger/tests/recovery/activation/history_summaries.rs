//! Library-owned activity summaries: independent amounts, view/Vizor equivalence and read costs.
use std::time::Instant;

use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization},
    params,
};

use super::{public_fixtures::*, *};
use crate::{
    WalletDb,
    wallet::history::{SUMMARY_QUERY, TransactionSummary, read_summary},
};

const VIZOR_BASELINE: &str = include_str!("vizor_history_baseline.sql");

fn summaries(st: &State, account: AccountUuid) -> Vec<TransactionSummary> {
    st.wallet()
        .db()
        .transaction_history_summaries(account)
        .unwrap()
}

fn summary(
    st: &State,
    account: AccountUuid,
    tx: &zcash_primitives::transaction::Transaction,
) -> TransactionSummary {
    summaries(st, account)
        .into_iter()
        .find(|s| s.txid == tx.txid())
        .unwrap()
}

fn assert_equivalent(st: &State, account: AccountUuid) {
    let mut expected = conn(st)
        .prepare(
            "SELECT vt.*, tx.id_tx AS transaction_id, tx.created,
         CAST(strftime('%s', tx.created) AS INTEGER) AS created_time,
         EXISTS (SELECT 1 FROM orchard_received_note_spends s
             JOIN orchard_received_notes n ON n.id = s.orchard_received_note_id
             WHERE s.transaction_id = tx.id_tx AND n.note_version = 2) AS has_orchard_spend
         FROM v_transactions vt JOIN transactions tx ON tx.txid = vt.txid
         WHERE vt.account_uuid = ?1",
        )
        .unwrap()
        .query_map([account.expose_uuid().as_bytes()], read_summary)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut actual = summaries(st, account);
    expected.sort_by_key(|s| s.transaction_id);
    actual.sort_by_key(|s| s.transaction_id);
    assert_eq!(actual, expected);

    // Independently preserve the fields Vizor currently consumes. NULL expiry/timestamps stay
    // optional in the library; Vizor's existing presentation defaults are applied here only.
    let mut old = conn(st).prepare(VIZOR_BASELINE).unwrap();
    let old = old
        .query_map(params![account.expose_uuid().as_bytes(), 2], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<u32>>(2)?,
                row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                row.get::<_, i64>(4)?,
                row.get::<_, Option<u64>>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, u64>(7)?,
                row.get::<_, u64>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, Option<u32>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, bool>(14)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(old.len(), actual.len());
    for old in old {
        let s = actual
            .iter()
            .find(|s| s.txid.as_ref().as_slice() == old.0)
            .unwrap();
        assert_eq!(
            (
                s.transaction_id,
                s.mined_height.map(u32::from),
                s.expired_unmined,
                s.account_balance_delta,
                s.fee,
                s.block_time.unwrap_or(0),
                s.total_spent
            ),
            (old.1, old.2, old.3, old.4, old.5, old.6, old.7)
        );
        assert_eq!(
            (
                s.total_received,
                s.is_shielding,
                s.expiry_height.map(u32::from),
                s.tx_index.map(i64::from).unwrap_or(-1),
                s.created.clone(),
                s.created_time.unwrap_or(0),
                s.has_orchard_spend
            ),
            (old.8, old.9, old.10, old.11, old.12, old.13, old.14)
        );
    }
}

#[test]
fn history_summaries_count_equal_outputs_and_multiple_inputs_once() {
    let (mut st, accounts) = public_wallet(1);
    let (sender, recipient) = (accounts[0], accounts[1]);
    let first = funding(0xc0, external_of(&st, sender), 500_000);
    let second = funding(0xc1, external_of(&st, sender), 500_000);
    let send = transaction(
        vec![outpoint(&first, 0), outpoint(&second, 0)],
        vec![
            (external_of(&st, recipient), 200_000),
            (external_of(&st, recipient), 200_000),
            (EXTERNAL, 200_000),
            (internal_of(&st, sender), 390_000),
        ],
    );
    for tx in [&first, &second, &send] {
        store(&mut st, tx);
    }
    let s = summary(&st, sender, &send);
    assert_eq!(
        (s.account_balance_delta, s.total_spent, s.total_received),
        (-610_000, 1_000_000, 390_000)
    );
    assert_eq!(
        (s.spent_note_count, s.sent_note_count, s.received_note_count),
        (2, 4, 1)
    );
    let r = summary(&st, recipient, &send);
    assert_eq!(
        (r.account_balance_delta, r.total_spent, r.total_received),
        (400_000, 0, 400_000)
    );
    assert_eq!(r.received_note_count, 2);
    assert_eq!(summaries(&st, recipient).len(), 1);
    assert_equivalent(&st, sender);
    assert_equivalent(&st, recipient);
}

#[test]
fn history_summaries_preserve_status_unknowns_and_local_metadata() {
    let (mut st, accounts) = public_wallet(0);
    let account = accounts[0];
    assert!(summaries(&st, account).is_empty());
    assert!(matches!(
        st.wallet()
            .db()
            .transaction_history_summaries(AccountUuid::from_uuid(uuid::Uuid::nil())),
        Err(SqliteClientError::AccountUnknown)
    ));
    let tx = funding(0xc2, external_of(&st, account), 50_000);
    store(&mut st, &tx);
    conn(&st)
        .execute(
            "UPDATE transactions SET mined_height = NULL, tx_index = NULL,
        expiry_height = NULL, fee = NULL, created = '2026-01-02 03:04:05', trust_status = 1
        WHERE txid = ?1",
            [tx.txid().as_ref()],
        )
        .unwrap();
    let s = summary(&st, account, &tx);
    assert_eq!(
        (
            s.mined_height,
            s.tx_index,
            s.expiry_height,
            s.fee,
            s.block_time
        ),
        (None, None, None, None, None)
    );
    assert!(!s.expired_unmined);
    assert!(s.is_trusted);
    assert_eq!(s.created_time, Some(1_767_323_045));
    assert!(!s.has_orchard_spend);
    assert_equivalent(&st, account);
    conn(&st)
        .execute(
            "UPDATE transactions SET expiry_height = 1 WHERE txid = ?1",
            [tx.txid().as_ref()],
        )
        .unwrap();
    assert!(summary(&st, account, &tx).expired_unmined);
    assert_equivalent(&st, account);
    conn(&st)
        .execute(
            "UPDATE transactions SET expiry_height = 0 WHERE txid = ?1",
            [tx.txid().as_ref()],
        )
        .unwrap();
    assert!(!summary(&st, account, &tx).expired_unmined);
}

#[test]
fn history_summaries_never_authorize_raw_payload_reads() {
    let (mut st, accounts) = public_wallet(0);
    let tx = funding(0xc3, external_of(&st, accounts[0]), 50_000);
    store(&mut st, &tx);
    conn(&st).authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Read {
            table_name: "transactions",
            column_name: "raw",
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    assert_eq!(summaries(&st, accounts[0]).len(), 1);
    assert!(conn(&st).prepare("SELECT * FROM v_transactions").is_err());
    conn(&st).authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
}

#[test]
fn history_summaries_share_the_callers_snapshot_with_detail_reads() {
    let (mut st, accounts) = public_wallet(0);
    let tx = funding(0xc4, external_of(&st, accounts[0]), 50_000);
    store(&mut st, &tx);
    conn(&st)
        .execute_batch("PRAGMA journal_mode = WAL")
        .unwrap();
    let writer = rusqlite::Connection::open(conn(&st).path().unwrap()).unwrap();
    let read = conn(&st).unchecked_transaction().unwrap();
    let db = WalletDb::from_connection(&*read, *st.network(), (), ())
        .with_transparent_ledger_mode(Public);
    let before = db.transaction_history_summaries(accounts[0]).unwrap();
    let details = db
        .transaction_history_details(accounts[0], &[tx.txid()])
        .unwrap();
    writer
        .execute(
            "UPDATE transactions SET fee = 12345 WHERE txid = ?1",
            [tx.txid().as_ref()],
        )
        .unwrap();
    assert_eq!(
        db.transaction_history_summaries(accounts[0]).unwrap(),
        before
    );
    assert_eq!(
        db.transaction_history_details(accounts[0], &[tx.txid()])
            .unwrap(),
        details
    );
    drop(read);
    assert_eq!(summary(&st, accounts[0], &tx).fee, Some(12345));
}

#[test]
fn history_summaries_apply_account_predicates_in_each_accounting_input() {
    // EXPLAIN validates the actual expanded query and the bound account, rather than a macro
    // token check. The materialized accounting inputs must use an account index at the leaves.
    let (st, accounts) = public_wallet(1);
    let account_id: i64 = conn(&st)
        .query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            [accounts[0].expose_uuid().as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    let plan = conn(&st)
        .prepare(&format!("EXPLAIN QUERY PLAN {SUMMARY_QUERY}"))
        .unwrap()
        .query_map([account_id], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let account_searches = plan
        .iter()
        .filter(|line| line.contains("account_id=?") || line.contains("from_account_id=?"))
        .count();
    assert!(
        account_searches >= 3,
        "accounting inputs must filter the account early: {plan:#?}"
    );
}

#[test]
#[ignore = "representative timing experiment; run with --ignored --nocapture"]
fn history_summaries_multi_account_benchmark() {
    let (mut st, accounts) = public_wallet(7);
    // Keep real migrated schema/receivers, but use deterministic bulk history to isolate read
    // cost from decryption. Eight accounts, 1000 transactions each, 32 KiB raw payloads (250 MiB).
    let connection = conn(&st);
    let populate = connection.unchecked_transaction().unwrap();
    for (a, account) in accounts.iter().enumerate() {
        let account_id: i64 = populate
            .query_row(
                "SELECT id FROM accounts WHERE uuid = ?1",
                [account.expose_uuid().as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let address = external_of(&st, *account);
        let address_id: i64 = populate.query_row("SELECT id FROM addresses WHERE account_id = ?1 AND cached_transparent_receiver_address = ?2",
            params![account_id, address.encode(st.network())], |r| r.get(0)).unwrap();
        for i in 0..1000u32 {
            let mut txid = [0u8; 32];
            txid[..4].copy_from_slice(&i.to_le_bytes());
            txid[4] = a as u8;
            populate.execute("INSERT INTO transactions (txid, raw, expiry_height) VALUES (?1, zeroblob(32768), 1000000)", [txid]).unwrap();
            let id = populate.last_insert_rowid();
            populate.execute("INSERT INTO transparent_received_outputs (transaction_id, output_index, account_id, address, script, value_zat, max_observed_unspent_height, address_id) VALUES (?1, 0, ?2, ?3, ?4, 50000, 1, ?5)",
                params![id, account_id, address.encode(st.network()), address.script().0, address_id]).unwrap();
        }
    }
    populate.commit().unwrap();
    let rounds = 5;
    let time = |name: &str, f: &mut dyn FnMut() -> usize| {
        assert_eq!(f(), 1000); // Warm up before measuring.
        let start = Instant::now();
        for _ in 0..rounds {
            assert_eq!(f(), 1000);
        }
        eprintln!(
            "{name}: {:.2} ms/read ({rounds} warm reads)",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds)
        );
    };
    time("typed account summaries", &mut || {
        summaries(&st, accounts[0]).len()
    });
    time("Vizor's current CTE", &mut || {
        conn(&st)
            .prepare(VIZOR_BASELINE)
            .unwrap()
            .query_map(params![accounts[0].expose_uuid().as_bytes(), 2], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap()
            .count()
    });
    time("v_transactions with raw projection", &mut || {
        conn(&st)
            .prepare("SELECT * FROM v_transactions WHERE account_uuid = ?1")
            .unwrap()
            .query_map([accounts[0].expose_uuid().as_bytes()], |r| {
                r.get::<_, Vec<u8>>(5)
            })
            .unwrap()
            .count()
    });
    // Hold st alive until all queries are complete (the file is temporary).
    let _ = &mut st;
}
