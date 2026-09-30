//! Production ownership wins over unprojected candidate derivation.
use super::*;
use crate::wallet::transparent::store_address_range;
use std::ops::Range;
use zcash_keys::keys::transparent::gap_limits::generate_address_list;

/// Whether an account other than `account` has a production address row for `address`.
fn owned_elsewhere<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    account: AccountRef,
    address: &TransparentAddress,
) -> Result<bool, SqliteClientError> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM addresses WHERE cached_transparent_receiver_address = ?1 AND account_id != ?2)",
    )?;
    Ok(stmt.query_row(
        rusqlite::params![Address::Transparent(*address).encode(params), account.0],
        |row| row.get::<_, bool>(0),
    )?)
}

/// Filter a derived watch without changing production address ownership or financial rows.
pub(super) fn retain_owned<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    account: AccountRef,
    addresses: &mut BTreeMap<TransparentAddress, WatchOrigin>,
) -> Result<(), SqliteClientError> {
    let mut excluded = vec![];
    for address in addresses.keys() {
        if owned_elsewhere(conn, params, account, address)? {
            excluded.push(*address);
        }
    }
    for address in excluded {
        addresses.remove(&address);
    }
    Ok(())
}

/// Writes `account`'s derived addresses in `range` to the address table, except receivers another
/// account owns. The watch set excluded those receivers, so the account has no coverage for them,
/// and transferring one here would make promotion refuse itself on every retry. The wallet's own
/// address generation, including gap-limit generation after a receive is projected, may still
/// transfer one under the existing rules; recovery then schedules it for its new owner.
pub(super) fn generate_unowned_range<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    account: &Account,
    key_scope: TransparentKeyScope,
    range: Range<NonHardenedChildIndex>,
) -> Result<(), SqliteClientError> {
    let account_ref = account.internal_id();
    let mut unowned = vec![];
    for entry in generate_address_list(
        &account.uivk(),
        account.ufvk(),
        key_scope,
        UnifiedAddressRequest::unsafe_custom(Allow, Allow, Require),
        range,
        false,
    )? {
        if !owned_elsewhere(conn, params, account_ref, &entry.1)? {
            unowned.push(entry);
        }
    }
    store_address_range(conn, params, account_ref, key_scope, unowned)
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
