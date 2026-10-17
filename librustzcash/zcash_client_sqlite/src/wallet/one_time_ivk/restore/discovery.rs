//! Discovery's store side (see `OneTimeIvkRestoreWrite`).
//!
//! An account's pass checks candidate keys against one publication's filters. Each paid
//! key's lookup pages through its history up to that publication's end (the cutoff). A
//! payment whose block the scan has not stored is a scan hint: the batch holding that
//! block tries its key (see `one_time_ivk::scanning_keys_for_range`). Any other payment is
//! a late candidate, admitted by `apply::apply_payment` once its publication matched the
//! wallet's chain, it has an inclusion path, and the scan covers every block from it on
//! for the account. The spend journal then says whether it was spent.
//!
//! Every publication discovery reads from is judged against the wallet's chain when the
//! scan stores its last block, or at once if the block is already stored.
use std::ops::Range;

use rusqlite::{Connection, OptionalExtension, params};
use zcash_client_backend::data_api::{
    Account as _,
    one_time_ivk::restore::{
        DirectoryPayment, DiscoveryStatus, DiscoveryWork, EnumerationCursor, FilterCheck,
        LookupWork, PublicationEnd, WitnessRequest,
    },
    scanning::ScanPriority,
    transparent_ledger::ChainPoint,
};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

use super::{
    apply::{Admission, Sessions, apply_payment, merkle_path},
    payments::{authenticate, directory_candidate, pending_payment, queue_payment},
    recovery::recover_refund_memos,
};
use crate::wallet::one_time_ivk::{
    Discovery, KeyId, Purpose, account_key, activate, corrupt, index_after, key_ref, register,
    restore_start, stored_key_id, swap::reservations::record_provider_seen,
};
use crate::{
    AccountUuid,
    error::SqliteClientError,
    wallet::{self, scanning::priority_code},
};

/// SQL condition on account `?1`'s key `k`: closed with no mined payment and none before the
/// birthday, so only a newer publication's paid filter would show a payment after it closed.
const UNPAID_CLOSED: &str = "k.account_id = ?1 AND k.closed_at IS NOT NULL
    AND k.paid_before_birthday = 0
    AND NOT EXISTS(SELECT 1 FROM ironwood_received_notes n
        JOIN transactions t ON t.id_tx = n.transaction_id
        WHERE n.one_time_key_id = k.id AND t.mined_height IS NOT NULL)";

/// `ironwood_one_time_publications.state` until the scan stores the publication's last block.
const PROVISIONAL: u8 = 0;
/// `ironwood_one_time_publications.state` once that block matched.
const ACCEPTED: u8 = 1;
/// `ironwood_one_time_publications.state` once that block did not match.
const REJECTED: u8 = 2;

/// An account's discovery pass against one publication.
struct Pass {
    publication: PublicationEnd,
    state: u8,
    receive: (EnumerationCursor, bool),
    refund: (EnumerationCursor, bool),
}

/// The pass of account `owner`, if one has started.
fn pass(conn: &Connection, owner: i64) -> Result<Option<Pass>, SqliteClientError> {
    Ok(conn
        .query_row(
            "SELECT d.publication_height, d.publication_hash, p.tree_size, p.state,
                d.receive_next, d.receive_reach, d.receive_done,
                d.refund_next, d.refund_reach, d.refund_done
             FROM ironwood_one_time_discovery d
             JOIN ironwood_one_time_publications p
               ON p.height = d.publication_height AND p.hash = d.publication_hash
             WHERE d.account_id = ?1",
            [owner],
            |r| {
                let cursor = |next: usize| -> rusqlite::Result<(EnumerationCursor, bool)> {
                    Ok((
                        EnumerationCursor {
                            next: r.get(next)?,
                            reach: r.get(next + 1)?,
                        },
                        r.get(next + 2)?,
                    ))
                };
                Ok(Pass {
                    publication: PublicationEnd {
                        height: BlockHeight::from(r.get::<_, u32>(0)?),
                        hash: r.get(1)?,
                        tree_size: r.get(2)?,
                    },
                    state: r.get(3)?,
                    receive: cursor(4)?,
                    refund: cursor(7)?,
                })
            },
        )
        .optional()?)
}

/// Whether account `owner` was declared fresh, so it has no history to discover.
fn fresh(conn: &Connection, owner: i64) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_fresh_accounts WHERE account_id = ?1)",
        [owner],
        |r| r.get(0),
    )?)
}

