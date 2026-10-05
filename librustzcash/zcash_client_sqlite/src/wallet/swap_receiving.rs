//! Experimental durable swap receiving-key registration.
//!
//! Reserve before exposing an address, and persist the returned key ID with the
//! operation. Retries reuse that ID rather than reserving again. Keys issued by
//! this wallet are trial-decrypted from issuance until they close. Keys recovered
//! from the seed are swept once through the receiver directory, then scanned only
//! while their swap is still open. Ownership remains available after a key closes.

mod apply;
pub use apply::PaymentApplication;
mod lifecycle;
mod payments;
mod planner;
pub use planner::{DiscoveryBatch, DiscoveryWork};
mod recovery;
mod reservations;
mod retention;
pub use payments::{PendingPayment, SpendStatus};
pub use recovery::RecoveredRefund;
pub use reservations::{
    RECEIVE_GAP_LIMIT, RECEIVE_RECLAIM_SECONDS, RECEIVE_UNFUNDED_LIMIT, ReceiveQuote,
    ReceiveReservation,
};

use std::{
    borrow::{Borrow, BorrowMut},
    collections::HashSet,
    ops::Range,
    time::UNIX_EPOCH,
};

use orchard::keys::{FullViewingKey, Scope};
use rusqlite::{Connection, OptionalExtension, named_params};
use zcash_client_backend::{
    data_api::{
        Account as _,
        scanning::{ScanPriority, ScanRange},
    },
    scanning::swap_receiving::SwapScanningKey,
};
use zcash_protocol::consensus::{BlockHeight, Parameters};

use zakura_swap_receiving::DerivationError;
pub use zakura_swap_receiving::{KeyId, Purpose};

use crate::{AccountUuid, SqlTransaction, WalletDb, error::SqliteClientError, util::Clock};

/// Stable allocation outcomes for wallet UI and retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationPolicy {
    /// Recovery has not established the next safe index.
    Gap,
    /// Too many unfunded incoming operations are reserved.
    Limit,
    /// The selected draft is no longer available.
    Stale,
    /// The wallet has not scanned this address through the chain tip.
    Coverage,
}
impl std::fmt::Display for ReservationPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Gap=>"Incoming address recovery is pending. Wait for a payment or abandoned-address reconciliation.",
            Self::Limit=>"Three incoming swaps are awaiting deposits. Resume an existing swap or wait for reconciliation.",
            Self::Stale=>"This receive reservation is no longer available. Request a new quote.",
            Self::Coverage=>"Finish syncing to the chain tip before requesting a quote.",
        })
    }
}

/// An error reserving or reconstructing a receiving key.
#[derive(Debug)]
pub enum Error {
    /// Database, account, or stored-data validation failed.
    Wallet(SqliteClientError),
    /// No valid key was found in the KDF retry space.
    Derivation(DerivationError),
    /// This purpose's index space is exhausted. Never wrap back to zero.
    IndexExhausted,
    /// Address allocation is waiting for recovery or an existing reservation.
    ReservationPolicy(ReservationPolicy),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wallet(e) => e.fmt(f),
            Self::Derivation(e) => e.fmt(f),
            Self::IndexExhausted => f.write_str("swap receiving index space exhausted"),
            Self::ReservationPolicy(policy) => policy.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wallet(e) => Some(e),
            Self::Derivation(e) => Some(e),
            Self::IndexExhausted | Self::ReservationPolicy(_) => None,
        }
    }
}

impl From<SqliteClientError> for Error {
    fn from(e: SqliteClientError) -> Self {
        Self::Wallet(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Wallet(e.into())
    }
}

impl From<DerivationError> for Error {
    fn from(e: DerivationError) -> Self {
        Self::Derivation(e)
    }
}

/// A registered key reconstructed from the wallet's account viewing key.
///
/// This contains viewing material. It deliberately does not implement `Debug`.
pub struct RegisteredKey {
    scanning_key: SwapScanningKey<AccountUuid>,
    scan_from: BlockHeight,
    advances_allocation: bool,
}

impl RegisteredKey {
    /// The owning wallet account.
    pub fn account(&self) -> AccountUuid {
        *self.scanning_key.account_id()
    }
    /// The purpose/index identity to retain with operations and received notes.
    pub fn key_id(&self) -> KeyId {
        self.scanning_key.key_id()
    }
    /// The derived FVK to use for note reconstruction and spending.
    pub fn full_viewing_key(&self) -> &FullViewingKey {
        self.scanning_key.full_viewing_key()
    }
    pub(crate) fn into_scanning_key(self) -> SwapScanningKey<AccountUuid> {
        self.scanning_key
    }

