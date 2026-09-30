//! Qualification: an independent block-derived oracle, upgrades of representative wallets,
//! failure injection, and chain and account lifecycle.

use super::*;

mod failpoints;
mod lifecycle;
mod migration;
mod oracle;

/// Every row of every table, the ledger's included, in a canonical order.
fn full_dump(conn: &Connection) -> Vec<(String, Vec<String>)> {
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite!_%' ESCAPE '!'
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
            let columns = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<Result<Vec<_>, _>>()
                        .map(|values| format!("{values:?}"))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            rows.sort();
            (table, rows)
        })
        .collect()
}

/// Asserts that `result` failed because the database could not grow.
fn assert_disk_full<T: std::fmt::Debug>(result: Result<T, SqliteClientError>) {
    match result {
        Err(SqliteClientError::DbError(rusqlite::Error::SqliteFailure(e, _))) => {
            assert_eq!(e.code, rusqlite::ErrorCode::DiskFull)
        }
        other => panic!("expected a full disk, got {other:?}"),
    }
}

/// Caps the database at its current size, so that any write needing a new page fails as a full
/// disk would. The free list is emptied first, so that no write can reuse freed pages.
fn fill_disk(st: &State) {
    let conn = conn(st);
    conn.execute_batch("VACUUM").unwrap();
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    let cap: i64 = conn
        .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cap, pages);
}

/// Lifts [`fill_disk`]'s cap.
fn free_disk(st: &State) {
    conn(st)
        .query_row("PRAGMA max_page_count = 4294967294", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
}