/// Whether account `owner`'s discovery is complete (see `DiscoveryStatus::complete`): no
/// lookup is due, and every publication a lookup cursor read from matched the wallet's
/// chain.
pub(in crate::wallet::one_time_ivk) fn complete<P: Parameters>(
    conn: &Connection,
    params: &P,
    owner: crate::AccountRef,
) -> Result<bool, SqliteClientError> {
    let settled = |conn: &Connection| -> Result<bool, SqliteClientError> {
        Ok(conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM ironwood_one_time_lookups l
                 JOIN ironwood_one_time_keys k ON k.id = l.one_time_key_id
                 WHERE k.account_id = ?1 AND l.cutoff_height IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM ironwood_one_time_lookup_reads r
                 JOIN ironwood_one_time_keys k ON k.id = r.one_time_key_id
                 JOIN ironwood_one_time_publications p
                   ON p.height = r.publication_height AND p.hash = r.publication_hash
                 WHERE k.account_id = ?1 AND p.state != ?2)",
            params![owner.0, ACCEPTED],
            |r| r.get(0),
        )?)
    };
    // A fresh account has no restored history to discover, but once it starts a pass to
    // recheck a key it issued, that pass and its lookups must finish too.
    let Some(pass) = pass(conn, owner.0)? else {
        return Ok(fresh(conn, owner.0)? && settled(conn)?);
    };
    let before_birthday = pass.publication.height < restore_start(conn, params, owner)?;
    let judged = pass.state == ACCEPTED || (pass.state == PROVISIONAL && before_birthday);
    Ok(judged && pass.receive.1 && pass.refund.1 && settled(conn)?)
}

/// Whether `account` may be issued new swap addresses: it was declared fresh, or its pass
/// checked both purposes against a publication that matched the wallet's chain (or ended
/// before the birthday). Every index the filters mark used is then registered, so none is
/// issued again; a pending lookup can only find payments to an index already registered.
pub(in crate::wallet::one_time_ivk) fn issuance_ready<P: Parameters>(
    conn: &Connection,
    params: &P,
    owner: crate::AccountRef,
) -> Result<bool, SqliteClientError> {
    if fresh(conn, owner.0)? {
        return Ok(true);
    }
    let Some(pass) = pass(conn, owner.0)? else {
        return Ok(false);
    };
    let before_birthday = pass.publication.height < restore_start(conn, params, owner)?;
    let judged = pass.state == ACCEPTED || (pass.state == PROVISIONAL && before_birthday);
    Ok(judged && pass.receive.1 && pass.refund.1)
}

