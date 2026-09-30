//! Production ownership wins over unprojected candidate derivation.
use super::*;

/// Filter a derived watch without changing production address ownership or financial rows.
pub(super) fn retain_owned<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    account: AccountRef,
    addresses: &mut BTreeMap<TransparentAddress, WatchOrigin>,
) -> Result<(), SqliteClientError> {
    let mut stmt = conn.prepare_cached("SELECT cached_transparent_receiver_address FROM addresses WHERE account_id != ?1 AND cached_transparent_receiver_address IS NOT NULL")?;
    for encoded in stmt.query_map([account.0], |row| row.get::<_, String>(0))? {
        let encoded = encoded?;
        let address = Address::decode(params, &encoded)
            .and_then(|a| a.to_transparent_address())
            .ok_or_else(|| {
                SqliteClientError::CorruptedData("invalid transparent owner receiver".into())
            })?;
        addresses.remove(&address);
    }
    Ok(())
}

/// A standalone import is a production ownership change. Remove only competing candidate
/// facts in the same transaction; active receivers already have production address rows.
#[cfg(feature = "transparent-key-import")]
pub(crate) fn forget_other_candidates(
    conn: &rusqlite::Connection,
    owner: AccountRef,
    address: &TransparentAddress,
) -> Result<(), SqliteClientError> {
    let installed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tpir_active_accounts')", [], |row| row.get(0))?;
    if !installed {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached("SELECT id FROM accounts WHERE id != ?1 AND NOT EXISTS (SELECT 1 FROM tpir_active_accounts a WHERE a.account_id = accounts.id)")?;
    let accounts = stmt
        .query_map([owner.0], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for account in accounts {
        lifecycle::forget_reattributed_script(conn, AccountRef(account), address)?;
    }
    Ok(())
}
