//! One-time IVK state that a rewind repairs in every build. The tables exist in every
//! build, and an `orchard` build may reopen a wallet that a build without it rewound, so
//! the repair cannot wait for `orchard`.

use rusqlite::{Connection, OptionalExtension, params};
use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;
use zcash_protocol::consensus::BlockHeight;

use crate::{AccountUuid, error::SqliteClientError};

/// `ironwood_one_time_publications.state` until the scan stores the publication's last block.
pub(crate) const PROVISIONAL: u8 = 0;
/// `ironwood_one_time_publications.state` once that block matched.
pub(crate) const ACCEPTED: u8 = 1;

/// Repairs one-time IVK state for a rewind that rescans every block above `height`.
/// Open keys start at the first rescanned block at the latest, which
/// `wallet::truncate_to_height_internal` sets after this in every build. A closed key that
/// scanned reopens if the rewind leaves one of its receipts short of the confirmations
/// closing requires (see `WalletDb::close_finished_one_time_keys`), and closes again once
/// that receipt is confirmed again. A key closed without scanning, whose note a lookup
/// found, reopens from its first note above `height`, so the rescan finds the note again,
/// and closes the same way. Discovery and the spend journal are repaired too (see
/// [`truncate_discovery`]).
pub(crate) fn truncate(
    conn: &rusqlite::Transaction<'_>,
    height: BlockHeight,
) -> Result<(), SqliteClientError> {
    let confirmed_below = u32::from(height)
        .saturating_add(1)
        .saturating_sub(ConfirmationsPolicy::default().untrusted().get());
    conn.execute(
        "UPDATE ironwood_one_time_keys SET closed_at = NULL
         WHERE closed_at IS NOT NULL AND active_from IS NOT NULL AND id IN (
             SELECT n.one_time_key_id FROM ironwood_received_notes n
             JOIN transactions t ON t.id_tx = n.transaction_id
             WHERE t.mined_height > ?1)",
        [confirmed_below],
    )?;
    let note_above = "SELECT MIN(t.mined_height) FROM ironwood_received_notes n
         JOIN transactions t ON t.id_tx = n.transaction_id
         WHERE n.one_time_key_id = ironwood_one_time_keys.id AND t.mined_height > ?1";
    conn.execute(
        &format!(
            "UPDATE ironwood_one_time_keys SET closed_at = NULL, active_from = ({note_above})
             WHERE closed_at IS NOT NULL AND active_from IS NULL
               AND ({note_above}) IS NOT NULL"
        ),
        [u32::from(height)],
    )?;
    truncate_discovery(conn, height)?;
    // The spend journal's entries and coverage above the floor may describe replaced blocks.
    let floor = u32::from(height);
    conn.execute(
        "DELETE FROM ironwood_spend_journal WHERE block_height > ?1",
        [floor],
    )?;
    conn.execute(
        "DELETE FROM ironwood_journal_coverage WHERE start_height > ?1",
        [floor],
    )?;
    conn.execute(
        "UPDATE ironwood_journal_coverage SET end_height = ?1 + 1 WHERE end_height > ?1 + 1",
        [floor],
    )?;
    Ok(())
}

/// Repairs discovery for a rewind whose blocks above `rescan_floor` may be replaced: a
/// publication ending there is judged again, a late candidate there becomes a hint again
/// (its block is rescanned), a path anchored there is requested again, and a cursor that
/// passed a record there restarts, since the replacement chain may put a record before it.
fn truncate_discovery(
    conn: &rusqlite::Transaction<'_>,
    rescan_floor: BlockHeight,
) -> Result<(), SqliteClientError> {
    let floor = u32::from(rescan_floor);
    conn.execute(
        "UPDATE ironwood_one_time_publications SET state = ?2 WHERE state = ?3 AND height > ?1",
        params![floor, PROVISIONAL, ACCEPTED],
    )?;
    conn.execute(
        "UPDATE ironwood_one_time_payment_recovery
         SET scan_hint = 1, witness = NULL, witness_height = NULL, witness_hash = NULL
         WHERE height > ?1",
        [floor],
    )?;
    conn.execute(
        "UPDATE ironwood_one_time_payment_recovery
         SET witness = NULL, witness_height = NULL, witness_hash = NULL
         WHERE witness_height > ?1",
        [floor],
    )?;
    restart_cursor(
        conn,
        "SELECT one_time_key_id FROM ironwood_one_time_lookups WHERE last_height > ?1",
        floor,
    )?;
    Ok(())
}

/// Reopens `account`'s discovery after a rewind lowers its birthday (see [`reopen`]).
pub(crate) fn reopen_discovery(
    conn: &rusqlite::Transaction<'_>,
    account: AccountUuid,
) -> Result<(), SqliteClientError> {
    let owner: Option<i64> = conn
        .query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            [account.0],
            |r| r.get(0),
        )
        .optional()?;
    match owner {
        Some(owner) => reopen(conn, owner),
        None => Ok(()),
    }
}

/// Reopens account `owner`'s discovery after its birthday was lowered: the next pass checks
/// every key again and every paid key's history from the start, since outcomes outside the
/// old range no longer hold. The account is no longer treated as fresh.
fn reopen(conn: &rusqlite::Transaction<'_>, owner: i64) -> Result<(), SqliteClientError> {
    conn.execute(
        "DELETE FROM ironwood_one_time_discovery WHERE account_id = ?1",
        [owner],
    )?;
    conn.execute(
        "DELETE FROM ironwood_one_time_fresh_accounts WHERE account_id = ?1",
        [owner],
    )?;
    restart_cursor(
        conn,
        "SELECT id FROM ironwood_one_time_keys WHERE account_id = ?1",
        owner,
    )?;
    // Until the next pass makes them due, no lookup runs.
    conn.execute(
        "UPDATE ironwood_one_time_lookups SET cutoff_height = NULL, done_height = NULL
         WHERE one_time_key_id IN (SELECT id FROM ironwood_one_time_keys WHERE account_id = ?1)",
        [owner],
    )?;
    Ok(())
}

/// Restarts from page zero the lookup cursors that SQL `keys` selects, with `?1` bound to
/// `param`, and makes each due again at the cutoff it last finished or still has.
pub(crate) fn restart_cursor(
    conn: &Connection,
    keys: &str,
    param: impl rusqlite::ToSql,
) -> Result<(), SqliteClientError> {
    conn.execute(
        &format!(
            "UPDATE ironwood_one_time_lookups
             SET applied = 0, last_position = NULL, last_nullifier = NULL, last_height = NULL,
                 cutoff_height = COALESCE(cutoff_height, done_height), done_height = NULL
             WHERE one_time_key_id IN ({keys})"
        ),
        [&param],
    )?;
    conn.execute(
        &format!("DELETE FROM ironwood_one_time_lookup_reads WHERE one_time_key_id IN ({keys})"),
        [&param],
    )?;
    // Reading again from page zero replaces what the earlier reads queued, so a corrected
    // answer is not refused as a conflict. Admitted notes stay.
    conn.execute(
        &format!(
            "DELETE FROM ironwood_one_time_payment_recovery WHERE one_time_key_id IN ({keys})"
        ),
        [&param],
    )?;
    Ok(())
}
