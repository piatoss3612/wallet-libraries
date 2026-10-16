//! Early restore: finds a restored account's one-time payments while the restore scan
//! runs, rather than through sweeps after it (see `OneTimeIvkRestoreWrite`).
//!
//! Before the scan reaches the account's history, the caller reads a receiver-directory
//! publication's public filters and registers the account's keys from them ([`begin`]),
//! then looks up only the paid ones ([`record_lookup`]). Each returned payment is queued
//! with its witness. One whose block is not scanned yet is a scan hint: the scanner tries
//! its key on the batch holding that block (see `one_time_ivk::scanning_keys_for_range`),
//! and [`after_blocks_stored`] checks that the batch found that exact note. One whose
//! block is already scanned is inserted with `apply::apply_payment` once every block from
//! it on carries spend evidence. Anything that does not settle this way falls back to a
//! restore sweep.
//!
//! The publication stays provisional until the scan stores its last block, which must
//! match its hash and tree size; a mismatch discards what it registered.
use std::ops::Range;

use rusqlite::{Connection, OptionalExtension, params};
use zcash_client_backend::data_api::{
    one_time_ivk::restore::{
        DirectoryPayment, DiscoveryWork, EarlyRestoreKey, PaymentApplication, PublicationEnd,
        recovery::EncryptedNote,
    },
    scanning::ScanPriority,
    transparent_ledger::{ChainPoint, TransparentLedgerMode},
};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

use super::{
    PendingPayment,
    apply::{Gate, apply_payment, merkle_path},
    payments::{authenticate, pending_payments, queue_payment},
    recovery::watch_receive_key,
};
use crate::wallet::one_time_ivk::{
    Discovery, KeyId, Purpose, account_key, corrupt, index_after, key_ref, register, restore_start,
    swap::RECEIVE_GAP_LIMIT, swap::reservations::record_provider_seen,
};
use crate::{
    AccountUuid,
    error::SqliteClientError,
    wallet::{
        enhance_pir::session::SessionState, scanning::priority_code,
        transparent_ledger::session::SessionState as TpirSessionState,
    },
};

/// A publication an account's early restore registered keys from.
struct Restore {
    account: i64,
    restore_from: u32,
    anchor: ChainPoint,
    tree_size: u64,
    state: u8,
}

/// `ironwood_one_time_restores.state` before the scan stores the anchor block.
const PROVISIONAL: u8 = 0;
/// `ironwood_one_time_restores.state` once the anchor block matched.
const ACCEPTED: u8 = 1;
/// `ironwood_one_time_restores.state` once the anchor block did not match.
const REJECTED: u8 = 2;

/// Implements `OneTimeIvkRestoreRead::one_time_early_restore_start`.
pub(crate) fn start<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
) -> Result<Option<BlockHeight>, SqliteClientError> {
    if params.activation_height(NetworkUpgrade::Nu6_3).is_none() {
        return Ok(None);
    }
    let (owner, _) = account_key(conn, params, account)?;
    let from = restore_start(conn, params, owner)?;
    // `add_account` requeues every block from a new account's birthday, so "scanned"
    // comes from the scan queue: block rows above the rewind may outlive it.
    let blocked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_fresh_accounts WHERE account_id = ?1)
         OR EXISTS(SELECT 1 FROM ironwood_one_time_restores WHERE account_id = ?1)
         OR EXISTS(SELECT 1 FROM ironwood_one_time_sweeps s
             JOIN ironwood_one_time_keys k ON k.id = s.one_time_key_id
             WHERE k.account_id = ?1 AND s.done_height IS NULL)
         OR EXISTS(SELECT 1 FROM scan_queue WHERE priority = ?2 AND block_range_end > ?3)",
        params![
            owner.0,
            priority_code(&ScanPriority::Scanned),
            u32::from(from)
        ],
        |r| r.get(0),
    )?;
    Ok((!blocked).then_some(from))
}

/// Implements `OneTimeIvkRestoreRead::one_time_receivers`.
pub(crate) fn receivers<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
    purpose: Purpose,
    from: u64,
    count: u32,
) -> Result<Vec<DiscoveryWork>, SqliteClientError> {
    let (_, parent) = account_key(conn, params, account)?;
    (0..u64::from(count))
        .map(|offset| {
            let key = KeyId::new(purpose, index_after(from, offset)?);
            let receiver = key
                .derive(&parent)?
                .address_at(0u32, orchard::keys::Scope::External)
                .to_raw_address_bytes();
            Ok(DiscoveryWork { key, receiver })
        })
        .collect()
}