    /// The receiver at external diversifier index zero.
    pub fn receiver(&self) -> orchard::Address {
        self.full_viewing_key().address_at(0u32, Scope::External)
    }
    /// Earliest requested scan height, inclusive. This is not scanned coverage.
    pub fn scan_from(&self) -> BlockHeight {
        self.scan_from
    }
    /// Whether reservation or recovery evidence puts subsequent allocations above this key.
    pub fn advances_allocation(&self) -> bool {
        self.advances_allocation
    }
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Reconstructs one registered key without deriving unrelated keys.
    ///
    /// Returns `None` if the account has no registration for this identity.
    /// The stored receiver must match the derived key, even for retired keys.
    pub fn get_swap_receiving_key(
        &self,
        account: AccountUuid,
        key_id: KeyId,
    ) -> Result<Option<RegisteredKey>, Error> {
        self.swap_receiving_key_matching(
            account,
            "k.purpose=?2 AND k.derivation_version=1 AND k.key_index=?3",
            rusqlite::params![
                account.0,
                purpose_code(key_id.purpose()),
                key_id.index().to_be_bytes()
            ],
        )
    }

    /// Finds one registered receiver under its owning account and validates its key.
    ///
    /// Returns `None` for ordinary addresses or receivers owned by another account.
    pub fn get_swap_receiving_key_for_receiver(
        &self,
        account: AccountUuid,
        receiver: &orchard::Address,
    ) -> Result<Option<RegisteredKey>, Error> {
        self.swap_receiving_key_matching(
            account,
            "k.receiver=?2",
            rusqlite::params![account.0, receiver.to_raw_address_bytes()],
        )
    }

