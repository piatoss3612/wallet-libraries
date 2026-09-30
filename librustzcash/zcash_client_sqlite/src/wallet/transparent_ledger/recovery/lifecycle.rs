use super::*;
use rusqlite::OptionalExtension as _;

/// Clears candidate state above `floor`, the height above which a truncation or rewind may
/// replace blocks. Callers pass the rescan floor, which can lie below the retained checkpoint.
///
/// Placements above the floor are cleared; the events stay and a later commit can place them
/// again. Pages opened for a later target are removed. Coverage anchored above the floor is
/// clipped to it and re-anchored at the floor block: the revision agreed with the old chain at
/// its anchor, and that chain equals the surviving one through `floor`. Without a local hash
/// for the floor block the coverage is deleted rather than given an invented anchor. Candidate
/// windows never shrink.
pub(crate) fn truncate(
    conn: &rusqlite::Connection,
    floor: BlockHeight,
) -> Result<(), SqliteClientError> {
    // A build that cannot interpret the wallet's ledger state cannot rewind it either: it would
    // leave whatever a newer reader maintains anchored on replaced blocks. Refusing fails the
    // whole rewind.
    super::super::durable_policy(conn)?;
    let height = u32::from(floor);
    conn.execute(
        "UPDATE tpir_receive_events SET mined_height = NULL WHERE mined_height > :height",
        named_params![":height": height],
    )?;
    conn.execute(
        "UPDATE tpir_spend_events SET mined_height = NULL WHERE mined_height > :height",
        named_params![":height": height],
    )?;
    conn.execute(
        "DELETE FROM tpir_pending_pages WHERE target_height > :height",
        named_params![":height": height],
    )?;
    let retained_hash: Option<Vec<u8>> = conn
        .query_row(
            "SELECT hash FROM blocks WHERE height = :height",
            named_params![":height": height],
            |row| row.get(0),
        )
        .optional()?;
    match retained_hash {
        Some(hash) => {
            conn.execute(
                "DELETE FROM tpir_coverage
                 WHERE anchor_height > :height AND from_height > :height",
                named_params![":height": height],
            )?;
            conn.execute(
                "UPDATE tpir_coverage
                 SET through_height = MIN(through_height, :height),
                     anchor_height = :height,
                     anchor_hash = :hash
                 WHERE anchor_height > :height",
                named_params![":height": height, ":hash": hash],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM tpir_coverage WHERE anchor_height > :height",
                named_params![":height": height],
            )?;
        }
    }
    Ok(())
}

/// Removes pending pages on a policy transition. A page is work started under the policy that
/// authorized it; after a transition it can be neither completed nor resumed.
pub(crate) fn clear_pending_pages(conn: &rusqlite::Connection) -> Result<(), SqliteClientError> {
    conn.execute("DELETE FROM tpir_pending_pages", [])?;
    Ok(())
}

/// Forgets `from_account`'s candidate state for a script that has just been re-attributed to
/// another account. The receiving account starts without coverage for it.
///
/// Runs inside address generation, which migrations also call; it is a no-op before the
/// recovery tables exist.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn forget_reattributed_script(
    conn: &rusqlite::Connection,
    from_account: AccountRef,
    address: &TransparentAddress,
) -> Result<(), SqliteClientError> {
    let has_tables: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tpir_coverage'
         )",
        [],
        |row| row.get(0),
    )?;
    if !has_tables {
        return Ok(());
    }
    // Only a build that interprets the wallet's ledger state may change it.
    durable_policy(conn)?;
    let params = named_params![":account_id": from_account.0, ":script": script_bytes(address)];
    conn.execute(
        "DELETE FROM tpir_receive_events WHERE account_id = :account_id AND script = :script",
        params,
    )?;
    conn.execute(
        "DELETE FROM tpir_spend_events
         WHERE account_id = :account_id AND prevout_script = :script",
        params,
    )?;
    conn.execute(
        "DELETE FROM tpir_coverage WHERE account_id = :account_id AND script = :script",
        params,
    )?;
    conn.execute(
        "DELETE FROM tpir_pending_page_scripts
         WHERE script = :script
         AND page_id IN (SELECT id FROM tpir_pending_pages WHERE account_id = :account_id)",
        params,
    )?;
    conn.execute(
        "DELETE FROM tpir_pending_pages
         WHERE account_id = :account_id
         AND NOT EXISTS (
             SELECT 1 FROM tpir_pending_page_scripts s WHERE s.page_id = tpir_pending_pages.id
         )",
        named_params![":account_id": from_account.0],
    )?;
    Ok(())
}