/// Implements `OneTimeIvkRestoreWrite::begin_one_time_early_restore`.
pub(crate) fn begin<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    account: AccountUuid,
    end: PublicationEnd,
    keys: &[EarlyRestoreKey],
    now: i64,
) -> Result<bool, SqliteClientError> {
    let Some(from) = start(conn, params, account)? else {
        return Ok(false);
    };
    // The scan never reaches an anchor below the restore start, so nothing would check it.
    if end.height < from {
        return Ok(false);
    }
    let (owner, _) = account_key(conn, params, account)?;
    conn.execute(
        "INSERT INTO ironwood_one_time_restores
            (account_id, restore_from, anchor_height, anchor_hash, anchor_tree_size)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            owner.0,
            u32::from(from),
            u32::from(end.height),
            end.hash,
            end.tree_size
        ],
    )?;
    for key in keys {
        let used = key.seen || key.paid;
        let (id, _) = register(
            conn,
            params,
            account,
            key.key,
            from,
            used,
            Discovery::Funding,
            now,
        )?;
        if used {
            record_provider_seen(conn, id)?;
        }
        // A paid key's restore waits for its lookup; any other is done at the anchor.
        let done = (!key.paid).then_some(u32::from(end.height));
        conn.execute(
            "INSERT INTO ironwood_one_time_sweeps (one_time_key_id, done_height, provisional)
             VALUES (?1, ?2, 1)
             ON CONFLICT (one_time_key_id) DO UPDATE SET done_height = excluded.done_height,
                 lookup_height = NULL, lookup_hash = NULL, attempts = 0, next_attempt_at = 0,
                 provisional = 1",
            params![id, done],
        )?;
        // Nothing from the restore start on is scanned, so starting needs no rescan.
        if key.recent {
            conn.execute(
                "UPDATE ironwood_one_time_keys SET active_from = ?2, closed_at = NULL
                 WHERE id = ?1 AND active_from IS NULL",
                params![id, u32::from(end.height + 1)],
            )?;
        }
    }
    Ok(true)
}

/// Implements `OneTimeIvkRestoreWrite::record_one_time_early_lookup`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_lookup<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    configured: Option<TransparentLedgerMode>,
    session: &SessionState,
    tpir_session: &TpirSessionState,
    account: AccountUuid,
    key: KeyId,
    payments: &[DirectoryPayment],
    mut witness: impl FnMut(u32, [u8; 32]) -> Option<[[u8; 32]; 32]>,
) -> Result<(), SqliteClientError> {
    let id = key_ref(conn, account, key)?;
    let (owner, _) = account_key(conn, params, account)?;
    let Some(restore) = restore(conn, owner.0)? else {
        return Ok(());
    };
    // Only a paid key still waiting for this lookup takes it, once.
    let waiting: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_sweeps
             WHERE one_time_key_id = ?1 AND provisional = 1 AND done_height IS NULL)
         AND NOT EXISTS(SELECT 1 FROM ironwood_one_time_payment_recovery
             WHERE one_time_key_id = ?1)",
        [id],
        |r| r.get(0),
    )?;
    if restore.state == REJECTED || !waiting {
        return Ok(());
    }
    let mut candidates = Vec::with_capacity(payments.len());
    for payment in payments {
        let Some(candidate) = pending_payment(payment)? else {
            return fall_back(conn, id);
        };
        if u32::from(candidate.height) < restore.restore_from
            || candidate.height > restore.anchor.height
        {
            return fall_back(conn, id);
        }
        // A payment the key cannot decrypt is the directory's error, not the wallet's.
        match authenticate(conn, params, account, key, &candidate) {
            Ok(_) => candidates.push(candidate),
            Err(SqliteClientError::InvalidOneTimeIvkInput(_)) => return fall_back(conn, id),
            Err(e) => return Err(e),
        }
    }
    for candidate in &candidates {
        let siblings = witness(candidate.position, candidate.encrypted_note.commitment());
        queue_payment(conn, params, account, key, candidate)?;
        conn.execute(
            "UPDATE ironwood_one_time_payment_recovery SET witness = ?3, scan_hint = ?4
             WHERE txid = ?1 AND action_index = ?2",
            params![
                candidate.txid.as_ref(),
                candidate.action_index,
                siblings.map(|s| s.concat()),
                !scanned(conn, candidate.height)?
            ],
        )?;
    }
    if restore.state == ACCEPTED {
        insert_ready(conn, params, configured, session, tpir_session, &restore)?;
    }
    settle(conn, id, &restore)
}