    // Keep selection and decoding in one query. Row IDs may be reused if another
    // connection deletes a registration. Predicates are library-owned SQL only.
    fn swap_receiving_key_matching(
        &self,
        account: AccountUuid,
        predicate: &'static str,
        bindings: impl rusqlite::Params,
    ) -> Result<Option<RegisteredKey>, Error> {
        let conn = self.conn.borrow();
        let (_, parent) = account_key(conn, &self.params, account)?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT k.purpose, k.derivation_version, k.key_index, k.receiver,
                    k.scan_from, k.advances_allocation
             FROM ironwood_receiving_keys k JOIN accounts a ON a.id=k.account_id
             WHERE a.uuid=?1 AND {predicate}",
        ))?;
        let mut rows = stmt.query(bindings)?;
        rows.next()?
            .map(|row| registered_key(row, account, &parent))
            .transpose()
    }

    /// Reloads registered keys, including unpaid lookahead keys, for an account.
    ///
    /// Re-derivation must reproduce the stored receiver. Corrupt or unsupported
    /// registrations return an error, not a silently shortened recovery list.
    pub fn get_swap_receiving_keys(
        &self,
        account: AccountUuid,
    ) -> Result<Vec<RegisteredKey>, Error> {
        self.swap_receiving_keys_where(account, "TRUE")
    }

    /// Keys to trial-decrypt in every scan batch: active and not yet closed.
    pub(crate) fn swap_scanning_keys(
        &self,
        account: AccountUuid,
    ) -> Result<Vec<RegisteredKey>, Error> {
        self.swap_receiving_keys_where(account, "active_from IS NOT NULL AND closed_at IS NULL")
    }

    // `predicate` is library-owned SQL over `ironwood_receiving_keys`.
    fn swap_receiving_keys_where(
        &self,
        account: AccountUuid,
        predicate: &'static str,
    ) -> Result<Vec<RegisteredKey>, Error> {
        let conn = self.conn.borrow();
        let (account_ref, parent) = account_key(conn, &self.params, account)?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT purpose, derivation_version, key_index, receiver, scan_from, advances_allocation
             FROM ironwood_receiving_keys WHERE account_id = ?1 AND {predicate}
             ORDER BY purpose, key_index",
        ))?;
        let mut rows = stmt.query([account_ref.0])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            keys.push(registered_key(row, account, &parent)?);
        }
        Ok(keys)
    }

    pub(crate) fn swap_receiving_transaction_keys(
        &self,
        account: AccountUuid,
        txid: zcash_primitives::transaction::TxId,
        height: Option<BlockHeight>,
        receivers: &[orchard::Address],
    ) -> Result<Vec<RegisteredKey>, Error> {
        let conn = self.conn.borrow();
        let (account_ref, parent) = account_key(conn, &self.params, account)?;
        let height = super::chain_tip_height(conn)?
            .map(|h| BlockHeight::from(u32::from(h).saturating_add(1)))
            .or(height);
        let mut selected = std::collections::HashSet::new();
        let mut by_receiver = conn.prepare_cached(
            "SELECT id FROM ironwood_receiving_keys WHERE account_id=?1 AND receiver=?2",
        )?;
        for receiver in receivers {
            for id in by_receiver.query_map(
                rusqlite::params![account_ref.0, receiver.to_raw_address_bytes()],
                |r| r.get::<_, i64>(0),
            )? {
                selected.insert(id?);
            }
        }
        let extra = selected
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        // Each union branch starts from transaction or scanning indexes. Enhancing
        // one closed key's note does not walk or derive the historical registry.
        let sql = format!(
            "SELECT purpose,derivation_version,key_index,receiver,scan_from,advances_allocation
            FROM ironwood_receiving_keys WHERE account_id=?1 AND id IN (
                SELECT receiving_key_id FROM ironwood_received_notes
                    WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?2)
                UNION SELECT receiving_key_id FROM ironwood_swap_payment_recovery WHERE txid=?2
                UNION SELECT id FROM ironwood_receiving_keys WHERE account_id=?1
                    AND closed_at IS NULL AND active_from<=COALESCE(?3,active_from)
                UNION SELECT id FROM ironwood_receiving_keys WHERE id IN ({extra}))"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![
            account_ref.0,
            txid.as_ref(),
            height.map(u32::from)
        ])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            keys.push(registered_key(row, account, &parent)?);
        }
        Ok(keys)
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL: Clock, R> WalletDb<C, P, CL, R> {
    /// Atomically reserves and registers the next index for this purpose.
    ///
    /// The key is trial-decrypted from `scan_from`, inclusive, until it closes.
    /// For a new address, use the next height after the accepted tip; blocks at
    /// or above it that were already scanned are queued for rescanning.
    /// Only a committed result may be exposed. On a concurrent-write error,
    /// retry the whole operation. To also persist application operation state,
    /// call this on the wallet handle inside `transactionally_with_extension`.
    pub fn reserve_swap_receiving_key(
        &mut self,
        account: AccountUuid,
        purpose: Purpose,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        self.transactionally(|wdb| wdb.reserve_swap_receiving_key(account, purpose, scan_from))
    }

    /// Registers a key backed by authenticated recovery evidence.
    ///
    /// For refunds the caller must validate its own funding memo. For incoming
    /// keys it must validate a payment, including one already spent. An empty
    /// directory result is not evidence. Repeated registration is idempotent.
    /// Set `scan_from` no later than the first possible payment. A key this
    /// wallet never scanned is queued for a receiver-directory sweep.
    pub fn recover_swap_receiving_key(
        &mut self,
        account: AccountUuid,
        key_id: KeyId,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        self.transactionally(|wdb| wdb.recover_swap_receiving_key(account, key_id, scan_from))
    }

    /// Registers an incoming lookahead key for a receiver-directory sweep without
    /// advancing address allocation.
    pub fn watch_swap_receive_key(
        &mut self,
        account: AccountUuid,
        index: u64,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        self.transactionally(|wdb| wdb.watch_swap_receive_key(account, index, scan_from))
    }
}

impl<P: Parameters, CL: Clock, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// Reserves in the enclosing transaction. Expose the address only after commit.
    pub fn reserve_swap_receiving_key(
        &mut self,
        account: AccountUuid,
        purpose: Purpose,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        let (account_ref, _) = account_key(self.conn.0, &self.params, account)?;
        // SQLite orders fixed-width big-endian blobs numerically. Unlike INTEGER,
        // this represents the entire u64 index space, including u64::MAX.
        let last: Option<Vec<u8>> = self.conn.0.query_row(
            "SELECT MAX(key_index) FROM ironwood_receiving_keys
             WHERE account_id = :account AND purpose = :purpose
               AND derivation_version = 1 AND advances_allocation = 1",
            named_params![":account": account_ref.0, ":purpose": purpose_code(purpose)],
            |row| row.get(0),
        )?;
        let index = match last {
            Some(bytes) => decode_index(bytes)?
                .checked_add(1)
                .ok_or(Error::IndexExhausted)?,
            None => 0,
        };
        register(
            self.conn.0,
            &self.params,
            account,
            KeyId::new(purpose, index),
            scan_from,
            true,
            Discovery::Scan(scan_from),
            unix_now(&self.clock),
        )
    }

    /// Records authenticated recovery evidence in the enclosing transaction.
    /// See [`WalletDb::recover_swap_receiving_key`] on a connection-backed handle.
    pub fn recover_swap_receiving_key(
        &mut self,
        account: AccountUuid,
        key_id: KeyId,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        register(
            self.conn.0,
            &self.params,
            account,
            key_id,
            scan_from,
            true,
            Discovery::Sweep,
            unix_now(&self.clock),
        )
    }

    /// Watches an unpaid incoming index in the enclosing transaction.
    pub fn watch_swap_receive_key(
        &mut self,
        account: AccountUuid,
        index: u64,
        scan_from: BlockHeight,
    ) -> Result<RegisteredKey, Error> {
        register(
            self.conn.0,
            &self.params,
            account,
            KeyId::new(Purpose::Receive, index),
            scan_from,
            false,
            Discovery::Sweep,
            unix_now(&self.clock),
        )
    }
}