/// Implements `OneTimeIvkRestoreRead::one_time_discovery`.
pub(crate) fn status<P: Parameters>(
    conn: &Connection,
    params: &P,
    account: AccountUuid,
    now: i64,
) -> Result<Option<DiscoveryStatus>, SqliteClientError> {
    if params.activation_height(NetworkUpgrade::Nu6_3).is_none() {
        return Ok(None);
    }
    let stored =
        wallet::get_account(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
    if stored.ufvk().and_then(|key| key.orchard()).is_none() {
        return Ok(None);
    }
    let (owner, _) = account_key(conn, params, account)?;
    // A fresh account has no restored history, so it needs discovery only once a key it
    // issued closed without a payment, which a newer publication could still show paid, and
    // until the work that started, or a recheck asked for, finishes.
    let unpaid_closed: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM ironwood_one_time_keys k WHERE {UNPAID_CLOSED})"),
        [owner.0],
        |r| r.get(0),
    )?;
    if fresh(conn, owner.0)?
        && !unpaid_closed
        && complete(conn, params, owner)?
        && !conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_lookups l
                 JOIN ironwood_one_time_keys k ON k.id = l.one_time_key_id
                 WHERE k.account_id = ?1 AND l.done_height IS NULL)
             OR EXISTS(SELECT 1 FROM ironwood_one_time_payment_recovery p
                 JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
                 WHERE k.account_id = ?1)",
            [owner.0],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(None);
    }
    let current = pass(conn, owner.0)?;
    let enumeration = match &current {
        None => Some([
            (Purpose::Receive, EnumerationCursor::default()),
            (Purpose::Refund, EnumerationCursor::default()),
        ]),
        Some(pass) if pass.receive.1 && pass.refund.1 => None,
        Some(pass) => Some([
            (Purpose::Receive, pass.receive.0),
            (Purpose::Refund, pass.refund.0),
        ]),
    };
    // Nothing is due against a rejected pass until a new publication starts another.
    let usable = current.as_ref().is_some_and(|pass| pass.state != REJECTED);
    let lookups = if usable {
        conn.prepare(
            "SELECT k.purpose, k.key_index, k.receiver, l.cutoff_height, l.applied,
                l.last_position, l.last_nullifier
             FROM ironwood_one_time_lookups l
             JOIN ironwood_one_time_keys k ON k.id = l.one_time_key_id
             WHERE k.account_id = ?1 AND l.cutoff_height IS NOT NULL AND l.next_attempt_at <= ?2
             ORDER BY l.next_attempt_at, k.id",
        )?
        .query_and_then(params![owner.0, now], |r| {
            Ok::<_, SqliteClientError>(LookupWork {
                work: DiscoveryWork {
                    key: stored_key_id(r)?,
                    receiver: r
                        .get::<_, Vec<u8>>(2)?
                        .try_into()
                        .map_err(|_| corrupt("invalid stored receiver"))?,
                },
                cutoff: BlockHeight::from(r.get::<_, u32>(3)?),
                applied: r.get(4)?,
                last: r
                    .get::<_, Option<u64>>(5)?
                    .zip(r.get::<_, Option<[u8; 32]>>(6)?),
            })
        })?
        .collect::<Result<_, _>>()?
    } else {
        vec![]
    };
    let witnesses = conn
        .prepare(
            "SELECT k.purpose, k.key_index, p.position, substr(p.encrypted_note, 33, 32)
             FROM ironwood_one_time_payment_recovery p
             JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
             JOIN ironwood_one_time_publications u
               ON u.height = p.publication_height AND u.hash = p.publication_hash
             WHERE k.account_id = ?1 AND p.scan_hint = 0 AND p.witness IS NULL
               AND u.state != ?2
             ORDER BY p.height, p.position",
        )?
        .query_and_then(params![owner.0, REJECTED], |r| {
            Ok::<_, SqliteClientError>(WitnessRequest {
                key: stored_key_id(r)?,
                position: r.get(2)?,
                cmx: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let complete = complete(conn, params, owner)?;
    let refresh_due = complete && unpaid_closed;
    Ok(Some(DiscoveryStatus {
        // A rejected pass's publication is no longer the account's, so the next one starts
        // a new pass.
        publication: current
            .as_ref()
            .filter(|pass| pass.state != REJECTED)
            .map(|pass| pass.publication),
        complete,
        refresh_due,
        enumeration,
        lookups,
        witnesses,
    }))
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

/// Records `publication` if it is new, and judges it at once if the scan stored its block.
/// Returns its state, or `None` if a claim for the same block with another tree size is
/// accepted or not judged yet.
fn note_publication(
    conn: &rusqlite::Transaction<'_>,
    publication: PublicationEnd,
) -> Result<Option<u8>, SqliteClientError> {
    let height = u32::from(publication.height);
    let stored: Option<(u64, u8)> = conn
        .query_row(
            "SELECT tree_size, state FROM ironwood_one_time_publications
             WHERE height = ?1 AND hash = ?2",
            params![height, publication.hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match stored {
        None => {
            conn.execute(
                "INSERT INTO ironwood_one_time_publications (height, hash, tree_size)
                 VALUES (?1, ?2, ?3)",
                params![height, publication.hash, publication.tree_size],
            )?;
        }
        Some((size, _)) if size == publication.tree_size => {}
        // A rejected claim for this block gives way to a corrected size, judged afresh.
        Some((_, REJECTED)) => {
            conn.execute(
                "UPDATE ironwood_one_time_publications SET tree_size = ?3, state = ?4
                 WHERE height = ?1 AND hash = ?2",
                params![height, publication.hash, publication.tree_size, PROVISIONAL],
            )?;
        }
        Some(_) => return Ok(None),
    }
    judge(conn, publication.height)?;
    Ok(Some(conn.query_row(
        "SELECT state FROM ironwood_one_time_publications WHERE height = ?1 AND hash = ?2",
        params![height, publication.hash],
        |r| r.get(0),
    )?))
}

/// Implements `OneTimeIvkRestoreWrite::record_one_time_filter_checks`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_filter_checks<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    account: AccountUuid,
    publication: PublicationEnd,
    purpose: Purpose,
    checked: &[FilterCheck],
    cursor: EnumerationCursor,
    finished: bool,
    now: i64,
) -> Result<bool, SqliteClientError> {
    if params.activation_height(NetworkUpgrade::Nu6_3).is_none()
        || wallet::chain_tip_height(conn)?.is_none_or(|tip| publication.height > tip)
    {
        return Ok(false);
    }
    let (owner, _) = account_key(conn, params, account)?;
    let start = restore_start(conn, params, owner)?;
    match pass(conn, owner.0)? {
        Some(current)
            if current.publication.height == publication.height
                && current.publication.hash == publication.hash
                && current.publication.tree_size == publication.tree_size => {}
        Some(current) if publication.height < current.publication.height => return Ok(false),
        _ => {
            if note_publication(conn, publication)?.is_none() {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO ironwood_one_time_discovery
                    (account_id, publication_height, publication_hash)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (account_id) DO UPDATE SET
                    publication_height = excluded.publication_height,
                    publication_hash = excluded.publication_hash,
                    receive_next = 0, receive_reach = 0, receive_done = 0,
                    refund_next = 0, refund_reach = 0, refund_done = 0",
                params![owner.0, u32::from(publication.height), publication.hash],
            )?;
            // A key registered for a lookup before any pass, such as a refund key from a
            // funding memo, is due in this one, and a lookup still due from an earlier pass
            // reads on to this one's end. Each keeps its cursor.
            if publication.height >= start {
                conn.execute(
                    "UPDATE ironwood_one_time_lookups SET cutoff_height = ?2
                     WHERE ((cutoff_height IS NULL AND done_height IS NULL)
                         OR cutoff_height < ?2)
                       AND one_time_key_id IN (
                           SELECT id FROM ironwood_one_time_keys WHERE account_id = ?1)",
                    params![owner.0, u32::from(publication.height)],
                )?;
            }
            recover_refund_memos(conn, params, account, now)?;
        }
    }
    // A publication ending before the birthday holds no payment in the account's range, so
    // its paid keys are protected from issuance but not looked up.
    let in_range = publication.height >= start;
    for check in checked {
        let used = check.seen || check.paid;
        // Refund issuance skips registered indices, so only a used refund key registers.
        let watched = check.recent && purpose == Purpose::Receive;
        if check.key.purpose() != purpose || !(used || watched) {
            continue;
        }
        // A key an earlier pass discovered already had its watch; another pass that lists
        // it as recent (as every key is without a current recent set) must not reopen it.
        let discovered: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_one_time_lookups l
                 JOIN ironwood_one_time_keys k ON k.id = l.one_time_key_id
                 WHERE k.account_id = ?1 AND k.purpose = ?2 AND k.key_index = ?3)",
            params![
                owner.0,
                u8::from(purpose == Purpose::Receive),
                check.key.index().to_be_bytes()
            ],
            |r| r.get(0),
        )?;
        let (id, _) = register(
            conn,
            params,
            account,
            check.key,
            start,
            used,
            Discovery::Funding,
            now,
        )?;
        if used {
            record_provider_seen(conn, id)?;
        }
        if check.paid && in_range {
            // A later pass reads a paid key's history again only if the key has no known
            // payment: an earlier pass found it unpaid, so the filter is newly positive. The
            // cursor keeps what earlier passes applied.
            conn.execute(
                &format!(
                    "INSERT INTO ironwood_one_time_lookups (one_time_key_id, cutoff_height)
                     VALUES (?2, ?3)
                     ON CONFLICT (one_time_key_id) DO UPDATE SET
                         cutoff_height = MAX(COALESCE(cutoff_height, 0), excluded.cutoff_height)
                     WHERE cutoff_height IS NOT NULL OR done_height IS NULL
                       OR (done_height < excluded.cutoff_height
                           AND EXISTS(SELECT 1 FROM ironwood_one_time_keys k
                               WHERE k.id = ?2 AND {UNPAID_CLOSED}))"
                ),
                params![owner.0, id, u32::from(publication.height)],
            )?;
        } else {
            // The row marks the key as discovered; there is nothing to look up.
            conn.execute(
                "INSERT OR IGNORE INTO ironwood_one_time_lookups (one_time_key_id, done_height)
                 VALUES (?1, ?2)",
                params![id, u32::from(publication.height)],
            )?;
        }
        if check.recent && !discovered {
            activate(conn, id, start.max(publication.height + 1))?;
        }
    }
    let columns = match purpose {
        Purpose::Receive => ("receive_next", "receive_reach", "receive_done"),
        Purpose::Refund => ("refund_next", "refund_reach", "refund_done"),
        _ => {
            return Err(SqliteClientError::InvalidOneTimeIvkInput(
                "unsupported key purpose",
            ));
        }
    };
    conn.execute(
        &format!(
            "UPDATE ironwood_one_time_discovery SET {} = ?2, {} = ?3, {} = ?4
             WHERE account_id = ?1",
            columns.0, columns.1, columns.2
        ),
        params![owner.0, cursor.next, cursor.reach, finished],
    )?;
    // `judge` closes a pass's keys when it accepts the publication, which happened before
    // these keys registered if the scan had already stored its block.
    if pass(conn, owner.0)?.is_some_and(|pass| pass.state == ACCEPTED) {
        close_looked_up(conn, owner.0)?;
    }
    Ok(true)
}

/// Implements `OneTimeIvkRestoreWrite::record_one_time_lookup`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_lookup<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    sessions: &Sessions<'_>,
    account: AccountUuid,
    key: KeyId,
    revision: PublicationEnd,
    page: u32,
    records: &[DirectoryPayment],
    complete: bool,
    now: i64,
) -> Result<bool, SqliteClientError> {
    let (owner, _) = account_key(conn, params, account)?;
    let id = key_ref(conn, account, key)?;
    let cursor: Option<(Option<u32>, u32)> = conn
        .query_row(
            "SELECT cutoff_height, applied FROM ironwood_one_time_lookups
             WHERE one_time_key_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((Some(cutoff), applied)) = cursor else {
        return Ok(false);
    };
    // A revision ending below the cutoff cannot show the history through it.
    if (page != applied && page != 0) || u32::from(revision.height) < cutoff {
        return Ok(false);
    }
    if note_publication(conn, revision)?.is_none_or(|state| state == REJECTED) {
        return Ok(false);
    }
    if page == 0 && applied > 0 {
        restart_cursor(conn, "?1", id)?;
    }
    let start = restore_start(conn, params, owner)?;
    let mut last = None;
    for payment in records {
        let Some(candidate) = directory_candidate(payment) else {
            return defer(conn, id, now);
        };
        if u32::from(candidate.height) > cutoff {
            return defer(conn, id, now);
        }
        let note = match authenticate(conn, params, account, key, &candidate) {
            Ok((_, note)) => note,
            Err(SqliteClientError::InvalidOneTimeIvkInput(_)) => return defer(conn, id, now),
            Err(e) => return Err(e),
        };
        // A note the scan already found stays as it is.
        let found: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes WHERE nf = ?1)",
            [note.nullifier().to_bytes()],
            |r| r.get(0),
        )?;
        if !found {
            let hint = candidate.height >= start && !scanned(conn, candidate.height)?;
            match queue_payment(conn, params, account, key, &candidate, revision, hint) {
                Ok(()) => {}
                Err(SqliteClientError::InvalidOneTimeIvkInput(_)) => {
                    return defer(conn, id, now);
                }
                Err(e) => return Err(e),
            }
        }
        last = Some((
            candidate.position,
            payment.action_nullifier,
            candidate.height,
        ));
    }
    if let Some((position, nullifier, height)) = last {
        conn.execute(
            "UPDATE ironwood_one_time_lookups
             SET applied = ?2, last_position = ?3, last_nullifier = ?4, last_height = ?5
             WHERE one_time_key_id = ?1",
            params![
                id,
                page + u32::try_from(records.len()).map_err(|_| corrupt("too many records"))?,
                position,
                nullifier,
                u32::from(height)
            ],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO ironwood_one_time_lookup_reads
            (one_time_key_id, publication_height, publication_hash)
         VALUES (?1, ?2, ?3)",
        params![id, u32::from(revision.height), revision.hash],
    )?;
    if complete {
        conn.execute(
            "UPDATE ironwood_one_time_lookups
             SET done_height = cutoff_height, cutoff_height = NULL, attempts = 0,
                 next_attempt_at = 0
             WHERE one_time_key_id = ?1",
            [id],
        )?;
        if pass(conn, owner.0)?.is_some_and(|pass| pass.state == ACCEPTED) {
            close_looked_up(conn, owner.0)?;
        }
    }
    admit(conn, params, sessions, account)?;
    Ok(true)
}

/// Backs lookup `id` off from `now`, from one minute to twelve hours, and returns false.
fn defer(conn: &Connection, id: i64, now: i64) -> Result<bool, SqliteClientError> {
    let attempts: u32 = conn.query_row(
        "SELECT attempts FROM ironwood_one_time_lookups WHERE one_time_key_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    let delay = (60i64 << attempts.min(10)).min(43200);
    conn.execute(
        "UPDATE ironwood_one_time_lookups
         SET attempts = MIN(attempts + 1, 30), next_attempt_at = ?2
         WHERE one_time_key_id = ?1",
        params![id, now.saturating_add(delay)],
    )?;
    Ok(false)
}

/// Implements `OneTimeIvkRestoreWrite::defer_one_time_lookup`.
pub(crate) fn defer_lookup(
    conn: &Connection,
    account: AccountUuid,
    key: KeyId,
    now: i64,
) -> Result<(), SqliteClientError> {
    defer(conn, key_ref(conn, account, key)?, now).map(|_| ())
}

/// Restarts from page zero the lookup cursors that SQL `keys` selects, with `?1` bound to
/// `param`, and makes each due again at the cutoff it last finished or still has.
fn restart_cursor(
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

/// Implements `OneTimeIvkRestoreWrite::record_one_time_witness`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_witness<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    sessions: &Sessions<'_>,
    account: AccountUuid,
    key: KeyId,
    position: u32,
    anchor: ChainPoint,
    siblings: [[u8; 32]; 32],
) -> Result<(), SqliteClientError> {
    let id = key_ref(conn, account, key)?;
    conn.execute(
        "UPDATE ironwood_one_time_payment_recovery
         SET witness = ?3, witness_height = ?4, witness_hash = ?5
         WHERE one_time_key_id = ?1 AND position = ?2 AND scan_hint = 0",
        params![
            id,
            position,
            siblings.concat(),
            u32::from(anchor.height),
            anchor.hash.0
        ],
    )?;
    admit(conn, params, sessions, account)
}

/// Runs after the scan stores `range`, with one-time keys `keys` if it used them: judges
/// the publications ending in it, settles its hints, recovers refund keys from newly
/// stored funding memos at `now`, and admits the late candidates the scan now covers.
pub(crate) fn after_blocks_stored<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    sessions: &Sessions<'_>,
    keys: Option<&[(AccountUuid, KeyId)]>,
    range: Range<BlockHeight>,
    now: i64,
) -> Result<(), SqliteClientError> {
    let heights: Vec<u32> = conn
        .prepare(
            "SELECT DISTINCT height FROM ironwood_one_time_publications
             WHERE state = ?1 AND height >= ?2 AND height < ?3",
        )?
        .query_map(
            params![PROVISIONAL, u32::from(range.start), u32::from(range.end)],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    for height in heights {
        judge(conn, BlockHeight::from(height))?;
    }
    check_hints(conn, keys.unwrap_or(&[]), range)?;
    let accounts: Vec<AccountUuid> = conn
        .prepare(
            "SELECT a.uuid FROM ironwood_one_time_discovery d
             JOIN accounts a ON a.id = d.account_id",
        )?
        .query_map([], |r| r.get(0).map(AccountUuid))?
        .collect::<Result<_, _>>()?;
    for account in accounts {
        recover_refund_memos(conn, params, account, now)?;
        admit(conn, params, sessions, account)?;
    }
    Ok(())
}

/// Accepts each provisional publication ending at `height` whose last block and tree size
/// match the stored block, and rejects any that does not. Nothing changes until the scan
/// stores that block.
fn judge(conn: &rusqlite::Transaction<'_>, height: BlockHeight) -> Result<(), SqliteClientError> {
    let stored: Option<([u8; 32], Option<u64>)> = conn
        .query_row(
            "SELECT hash, ironwood_commitment_tree_size FROM blocks WHERE height = ?1",
            [u32::from(height)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((hash, tree_size)) = stored else {
        return Ok(());
    };
    let provisional: Vec<([u8; 32], u64)> = conn
        .prepare(
            "SELECT hash, tree_size FROM ironwood_one_time_publications
             WHERE height = ?1 AND state = ?2",
        )?
        .query_map(params![u32::from(height), PROVISIONAL], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    for (claimed, claimed_size) in provisional {
        if claimed == hash && tree_size == Some(claimed_size) {
            conn.execute(
                "UPDATE ironwood_one_time_publications SET state = ?3
                 WHERE height = ?1 AND hash = ?2",
                params![u32::from(height), claimed, ACCEPTED],
            )?;
            let owners: Vec<i64> = conn
                .prepare(
                    "SELECT account_id FROM ironwood_one_time_discovery
                     WHERE publication_height = ?1 AND publication_hash = ?2",
                )?
                .query_map(params![u32::from(height), claimed], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for owner in owners {
                close_looked_up(conn, owner)?;
            }
        } else {
            reject(conn, height, claimed)?;
        }
    }
    Ok(())
}

/// Rejects the publication ending at `height` with hash `hash`: every lookup cursor that read
/// from it restarts, and the candidates it supplied that are not notes yet are dropped. An
/// account whose pass used it waits for a new publication.
fn reject(
    conn: &rusqlite::Transaction<'_>,
    height: BlockHeight,
    hash: [u8; 32],
) -> Result<(), SqliteClientError> {
    let height = u32::from(height);
    conn.execute(
        "UPDATE ironwood_one_time_publications SET state = ?3 WHERE height = ?1 AND hash = ?2",
        params![height, hash, REJECTED],
    )?;
    conn.execute(
        "DELETE FROM ironwood_one_time_payment_recovery
         WHERE publication_height = ?1 AND publication_hash = ?2",
        params![height, hash],
    )?;
    let readers: Vec<i64> = conn
        .prepare(
            "SELECT one_time_key_id FROM ironwood_one_time_lookup_reads
             WHERE publication_height = ?1 AND publication_hash = ?2",
        )?
        .query_map(params![height, hash], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in readers {
        restart_cursor(conn, "?1", id)?;
    }
    Ok(())
}

/// Closes account `owner`'s keys whose lookups are done and which neither scan nor belong to
/// a reservation or operation of this wallet, at the time of the block the lookup finished
/// at, as a found restore key does once its history is known.
fn close_looked_up(conn: &Connection, owner: i64) -> Result<(), SqliteClientError> {
    conn.execute(
        "UPDATE ironwood_one_time_keys SET closed_at = COALESCE(closed_at,
             (SELECT b.time FROM ironwood_one_time_lookups l
              JOIN blocks b ON b.height = l.done_height
              WHERE l.one_time_key_id = ironwood_one_time_keys.id))
         WHERE account_id = ?1 AND active_from IS NULL AND reserved_at IS NULL
           AND id IN (SELECT one_time_key_id FROM ironwood_one_time_lookups
               WHERE cutoff_height IS NULL AND done_height IS NOT NULL)
           AND NOT EXISTS(SELECT 1 FROM ironwood_one_time_operations o
               WHERE o.one_time_key_id = ironwood_one_time_keys.id)",
        [owner],
    )?;
    Ok(())
}

/// Settles the hints in `range`, which the scan just stored with one-time keys `keys`. A
/// hint is satisfied only by its own note. One whose key the batch did not try, because
/// the lookup came in after the scanner took its keys or the scan used none, becomes a late
/// candidate. One the batch tried and missed is the directory's error: its key's lookup
/// restarts.
fn check_hints(
    conn: &rusqlite::Transaction<'_>,
    keys: &[(AccountUuid, KeyId)],
    range: Range<BlockHeight>,
) -> Result<(), SqliteClientError> {
    let hints: Vec<(KeyId, AccountUuid, i64, TxId, u32, u32, Vec<u8>)> = conn
        .prepare(
            "SELECT k.purpose, k.key_index, a.uuid, p.one_time_key_id, p.txid,
                 p.action_index, p.position, substr(p.encrypted_note, 1, 32)
             FROM ironwood_one_time_payment_recovery p
             JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
             JOIN accounts a ON a.id = k.account_id
             WHERE p.scan_hint = 1 AND p.height >= ?1 AND p.height < ?2",
        )?
        .query_and_then(params![u32::from(range.start), u32::from(range.end)], |r| {
            Ok::<_, SqliteClientError>((
                stored_key_id(r)?,
                AccountUuid(r.get(2)?),
                r.get(3)?,
                TxId::from_bytes(r.get(4)?),
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (key, account, id, txid, action, position, rho) in hints {
        if !keys.contains(&(account, key)) {
            conn.execute(
                "UPDATE ironwood_one_time_payment_recovery SET scan_hint = 0
                 WHERE txid = ?1 AND action_index = ?2",
                params![txid.as_ref(), action],
            )?;
            continue;
        }
        // The note's rho is the action's nullifier, which the candidate carries, so another
        // payment to the key at the claimed position does not satisfy the hint.
        let found: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ironwood_received_notes
             WHERE one_time_key_id = ?1 AND commitment_tree_position = ?2 AND rho = ?3)",
            params![id, position, rho],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM ironwood_one_time_payment_recovery WHERE txid = ?1 AND action_index = ?2",
            params![txid.as_ref(), action],
        )?;
        if !found {
            restart_cursor(conn, "?1", id)?;
        }
    }
    Ok(())
}

/// Admits `account`'s late candidates that have an inclusion path, whose publication matched
/// the wallet's chain, and that the scan now covers (see `apply::apply_payment`).
pub(in crate::wallet::one_time_ivk) fn admit<P: Parameters>(
    conn: &rusqlite::Transaction<'_>,
    params: &P,
    sessions: &Sessions<'_>,
    account: AccountUuid,
) -> Result<(), SqliteClientError> {
    let Some(through) = highest_scanned(conn)? else {
        return Ok(());
    };
    let (owner, _) = account_key(conn, params, account)?;
    let waiting: Vec<(i64, KeyId, TxId, u32, Vec<u8>, u32, [u8; 32])> = conn
        .prepare(
            "SELECT k.id, k.purpose, k.key_index, p.txid, p.action_index, p.witness,
                 p.witness_height, p.witness_hash
             FROM ironwood_one_time_payment_recovery p
             JOIN ironwood_one_time_keys k ON k.id = p.one_time_key_id
             JOIN ironwood_one_time_publications u
               ON u.height = p.publication_height AND u.hash = p.publication_hash
             WHERE k.account_id = ?1 AND p.scan_hint = 0 AND p.witness IS NOT NULL
               AND u.state = ?2
             ORDER BY p.height, p.position",
        )?
        .query_and_then(params![owner.0, ACCEPTED], |r| {
            let key = {
                let purpose = match r.get::<_, u8>(1)? {
                    0 => Purpose::Refund,
                    1 => Purpose::Receive,
                    _ => return Err(corrupt("unsupported one-time key purpose")),
                };
                KeyId::new(
                    purpose,
                    crate::wallet::one_time_ivk::decode_index(r.get(2)?)?,
                )
            };
            Ok::<_, SqliteClientError>((
                r.get(0)?,
                key,
                TxId::from_bytes(r.get(3)?),
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (id, key, txid, action, witness, anchor_height, anchor_hash) in waiting {
        let Some(candidate) = pending_payment(conn, txid, action)? else {
            continue;
        };
        let anchor = ChainPoint {
            height: BlockHeight::from(anchor_height),
            hash: BlockHash(anchor_hash),
        };
        let path = siblings(&witness).and_then(|s| merkle_path(candidate.position, s));
        let admission = match path {
            Some(path) => apply_payment(
                conn,
                sessions,
                params,
                account,
                key,
                &candidate,
                through,
                (anchor, &path),
            )?,
            None => Admission::StaleWitness,
        };
        match admission {
            Admission::Admitted | Admission::Waiting => {}
            Admission::Uncovered => rescan_uncovered(conn, owner.0, candidate.height, through)?,
            Admission::StaleWitness => {
                conn.execute(
                    "UPDATE ironwood_one_time_payment_recovery
                     SET witness = NULL, witness_height = NULL, witness_hash = NULL
                     WHERE txid = ?1 AND action_index = ?2",
                    params![txid.as_ref(), action],
                )?;
            }
            Admission::Rejected => {
                conn.execute(
                    "DELETE FROM ironwood_one_time_payment_recovery
                     WHERE txid = ?1 AND action_index = ?2",
                    params![txid.as_ref(), action],
                )?;
                restart_cursor(conn, "?1", id)?;
            }
        }
    }
    Ok(())
}

/// Queues for a rescan each stretch from `from` through `through` that the scan finished
/// without covering for account `owner`, as blocks scanned before the journal existed or by
/// the full-block scanner are, so a late payment there is not left waiting for good.
fn rescan_uncovered(
    conn: &rusqlite::Transaction<'_>,
    owner: i64,
    from: BlockHeight,
    through: ChainPoint,
) -> Result<(), SqliteClientError> {
    let covered: Vec<(u32, u32)> = conn
        .prepare(
            "SELECT start_height, end_height FROM ironwood_journal_coverage
             WHERE account_id = ?1 AND end_height > ?2 AND start_height <= ?3
             ORDER BY start_height",
        )?
        .query_map(
            params![owner, u32::from(from), u32::from(through.height)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;
    let end = u32::from(through.height) + 1;
    let mut gaps = vec![];
    let mut next = u32::from(from);
    for (start, stop) in covered {
        if start > next {
            gaps.push(next..start.min(end));
        }
        next = next.max(stop);
    }
    if next < end {
        gaps.push(next..end);
    }
    for gap in gaps {
        // Only a stretch the scan finished; one still queued is covered when it is scanned.
        let finished: bool = conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM scan_queue
             WHERE priority != ?3 AND block_range_start < ?2 AND block_range_end > ?1)",
            params![gap.start, gap.end, priority_code(&ScanPriority::Scanned)],
            |r| r.get(0),
        )?;
        if finished {
            crate::wallet::one_time_ivk::queue_rescan(
                conn,
                BlockHeight::from(gap.start)..BlockHeight::from(gap.end),
            )?;
        }
    }
    Ok(())
}

/// The highest block the scan stored that the scan queue no longer requires.
fn highest_scanned(conn: &Connection) -> Result<Option<ChainPoint>, SqliteClientError> {
    Ok(conn
        .query_row(
            "SELECT b.height, b.hash FROM blocks b WHERE EXISTS(SELECT 1 FROM scan_queue q
                 WHERE q.priority = ?1 AND b.height >= q.block_range_start
                   AND b.height < q.block_range_end)
             ORDER BY b.height DESC LIMIT 1",
            [priority_code(&ScanPriority::Scanned)],
            |r| {
                Ok(ChainPoint {
                    height: BlockHeight::from(r.get::<_, u32>(0)?),
                    hash: BlockHash(r.get(1)?),
                })
            },
        )
        .optional()?)
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

/// Repairs discovery for a rewind whose blocks above `rescan_floor` may be replaced: a
/// publication ending there is judged again, a late candidate there becomes a hint again
/// (its block is rescanned), a path anchored there is requested again, and a cursor that
/// passed a record there restarts, since the replacement chain may put a record before it.
pub(crate) fn truncate(
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

/// Reopens account `owner`'s discovery after its birthday was lowered: the next pass checks
/// every key again and every paid key's history from the start, since outcomes outside the
/// old range no longer hold. The account is no longer treated as fresh.
pub(crate) fn reopen(
    conn: &rusqlite::Transaction<'_>,
    owner: i64,
) -> Result<(), SqliteClientError> {
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

/// The 32 sibling hashes a stored witness holds.
fn siblings(bytes: &[u8]) -> Option<[[u8; 32]; 32]> {
    let (chunks, rest) = bytes.as_chunks::<32>();
    rest.is_empty().then(|| chunks.try_into().ok()).flatten()
}