/// Runs after the scan stores `range` with the one-time keys `keys`: accepts or rejects
/// a publication whose anchor block it stored, checks the hints it covered, and inserts
/// queued payments whose spend evidence is now complete.
pub(crate) fn after_blocks_stored<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    configured: Option<TransparentLedgerMode>,
    session: &SessionState,
    tpir_session: &TpirSessionState,
    keys: &[(AccountUuid, KeyId)],
    range: Range<BlockHeight>,
) -> Result<(), SqliteClientError> {
    for restore in restores(conn, PROVISIONAL)? {
        if range.contains(&restore.anchor.height) {
            judge(conn, params, &restore)?;
        }
    }
    check_hints(conn, keys, range)?;
    for restore in restores(conn, ACCEPTED)? {
        insert_ready(conn, params, configured, session, tpir_session, &restore)?;
    }
    Ok(())
}

/// Accepts `restore`'s publication if its anchor block matches the wallet's, closing the
/// keys that neither scan nor wait for payments, or rejects it.
fn judge<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    restore: &Restore,
) -> Result<(), SqliteClientError> {
    let stored: Option<(BlockHash, Option<u64>)> = conn
        .query_row(
            "SELECT hash, ironwood_commitment_tree_size FROM blocks WHERE height = ?1",
            [u32::from(restore.anchor.height)],
            |r| Ok((BlockHash(r.get(0)?), r.get(1)?)),
        )
        .optional()?;
    if stored != Some((restore.anchor.hash, Some(restore.tree_size))) {
        return reject(conn, params, restore);
    }
    conn.execute(
        "UPDATE ironwood_one_time_restores SET state = ?2 WHERE account_id = ?1",
        params![restore.account, ACCEPTED],
    )?;
    let accepted = Restore {
        state: ACCEPTED,
        ..*restore
    };
    let done: Vec<i64> = conn
        .prepare(
            "SELECT s.one_time_key_id FROM ironwood_one_time_sweeps s
             JOIN ironwood_one_time_keys k ON k.id = s.one_time_key_id
             WHERE k.account_id = ?1 AND s.provisional = 1 AND s.done_height IS NOT NULL",
        )?
        .query_map([restore.account], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in done {
        close_done(conn, id, &accepted)?;
    }
    Ok(())
}

/// Discards what `restore`'s rejected publication registered and falls back to restore
/// sweeps: keys holding no payment, operation or reservation go, the rest are swept
/// again, and the incoming lookahead restarts from index zero. Refund keys come back from
/// their funding memos.
fn reject<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    restore: &Restore,
) -> Result<(), SqliteClientError> {
    conn.execute(
        "UPDATE ironwood_one_time_restores SET state = ?2 WHERE account_id = ?1",
        params![restore.account, REJECTED],
    )?;
    conn.execute(
        "DELETE FROM ironwood_one_time_keys WHERE account_id = ?1 AND reserved_at IS NULL
           AND id IN (SELECT one_time_key_id FROM ironwood_one_time_sweeps WHERE provisional = 1)
           AND NOT EXISTS(SELECT 1 FROM ironwood_received_notes n
               WHERE n.one_time_key_id = ironwood_one_time_keys.id)
           AND NOT EXISTS(SELECT 1 FROM ironwood_one_time_operations o
               WHERE o.one_time_key_id = ironwood_one_time_keys.id)",
        [restore.account],
    )?;
    // Every payment a provisional key holds came from this publication.
    conn.execute(
        "DELETE FROM ironwood_one_time_payment_recovery WHERE one_time_key_id IN (
             SELECT s.one_time_key_id FROM ironwood_one_time_sweeps s
             JOIN ironwood_one_time_keys k ON k.id = s.one_time_key_id
             WHERE k.account_id = ?1 AND s.provisional = 1)",
        [restore.account],
    )?;
    conn.execute(
        "UPDATE ironwood_one_time_sweeps SET done_height = NULL, lookup_height = NULL,
             lookup_hash = NULL, attempts = 0, next_attempt_at = 0, provisional = 0
         WHERE provisional = 1 AND one_time_key_id IN (
             SELECT id FROM ironwood_one_time_keys WHERE account_id = ?1)",
        [restore.account],
    )?;
    let account = account_uuid(conn, restore.account)?;
    let from = BlockHeight::from(restore.restore_from);
    for index in 0..RECEIVE_GAP_LIMIT {
        let registered: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_keys
             WHERE account_id = ?1 AND purpose = 1 AND key_index = ?2)",
            params![restore.account, index.to_be_bytes()],
            |r| r.get(0),
        )?;
        if !registered {
            watch_receive_key(conn, params, account, index, from, 0)?;
        }
    }
    Ok(())
}