// Shared registry projection adds receiver, scan_from and advances_allocation.
fn registered_key(
    row: &rusqlite::Row<'_>,
    account: AccountUuid,
    parent: &FullViewingKey,
) -> Result<RegisteredKey, Error> {
    let key = RegisteredKey {
        scanning_key: SwapScanningKey::derive(account, stored_key_id(row)?, parent)?,
        scan_from: BlockHeight::from(row.get::<_, u32>(4)?),
        advances_allocation: row.get(5)?,
    };
    let receiver: Vec<u8> = row.get(3)?;
    if receiver != key.receiver().to_raw_address_bytes() {
        return Err(corrupt(
            "stored swap receiver does not match its derived key",
        ));
    }
    Ok(key)
}

// Shared column order: purpose, derivation_version, key_index.
fn stored_key_id(row: &rusqlite::Row<'_>) -> Result<KeyId, Error> {
    if row.get::<_, u8>(1)? != 1 {
        return Err(corrupt("unsupported swap key derivation version"));
    }
    let purpose = match row.get::<_, u8>(0)? {
        0 => Purpose::Refund,
        1 => Purpose::Receive,
        _ => return Err(corrupt("unsupported swap key purpose")),
    };
    Ok(KeyId::new(purpose, decode_index(row.get(2)?)?))
}