/// Settles the hints in `range`, which the scan just stored with `keys`. A hint is
/// satisfied only by its own note. One whose key the batch did not try, because the
/// lookup came in after the scanner took its keys, becomes a payment to insert.
fn check_hints(
    conn: &rusqlite::Transaction<'_>,
    keys: &[(AccountUuid, KeyId)],
    range: Range<BlockHeight>,
) -> Result<(), SqliteClientError> {
    let hints: Vec<(i64, AccountUuid, KeyId, TxId, u32, u32)> = conn
        .prepare(
            "SELECT p.one_time_key_id, a.uuid, k.purpose, k.key_index, p.txid,
                 p.action_index, p.position
             FROM ironwood_one_time_payment_recovery p
             JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
             JOIN accounts a ON a.id = k.account_id
             WHERE p.scan_hint = 1 AND p.height >= ?1 AND p.height < ?2",
        )?
        .query_map(params![u32::from(range.start), u32::from(range.end)], |r| {
            Ok((
                r.get(0)?,
                AccountUuid(r.get(1)?),
                (r.get::<_, u8>(2)?, r.get::<_, Vec<u8>>(3)?),
                TxId::from_bytes(r.get(4)?),
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .map(|row| {
            let (id, account, (purpose, index), txid, action, position) = row?;
            Ok((
                id,
                account,
                decoded_key(purpose, index)?,
                txid,
                action,
                position,
            ))
        })
        .collect::<Result<_, SqliteClientError>>()?;
    for (id, account, key, txid, action, position) in hints {
        if !keys.contains(&(account, key)) {
            conn.execute(
                "UPDATE ironwood_one_time_payment_recovery SET scan_hint = 0
                 WHERE txid = ?1 AND action_index = ?2",
                params![txid.as_ref(), action],
            )?;
            continue;
        }
        let found: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes
             WHERE one_time_key_id = ?1 AND commitment_tree_position = ?2)",
            params![id, position],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM ironwood_one_time_payment_recovery WHERE txid = ?1 AND action_index = ?2",
            params![txid.as_ref(), action],
        )?;
        if found {
            let owner = account_ref_of(conn, id)?;
            if let Some(restore) = restore(conn, owner)? {
                settle(conn, id, &restore)?;
            }
        } else {
            fall_back(conn, id)?;
        }
    }
    Ok(())
}

/// Inserts `restore`'s queued payments whose block and spend evidence the scan now
/// covers, once its publication is accepted.
fn insert_ready<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    configured: Option<TransparentLedgerMode>,
    session: &SessionState,
    tpir_session: &TpirSessionState,
    restore: &Restore,
) -> Result<(), SqliteClientError> {
    if restore.state != ACCEPTED {
        return Ok(());
    }
    // The highest block the scan stored, as later blocks' spend evidence is still missing.
    let through: Option<(u32, [u8; 32])> = conn
        .query_row(
            "SELECT b.height, b.hash FROM blocks b WHERE EXISTS(SELECT 1 FROM scan_queue q
                 WHERE q.priority = ?1 AND b.height >= q.block_range_start
                   AND b.height < q.block_range_end)
             ORDER BY b.height DESC LIMIT 1",
            [priority_code(&ScanPriority::Scanned)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((height, hash)) = through else {
        return Ok(());
    };
    let through = ChainPoint {
        height: BlockHeight::from(height),
        hash: BlockHash(hash),
    };
    let account = account_uuid(conn, restore.account)?;
    let ready: Vec<(i64, KeyId)> = conn
        .prepare(
            "SELECT DISTINCT k.id, k.purpose, k.key_index
             FROM ironwood_one_time_payment_recovery p
             JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
             WHERE k.account_id = ?1 AND p.scan_hint = 0 AND p.witness IS NOT NULL",
        )?
        .query_map([restore.account], |r| {
            Ok((r.get(0)?, (r.get::<_, u8>(1)?, r.get::<_, Vec<u8>>(2)?)))
        })?
        .map(|row| {
            let (id, (purpose, index)) = row?;
            Ok((id, decoded_key(purpose, index)?))
        })
        .collect::<Result<_, SqliteClientError>>()?;
    for (id, key) in ready {
        for candidate in pending_payments(conn, account, key)? {
            let witness: Option<Vec<u8>> = conn.query_row(
                "SELECT witness FROM ironwood_one_time_payment_recovery
                 WHERE txid = ?1 AND action_index = ?2",
                params![candidate.txid.as_ref(), candidate.action_index],
                |r| r.get(0),
            )?;
            let path = witness
                .and_then(|bytes| siblings(&bytes))
                .and_then(|siblings| merkle_path(candidate.position, siblings));
            let Some(path) = path else {
                continue;
            };
            if !scanned(conn, candidate.height)? {
                continue;
            }
            let applied = apply_payment(
                conn,
                tpir_session,
                params,
                configured,
                session,
                account,
                key,
                &candidate,
                through,
                (restore.anchor, &path),
                Gate::Covered,
            )?;
            match applied {
                Ok(PaymentApplication::Applied)
                | Ok(PaymentApplication::AwaitingScan)
                | Ok(PaymentApplication::AwaitingWitness)
                | Ok(PaymentApplication::AwaitingSpendHistory)
                | Err(_) => {}
                // `apply_payment` discarded the key's candidates and reopened its sweep.
                Ok(PaymentApplication::Rejected) => {
                    fall_back(conn, id)?;
                    break;
                }
            }
        }
        settle(conn, id, restore)?;
    }
    Ok(())
}

/// Finishes key `id`'s early restore at the anchor once none of its payments wait.
fn settle(
    conn: &rusqlite::Transaction<'_>,
    id: i64,
    restore: &Restore,
) -> Result<(), SqliteClientError> {
    let changed = conn.execute(
        "UPDATE ironwood_one_time_sweeps SET done_height = ?2
         WHERE one_time_key_id = ?1 AND provisional = 1 AND done_height IS NULL
           AND NOT EXISTS(SELECT 1 FROM ironwood_one_time_payment_recovery
               WHERE one_time_key_id = ?1)",
        params![id, u32::from(restore.anchor.height)],
    )?;
    if changed > 0 && restore.state == ACCEPTED {
        close_done(conn, id, restore)?;
    }
    Ok(())
}

/// Closes done key `id` at the accepted anchor's block time unless it scans, as a
/// finished sweep does (see `sweep::finish`), and ends its provisional state.
fn close_done(
    conn: &rusqlite::Transaction<'_>,
    id: i64,
    restore: &Restore,
) -> Result<(), SqliteClientError> {
    conn.execute(
        "UPDATE ironwood_one_time_keys SET closed_at = COALESCE(closed_at,
             (SELECT time FROM blocks WHERE height = ?2))
         WHERE id = ?1 AND active_from IS NULL",
        params![id, u32::from(restore.anchor.height)],
    )?;
    conn.execute(
        "UPDATE ironwood_one_time_sweeps SET provisional = 0 WHERE one_time_key_id = ?1",
        [id],
    )?;
    Ok(())
}

/// Hands key `id` to an ordinary restore sweep, dropping the payments its early lookup
/// queued, which are all it holds while its restore is provisional.
fn fall_back(conn: &rusqlite::Transaction<'_>, id: i64) -> Result<(), SqliteClientError> {
    conn.execute(
        "DELETE FROM ironwood_one_time_payment_recovery WHERE one_time_key_id = ?1",
        [id],
    )?;
    conn.execute(
        "UPDATE ironwood_one_time_sweeps SET done_height = NULL, lookup_height = NULL,
             lookup_hash = NULL, next_attempt_at = 0, provisional = 0
         WHERE one_time_key_id = ?1",
        [id],
    )?;
    Ok(())
}

/// Whether the scan stored block `height` since the scan queue last required it.
fn scanned(conn: &Connection, height: BlockHeight) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM blocks WHERE height = ?1)
         AND EXISTS(SELECT 1 FROM scan_queue WHERE priority = ?2
             AND block_range_start <= ?1 AND block_range_end > ?1)",
        params![u32::from(height), priority_code(&ScanPriority::Scanned)],
        |r| r.get(0),
    )?)
}