fn account_key<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
) -> Result<(super::AccountRef, FullViewingKey), Error> {
    let account =
        super::get_account(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
    let fvk = account
        .ufvk()
        .and_then(|key| key.orchard())
        .ok_or_else(|| {
            SqliteClientError::BadAccountData(
                "swap receiving requires an Orchard full viewing key".into(),
            )
        })?;
    Ok((account.id, fvk.clone()))
}

/// How a newly registered key's history is covered.
#[derive(Clone, Copy)]
pub(super) enum Discovery {
    /// Trial-decrypt from this height until the key closes.
    Scan(BlockHeight),
    /// Sweep the receiver directory once, unless the key is already scanned.
    Sweep,
}

#[allow(clippy::too_many_arguments)]
fn register<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    account: AccountUuid,
    key_id: KeyId,
    scan_from: BlockHeight,
    advances_allocation: bool,
    discovery: Discovery,
    now: i64,
) -> Result<RegisteredKey, Error> {
    let (account_ref, parent) = account_key(conn, params, account)?;
    let scanning_key = SwapScanningKey::derive(account, key_id, &parent)?;
    let receiver = scanning_key
        .full_viewing_key()
        .address_at(0u32, Scope::External)
        .to_raw_address_bytes();
    // Repeated recovery can widen required history or promote a lookahead key.
    // It must never forget a reservation, narrow history, or replace a receiver.
    let updated: Option<(i64, u32, bool, Option<u32>)> = conn.query_row(
        "INSERT INTO ironwood_receiving_keys
             (account_id, purpose, derivation_version, key_index, receiver, scan_from,
              advances_allocation, registered_at)
         VALUES (:account, :purpose, 1, :index, :receiver, :scan_from, :allocated, :now)
         ON CONFLICT (account_id, purpose, derivation_version, key_index) DO UPDATE SET
             scan_from = MIN(ironwood_receiving_keys.scan_from, excluded.scan_from),
             advances_allocation = MAX(ironwood_receiving_keys.advances_allocation, excluded.advances_allocation)
         WHERE ironwood_receiving_keys.receiver = excluded.receiver
         RETURNING id, scan_from, advances_allocation, active_from",
        named_params![
            ":account": account_ref.0,
            ":purpose": purpose_code(key_id.purpose()),
            ":index": &key_id.index().to_be_bytes(),
            ":receiver": &receiver,
            ":scan_from": u32::from(scan_from),
            ":allocated": advances_allocation,
            ":now": now,
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional()?;
    let (id, scan_from, advances_allocation, active_from) =
        updated.ok_or_else(|| corrupt("stored swap receiver does not match its derived key"))?;
    match discovery {
        Discovery::Scan(from) => activate(conn, id, from)?,
        // Only a key scanned from its earliest possible payment needs no directory history.
        Discovery::Sweep if active_from.is_none_or(|start| start > scan_from) => {
            conn.execute(
                "INSERT OR IGNORE INTO ironwood_swap_sweeps (receiving_key_id) VALUES (?1)",
                [id],
            )?;
        }
        Discovery::Sweep => {}
    }
    Ok(RegisteredKey {
        scanning_key,
        scan_from: scan_from.into(),
        advances_allocation,
    })
}

/// Starts or extends trial decryption of key `id` from `from`, reopening a closed key.
///
/// Blocks at or above `from` that were scanned without this key are queued for
/// rescanning, so no block in the key's active range is left unchecked. A key
/// first found by a restore sweep starts no later than the block after the
/// sweep's coverage, and cannot start while that sweep is pending.
pub(super) fn activate(
    conn: &rusqlite::Transaction<'_>,
    id: i64,
    from: BlockHeight,
) -> Result<(), Error> {
    let (active_from, closed, swept, sweep_done): (Option<u32>, bool, bool, Option<u32>) = conn
        .query_row(
            "SELECT k.active_from, k.closed_at IS NOT NULL, s.receiving_key_id IS NOT NULL, s.done_height
             FROM ironwood_receiving_keys k
             LEFT JOIN ironwood_swap_sweeps s ON s.receiving_key_id = k.id
             WHERE k.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let from = match (active_from, swept, sweep_done) {
        (None, true, Some(done)) => from.min(BlockHeight::from(done.saturating_add(1))),
        (None, true, None) => return Err(Error::ReservationPolicy(ReservationPolicy::Gap)),
        _ => from,
    };
    let scanned_end = conn
        .query_row("SELECT MAX(height) FROM blocks", [], |row| {
            row.get::<_, Option<u32>>(0)
        })?
        .map_or(u32::from(from), |h| h.saturating_add(1));
    let rescan_end = match active_from {
        Some(start) if !closed && start <= u32::from(from) => return Ok(()),
        // Lowering an open key's start only adds the older blocks.
        Some(start) if !closed => start.min(scanned_end),
        _ => scanned_end,
    };
    conn.execute(
        "UPDATE ironwood_receiving_keys SET active_from = ?2, closed_at = NULL WHERE id = ?1",
        rusqlite::params![id, u32::from(from)],
    )?;
    queue_rescan(conn, from..BlockHeight::from(rescan_end))
}

/// Queues `range` for a forced rescan with historic priority.
fn queue_rescan(conn: &rusqlite::Transaction<'_>, range: Range<BlockHeight>) -> Result<(), Error> {
    if !range.is_empty() {
        crate::wallet::scanning::replace_queue_entries::<SqliteClientError>(
            conn,
            &range,
            std::iter::once(ScanRange::from_parts(range.clone(), ScanPriority::Historic)),
            true,
        )?;
    }
    Ok(())
}

/// Requeues the part of a stored batch that active keys missed.
///
/// `used` is the key snapshot the batch was scanned with. A key activated while
/// the batch was in flight was not in that snapshot, so its share is rescanned.
pub(crate) fn rescan_missed_keys(
    conn: &rusqlite::Transaction<'_>,
    used: &[(AccountUuid, KeyId)],
    range: Range<BlockHeight>,
) -> Result<(), SqliteClientError> {
    let used: HashSet<_> = used.iter().copied().collect();
    let mut stmt = conn.prepare_cached(
        "SELECT k.purpose, k.derivation_version, k.key_index, a.uuid, k.active_from
         FROM ironwood_receiving_keys k JOIN accounts a ON a.id = k.account_id
         WHERE k.closed_at IS NULL AND k.active_from < ?1",
    )?;
    let mut rows = stmt.query([u32::from(range.end)])?;
    let mut missed = Vec::new();
    while let Some(row) = rows.next()? {
        let key = stored_key_id(row).map_err(wallet_error)?;
        if !used.contains(&(AccountUuid(row.get(3)?), key)) {
            missed.push(BlockHeight::from(row.get::<_, u32>(4)?).max(range.start));
        }
    }
    drop(rows);
    if let Some(start) = missed.into_iter().min() {
        queue_rescan(conn, start..range.end).map_err(wallet_error)?;
    }
    Ok(())
}

/// Seconds since the Unix epoch on the wallet clock, or 0 before it.
fn unix_now(clock: &impl Clock) -> i64 {
    clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Reconstruct a note's key and check that its registry account is the selected account.
pub(super) fn note_key<P: Parameters>(
    conn: &Connection,
    params: &P,
    id: i64,
    parent: &FullViewingKey,
) -> Result<(KeyId, FullViewingKey), SqliteClientError> {
    let (account, purpose, version, index, receiver): (AccountUuid, u8, u8, Vec<u8>, Vec<u8>) =
        conn.query_row(
            "SELECT a.uuid, k.purpose, k.derivation_version, k.key_index, k.receiver
         FROM ironwood_receiving_keys k JOIN accounts a ON a.id = k.account_id
         WHERE k.id = ?1",
            [id],
            |row| {
                Ok((
                    AccountUuid(row.get(0)?),
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
    let (_, registered_parent) = account_key(conn, params, account).map_err(wallet_error)?;
    if registered_parent.to_bytes() != parent.to_bytes() || version != 1 {
        return Err(SqliteClientError::CorruptedData(
            "swap note key belongs to another account or version".into(),
        ));
    }
    let purpose = match purpose {
        0 => Purpose::Refund,
        1 => Purpose::Receive,
        _ => {
            return Err(SqliteClientError::CorruptedData(
                "invalid swap key purpose".into(),
            ));
        }
    };
    let key_id = KeyId::new(purpose, decode_index(index).map_err(wallet_error)?);
    let fvk = key_id.derive(parent).map_err(|e| wallet_error(e.into()))?;
    if receiver != fvk.address_at(0u32, Scope::External).to_raw_address_bytes() {
        return Err(SqliteClientError::CorruptedData(
            "stored swap receiver does not match its derived key".into(),
        ));
    }
    Ok((key_id, fvk))
}

/// Validate scan metadata before it can change a note or advance sequence allocation.
pub(super) fn validate_received_key<
    P: Parameters,
    T: zcash_client_backend::data_api::ll::ReceivedOrchardOutput<AccountId = AccountUuid>,
>(
    conn: &Connection,
    params: &P,
    pool: zcash_protocol::ShieldedPool,
    output: &T,
) -> Result<Option<i64>, SqliteClientError> {
    let Some(key_id) = output.swap_key_id() else {
        return Ok(None);
    };
    let (account, parent) = account_key(conn, params, output.account_id()).map_err(wallet_error)?;
    let id: i64 = conn.query_row(
        "SELECT id FROM ironwood_receiving_keys WHERE account_id = ?1
         AND purpose = ?2 AND derivation_version = 1 AND key_index = ?3",
        rusqlite::params![
            account.0,
            purpose_code(key_id.purpose()),
            key_id.index().to_be_bytes()
        ],
        |row| row.get(0),
    )?;
    let (_, fvk) = note_key(conn, params, id, &parent)?;
    if pool != zcash_protocol::ShieldedPool::Ironwood
        || output.note().version() != orchard::note::NoteVersion::V3
        || output.recipient_key_scope() != Some(Scope::External)
        || output.note().recipient() != fvk.address_at(0u32, Scope::External)
        || output
            .nullifier()
            .is_some_and(|nf| *nf != output.note().nullifier(&fvk))
    {
        return Err(SqliteClientError::CorruptedData(
            "swap note does not match its registered key".into(),
        ));
    }
    Ok(Some(id))
}

fn wallet_error(error: Error) -> SqliteClientError {
    match error {
        Error::Wallet(error) => error,
        other => SqliteClientError::CorruptedData(other.to_string()),
    }
}

fn purpose_code(purpose: Purpose) -> u8 {
    match purpose {
        Purpose::Refund => 0,
        Purpose::Receive => 1,
    }
}

fn decode_index(bytes: Vec<u8>) -> Result<u64, Error> {
    bytes
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| corrupt("invalid swap key index"))
}

fn corrupt(message: &str) -> Error {
    SqliteClientError::CorruptedData(message.to_owned()).into()
}

#[cfg(test)]
mod tests;