/// The account's early restore, if any.
fn restore(conn: &Connection, account: i64) -> Result<Option<Restore>, SqliteClientError> {
    Ok(conn
        .query_row(
            "SELECT account_id, restore_from, anchor_height, anchor_hash, anchor_tree_size,
                 state FROM ironwood_one_time_restores WHERE account_id = ?1",
            [account],
            restore_row,
        )
        .optional()?)
}

/// Every early restore in `state`.
fn restores(conn: &Connection, state: u8) -> Result<Vec<Restore>, SqliteClientError> {
    Ok(conn
        .prepare(
            "SELECT account_id, restore_from, anchor_height, anchor_hash, anchor_tree_size,
                 state FROM ironwood_one_time_restores WHERE state = ?1",
        )?
        .query_map([state], restore_row)?
        .collect::<Result<_, _>>()?)
}

/// A restore from a row selecting its columns in declaration order.
fn restore_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Restore> {
    Ok(Restore {
        account: r.get(0)?,
        restore_from: r.get(1)?,
        anchor: ChainPoint {
            height: BlockHeight::from(r.get::<_, u32>(2)?),
            hash: BlockHash(r.get(3)?),
        },
        tree_size: r.get(4)?,
        state: r.get(5)?,
    })
}

/// A directory payment as a queued candidate, or `None` if a field is out of range.
fn pending_payment(
    payment: &DirectoryPayment,
) -> Result<Option<PendingPayment>, SqliteClientError> {
    let (Ok(position), Ok(tx_index)) = (
        u32::try_from(payment.position),
        u16::try_from(payment.tx_index),
    ) else {
        return Ok(None);
    };
    Ok(Some(PendingPayment {
        txid: TxId::from_bytes(payment.txid),
        action_index: payment.action_index,
        height: payment.height.into(),
        block_hash: BlockHash(payment.block_hash),
        tx_index,
        position,
        encrypted_note: EncryptedNote::from_parts(
            payment.action_nullifier,
            payment.cmx,
            payment.ephemeral_key,
            payment.ciphertext_prefix,
        ),
    }))
}

/// The 32 sibling hashes a stored witness holds.
fn siblings(bytes: &[u8]) -> Option<[[u8; 32]; 32]> {
    let (chunks, rest) = bytes.as_chunks::<32>();
    rest.is_empty().then(|| chunks.try_into().ok()).flatten()
}

/// Decodes a stored purpose code and key index.
fn decoded_key(purpose: u8, index: Vec<u8>) -> Result<KeyId, SqliteClientError> {
    let purpose = match purpose {
        0 => Purpose::Refund,
        1 => Purpose::Receive,
        _ => return Err(corrupt("unsupported one-time key purpose")),
    };
    let index: [u8; 8] = index
        .try_into()
        .map_err(|_| corrupt("invalid one-time key index"))?;
    Ok(KeyId::new(purpose, u64::from_be_bytes(index)))
}

/// The UUID of account row `id`.
fn account_uuid(conn: &Connection, id: i64) -> Result<AccountUuid, SqliteClientError> {
    Ok(AccountUuid(conn.query_row(
        "SELECT uuid FROM accounts WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?))
}

/// The account row of registered key `id`.
fn account_ref_of(conn: &Connection, id: i64) -> Result<i64, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT account_id FROM ironwood_one_time_keys WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?)
}
