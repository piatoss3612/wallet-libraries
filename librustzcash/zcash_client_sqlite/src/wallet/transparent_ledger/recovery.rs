//! Candidate recovery storage: watched addresses, events, coverage, and pending pages.
//!
//! Candidate writes touch only `tpir_*` recovery tables. They never change the LRZ outputs,
//! spends, locks, addresses, or transactions that balances, input selection, receiving-address
//! allocation, and history read, and the candidate ledger never reads those back as evidence.
//!
//! Rewinds and policy transitions maintain recovery state in every build, because another
//! build may have written it.

use rusqlite::{OptionalExtension as _, named_params};
use zcash_protocol::consensus::BlockHeight;

use crate::error::SqliteClientError;

#[cfg(feature = "transparent-inputs")]
use {
    super::{capture_policy_generation, durable_policy, ensure_policy_generation, resolve_mode},
    crate::{
        AccountRef, AccountUuid,
        wallet::{
            Account, encoding::KeyScope, fully_scanned_height, get_account, get_block_hash,
            transparent::get_legacy_transparent_address,
        },
    },
    std::collections::{BTreeMap, BTreeSet},
    transparent::{
        address::TransparentAddress,
        bundle::OutPoint,
        keys::{IncomingViewingKey as _, NonHardenedChildIndex, TransparentKeyScope},
    },
    zcash_client_backend::data_api::{
        Account as _,
        transparent_ledger::{
            AddressRange, CandidateBlocker, CandidateRecovery, ChainPoint, CommitOutcome,
            CommitRejection, IntegrityFailure, InvalidCommit, MAX_RECOVERY_IDENTIFIER_LEN,
            PageRequest, PendingPage, PublicationAnchor, ReceiveEvent, RecoveryRevision,
            SpendEvent, StaleCommit, TransparentLedgerCommit, TransparentLedgerMode,
            TransparentWatchSet, WatchOrigin, WatchedAddress,
        },
    },
    zcash_keys::{address::Address, keys::transparent::gap_limits::GapLimits},
    zcash_primitives::{block::BlockHash, transaction::TxId},
    zcash_protocol::{consensus, value::Zatoshis},
    zcash_script::script,
};

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

#[cfg(feature = "transparent-inputs")]
fn script_bytes(address: &TransparentAddress) -> Vec<u8> {
    transparent::address::Script::from(address.script()).0.0
}

#[cfg(feature = "transparent-inputs")]
fn address_from_script(bytes: Vec<u8>) -> Result<TransparentAddress, SqliteClientError> {
    script::FromChain::parse(&script::Code(bytes))
        .ok()
        .and_then(|script| TransparentAddress::from_script_from_chain(&script))
        .ok_or_else(|| {
            SqliteClientError::CorruptedData("recovery script is not a standard address".into())
        })
}

#[cfg(feature = "transparent-inputs")]
fn reject(rejection: CommitRejection) -> SqliteClientError {
    SqliteClientError::TransparentLedgerCommitRejected(rejection)
}

/// The derived scopes candidate windows extend, indexed by their `key_scope` code.
#[cfg(feature = "transparent-inputs")]
const WINDOW_SCOPES: [TransparentKeyScope; 3] = [
    TransparentKeyScope::EXTERNAL,
    TransparentKeyScope::INTERNAL,
    TransparentKeyScope::EPHEMERAL,
];

/// One past the highest non-hardened child index.
#[cfg(feature = "transparent-inputs")]
const WINDOW_LIMIT: u32 = 1 << 31;

#[cfg(feature = "transparent-inputs")]
fn scope_slot(scope: TransparentKeyScope) -> Option<usize> {
    WINDOW_SCOPES.iter().position(|s| *s == scope)
}

/// An account's watched addresses, with the window bounds they were built from.
#[cfg(feature = "transparent-inputs")]
struct Watch {
    account: Account,
    required_from: BlockHeight,
    addresses: BTreeMap<TransparentAddress, WatchOrigin>,
    /// One past the highest index in the wallet's own address table, per window scope.
    production_end: [u32; 3],
    /// The stored candidate window end, per window scope.
    candidate_end: [u32; 3],
    /// Whether the account's keys can derive further addresses, per window scope.
    derivable: [bool; 3],
}

#[cfg(feature = "transparent-inputs")]
impl Watch {
    fn load<P: consensus::Parameters>(
        conn: &rusqlite::Connection,
        params: &P,
        account_uuid: AccountUuid,
    ) -> Result<Option<Self>, SqliteClientError> {
        let Some(account) = get_account(conn, params, account_uuid)? else {
            return Ok(None);
        };
        let account_ref = account.internal_id();
        let mut addresses = BTreeMap::new();
        let mut production_end = [0u32; 3];
        let mut note_derived = |addresses: &mut BTreeMap<TransparentAddress, WatchOrigin>,
                                address: TransparentAddress,
                                scope: TransparentKeyScope,
                                index: NonHardenedChildIndex| {
            if let Some(slot) = scope_slot(scope) {
                production_end[slot] = production_end[slot].max(index.index() + 1);
            }
            note_origin(addresses, address, WatchOrigin::Derived { scope, index });
        };

        let mut stmt = conn.prepare_cached(
            "SELECT cached_transparent_receiver_address, key_scope, transparent_child_index
             FROM addresses
             WHERE account_id = :account_id
             AND cached_transparent_receiver_address IS NOT NULL",
        )?;
        let mut rows = stmt.query(named_params![":account_id": account_ref.0])?;
        while let Some(row) = rows.next()? {
            let encoded: String = row.get(0)?;
            let address = Address::decode(params, &encoded)
                .and_then(|a| a.to_transparent_address())
                .ok_or_else(|| {
                    SqliteClientError::CorruptedData(format!(
                        "invalid transparent receiver {encoded}"
                    ))
                })?;
            match KeyScope::decode(row.get(1)?)?.as_transparent() {
                Some(scope) => {
                    let index = row
                        .get::<_, Option<u32>>(2)?
                        .and_then(NonHardenedChildIndex::from_index)
                        .ok_or_else(|| {
                            SqliteClientError::CorruptedData(
                                "derived address without a valid child index".into(),
                            )
                        })?;
                    note_derived(&mut addresses, address, scope, index);
                }
                None => note_origin(&mut addresses, address, WatchOrigin::Standalone),
            }
        }
        // The legacy external address may have no address row.
        if let Some((address, index)) = get_legacy_transparent_address(params, conn, account_uuid)?
        {
            note_derived(
                &mut addresses,
                address,
                TransparentKeyScope::EXTERNAL,
                index,
            );
        }

        let mut candidate_end = [0u32; 3];
        let mut stmt = conn.prepare_cached(
            "SELECT key_scope, end_index FROM tpir_candidate_windows WHERE account_id = :account_id",
        )?;
        let mut rows = stmt.query(named_params![":account_id": account_ref.0])?;
        while let Some(row) = rows.next()? {
            let slot = usize::try_from(row.get::<_, i64>(0)?)
                .ok()
                .filter(|slot| *slot < WINDOW_SCOPES.len())
                .ok_or_else(|| {
                    SqliteClientError::CorruptedData("invalid candidate window scope".into())
                })?;
            candidate_end[slot] = u32::try_from(row.get::<_, i64>(1)?)
                .ok()
                .filter(|end| *end <= WINDOW_LIMIT)
                .ok_or_else(|| {
                    SqliteClientError::CorruptedData("invalid candidate window end".into())
                })?;
        }

        let derivable = WINDOW_SCOPES.map(|scope| derive(&account, scope, None).is_some());
        for (slot, scope) in WINDOW_SCOPES.into_iter().enumerate() {
            for index in production_end[slot]..candidate_end[slot] {
                let index = NonHardenedChildIndex::from_index(index).expect("below WINDOW_LIMIT");
                if let Some(address) = derive(&account, scope, Some(index)) {
                    addresses
                        .entry(address)
                        .or_insert(WatchOrigin::CandidateWindow { scope, index });
                }
            }
        }

        let required_from = account.birthday();
        Ok(Some(Watch {
            account,
            required_from,
            addresses,
            production_end,
            candidate_end,
            derivable,
        }))
    }

    fn watched_addresses(&self) -> Vec<WatchedAddress> {
        self.addresses
            .iter()
            .map(|(address, origin)| WatchedAddress {
                address: *address,
                origin: *origin,
                required_from: self.required_from,
            })
            .collect()
    }

    /// The window end each scope needs for the account's mined candidate activity: one gap
    /// beyond the highest index with activity, where that exceeds the current window.
    fn window_needs(
        &self,
        conn: &rusqlite::Connection,
        gap_limits: &GapLimits,
    ) -> Result<[Option<u32>; 3], SqliteClientError> {
        let mut stmt = conn.prepare_cached(
            "SELECT script FROM tpir_receive_events
             WHERE account_id = :account_id AND mined_height IS NOT NULL
             UNION
             SELECT prevout_script FROM tpir_spend_events
             WHERE account_id = :account_id AND mined_height IS NOT NULL",
        )?;
        let used = stmt
            .query_map(
                named_params![":account_id": self.account.internal_id().0],
                |row| row.get::<_, Vec<u8>>(0),
            )?
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut max_used: [Option<u32>; 3] = [None; 3];
        for (address, origin) in &self.addresses {
            let (WatchOrigin::Derived { scope, index }
            | WatchOrigin::CandidateWindow { scope, index }) = origin
            else {
                continue;
            };
            if let Some(slot) = scope_slot(*scope)
                && used.contains(&script_bytes(address))
            {
                max_used[slot] =
                    Some(max_used[slot].map_or(index.index(), |m| m.max(index.index())));
            }
        }
        let mut needs = [None; 3];
        for (slot, scope) in WINDOW_SCOPES.into_iter().enumerate() {
            let (Some(used), Some(gap)) = (max_used[slot], gap_limits.limit_for(scope)) else {
                continue;
            };
            let needed = used.saturating_add(1).saturating_add(gap).min(WINDOW_LIMIT);
            if needed > self.production_end[slot].max(self.candidate_end[slot]) {
                needs[slot] = Some(needed);
            }
        }
        Ok(needs)
    }
}

/// Records why `address` is watched. A derivation outranks a standalone import of the same
/// receiver, so activity there still extends the derived window.
#[cfg(feature = "transparent-inputs")]
fn note_origin(
    addresses: &mut BTreeMap<TransparentAddress, WatchOrigin>,
    address: TransparentAddress,
    origin: WatchOrigin,
) {
    let entry = addresses.entry(address).or_insert(origin);
    if *entry == WatchOrigin::Standalone {
        *entry = origin;
    }
}

/// Derives the address at `index` in `scope`, or with `index` absent, reports whether the
/// account's keys can derive the scope at all.
#[cfg(feature = "transparent-inputs")]
fn derive(
    account: &Account,
    scope: TransparentKeyScope,
    index: Option<NonHardenedChildIndex>,
) -> Option<TransparentAddress> {
    let probe = index.unwrap_or(NonHardenedChildIndex::ZERO);
    let pubkey = account.ufvk().and_then(|ufvk| ufvk.transparent());
    match scope {
        TransparentKeyScope::EXTERNAL => match pubkey {
            Some(pubkey) => pubkey
                .derive_external_ivk()
                .ok()?
                .derive_address(probe)
                .ok(),
            None => account
                .uivk()
                .transparent()
                .as_ref()?
                .derive_address(probe)
                .ok(),
        },
        TransparentKeyScope::INTERNAL => pubkey?
            .derive_internal_ivk()
            .ok()?
            .derive_address(probe)
            .ok(),
        TransparentKeyScope::EPHEMERAL => pubkey?
            .derive_ephemeral_ivk()
            .ok()?
            .derive_ephemeral_address(probe)
            .ok(),
        _ => None,
    }
}

/// The highest contiguously scanned local block.
#[cfg(feature = "transparent-inputs")]
fn local_target(conn: &rusqlite::Connection) -> Result<Option<ChainPoint>, SqliteClientError> {
    let Some(height) = fully_scanned_height(conn)? else {
        return Ok(None);
    };
    Ok(get_block_hash(conn, height)?.map(|hash| ChainPoint { height, hash }))
}

#[cfg(feature = "transparent-inputs")]
fn is_local_block(
    conn: &rusqlite::Connection,
    point: &ChainPoint,
) -> Result<bool, SqliteClientError> {
    Ok(get_block_hash(conn, point.height)? == Some(point.hash))
}

#[cfg(feature = "transparent-inputs")]
fn height(value: u32) -> BlockHeight {
    BlockHeight::from_u32(value)
}

#[cfg(feature = "transparent-inputs")]
fn read_revision(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> Result<RecoveryRevision, SqliteClientError> {
    Ok(RecoveryRevision {
        source: row.get(offset)?,
        revision: row.get(offset + 1)?,
        lineage: u64::try_from(row.get::<_, i64>(offset + 2)?)
            .map_err(|_| SqliteClientError::CorruptedData("negative revision lineage".into()))?,
        sealed: row.get(offset + 3)?,
        publication: PublicationAnchor {
            height: height(row.get(offset + 4)?),
            hash: BlockHash::try_from_slice(&row.get::<_, Vec<u8>>(offset + 5)?).ok_or_else(
                || SqliteClientError::CorruptedData("invalid publication hash".into()),
            )?,
        },
    })
}

#[cfg(feature = "transparent-inputs")]
fn read_hash(bytes: Vec<u8>) -> Result<BlockHash, SqliteClientError> {
    BlockHash::try_from_slice(&bytes)
        .ok_or_else(|| SqliteClientError::CorruptedData("invalid block hash".into()))
}

#[cfg(feature = "transparent-inputs")]
fn pending_pages(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
) -> Result<Vec<PendingPage>, SqliteClientError> {
    let mut pages = conn.prepare_cached(
        "SELECT p.id, p.page, p.from_height, p.through_height, p.target_height, p.target_hash,
                r.source, r.revision, r.lineage, r.sealed, r.publication_height,
                r.publication_hash
         FROM tpir_pending_pages p
         JOIN tpir_revisions r ON r.id = p.revision_id
         WHERE p.account_id = :account_id
         ORDER BY p.id",
    )?;
    let mut scripts = conn.prepare_cached(
        "SELECT script FROM tpir_pending_page_scripts WHERE page_id = :page_id ORDER BY script",
    )?;
    let mut rows = pages.query(named_params![":account_id": account_ref.0])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        let page_id: i64 = row.get(0)?;
        let addresses = scripts
            .query_map(named_params![":page_id": page_id], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .map(|script| address_from_script(script?))
            .collect::<Result<_, _>>()?;
        result.push(PendingPage {
            request: PageRequest {
                page: row.get(1)?,
                addresses,
                from: height(row.get(2)?),
                through: height(row.get(3)?),
            },
            target: ChainPoint {
                height: height(row.get(4)?),
                hash: read_hash(row.get(5)?)?,
            },
            revision: read_revision(row, 6)?,
        });
    }
    Ok(result)
}

/// Returns `account`'s watched addresses, capture context, and pending pages. The caller
/// provides the read snapshot.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn watch_set<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    configured: Option<TransparentLedgerMode>,
    account: AccountUuid,
) -> Result<TransparentWatchSet<AccountUuid>, SqliteClientError> {
    resolve_mode(conn, configured)?;
    let watch = Watch::load(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
    Ok(TransparentWatchSet {
        account,
        policy_generation: capture_policy_generation(conn)?,
        target: local_target(conn)?,
        addresses: watch.watched_addresses(),
        pending_pages: pending_pages(conn, watch.account.internal_id())?,
    })
}

/// Runs `f` atomically: in a new immediate transaction, or under a savepoint inside the
/// caller's transaction. Nothing `f` wrote survives its failure, including a failure to
/// commit or release.
#[cfg(feature = "transparent-inputs")]
fn atomically<T>(
    conn: &rusqlite::Connection,
    f: impl FnOnce(&rusqlite::Connection) -> Result<T, SqliteClientError>,
) -> Result<T, SqliteClientError> {
    if conn.is_autocommit() {
        // The guard rolls back when dropped, including after a failed commit.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        return Ok(value);
    }
    conn.execute_batch("SAVEPOINT tpir_commit")?;
    let result = f(conn).and_then(|value| {
        conn.execute_batch("RELEASE tpir_commit")?;
        Ok(value)
    });
    if result.is_err() {
        // Undo everything since the savepoint and remove it, even when releasing it failed. If
        // this cleanup fails too, the error still aborts the caller's enclosing transaction.
        let _ = conn.execute_batch("ROLLBACK TO tpir_commit; RELEASE tpir_commit");
    }
    result
}

/// Checks everything about a commit that needs no stored state.
#[cfg(feature = "transparent-inputs")]
fn check_well_formed(commit: &TransparentLedgerCommit<AccountUuid>) -> Result<(), InvalidCommit> {
    let identifier_ok = |id: &[u8]| !id.is_empty() && id.len() <= MAX_RECOVERY_IDENTIFIER_LEN;
    if !identifier_ok(&commit.revision.source) || !identifier_ok(&commit.revision.revision) {
        return Err(InvalidCommit::Identifier);
    }
    if i64::try_from(commit.revision.lineage).is_err() {
        return Err(InvalidCommit::Lineage);
    }
    let anchor = commit.anchor.height;
    if anchor > commit.context.target.height {
        return Err(InvalidCommit::AnchorAboveTarget);
    }
    // The publication anchor is not chain evidence, but it bounds what the revision indexed.
    let publication = &commit.revision.publication;
    if anchor > publication.height
        || (anchor == publication.height && commit.anchor.hash != publication.hash)
    {
        return Err(InvalidCommit::AnchorOutsidePublication);
    }
    let check_range = |from: BlockHeight, through: BlockHeight| {
        if from > through {
            Err(InvalidCommit::EmptyRange)
        } else if through > anchor {
            Err(InvalidCommit::AboveAnchor)
        } else {
            Ok(())
        }
    };
    for range in commit.coverage.iter().chain(&commit.unsupported) {
        check_range(range.from, range.through)?;
    }
    let mut opened = BTreeSet::new();
    for page in &commit.opened_pages {
        check_range(page.from, page.through)?;
        if !identifier_ok(&page.page) {
            return Err(InvalidCommit::Identifier);
        }
        if page.addresses.is_empty() || !opened.insert(&page.page) {
            return Err(InvalidCommit::Page);
        }
    }
    if commit
        .completed_pages
        .iter()
        .any(|page| !identifier_ok(page))
    {
        return Err(InvalidCommit::Identifier);
    }
    let mined = commit.receives.iter().map(|r| r.mined_height);
    if mined
        .chain(commit.spends.iter().map(|s| s.mined_height))
        .any(|h| h > anchor)
    {
        return Err(InvalidCommit::AboveAnchor);
    }
    Ok(())
}

/// Returns the stored id of `revision`, recording it if new. Accepting a newer lineage
/// supersedes the source's older provisional revisions, removing their coverage, pages, and
/// observations. Events with no remaining observations are removed; an independent source's
/// observation or a sealed revision keeps an event alive.
#[cfg(feature = "transparent-inputs")]
fn accept_revision(
    conn: &rusqlite::Connection,
    revision: &RecoveryRevision,
) -> Result<i64, SqliteClientError> {
    let lineage = i64::try_from(revision.lineage).expect("checked by check_well_formed");
    let accepted: Option<i64> = conn.query_row(
        "SELECT MAX(lineage) FROM tpir_revisions WHERE source = :source",
        named_params![":source": revision.source],
        |row| row.get(0),
    )?;
    let superseded = !revision.sealed && accepted.is_some_and(|accepted| lineage < accepted);
    let existing = conn
        .query_row(
            "SELECT id, source, revision, lineage, sealed, publication_height, publication_hash
             FROM tpir_revisions
             WHERE source = :source AND (revision = :revision OR lineage = :lineage)",
            named_params![
                ":source": revision.source,
                ":revision": revision.revision,
                ":lineage": lineage,
            ],
            |row| Ok((row.get::<_, i64>(0)?, read_revision(row, 1))),
        )
        .optional()?;
    if let Some((id, stored)) = existing {
        if stored? != *revision {
            return Err(reject(CommitRejection::Integrity(
                IntegrityFailure::RevisionMismatch,
            )));
        }
        if superseded {
            return Err(reject(CommitRejection::Stale(
                StaleCommit::SupersededRevision,
            )));
        }
        return Ok(id);
    }
    if superseded {
        return Err(reject(CommitRejection::Stale(
            StaleCommit::SupersededRevision,
        )));
    }
    let id = conn.query_row(
        "INSERT INTO tpir_revisions (
             source, revision, lineage, sealed, publication_height, publication_hash
         )
         VALUES (:source, :revision, :lineage, :sealed, :publication_height, :publication_hash)
         RETURNING id",
        named_params![
            ":source": revision.source,
            ":revision": revision.revision,
            ":lineage": lineage,
            ":sealed": revision.sealed,
            ":publication_height": u32::from(revision.publication.height),
            ":publication_hash": revision.publication.hash.0.to_vec(),
        ],
        |row| row.get(0),
    )?;
    if accepted.is_none_or(|accepted| lineage > accepted) {
        let older_provisional = "SELECT id FROM tpir_revisions
             WHERE source = :source AND sealed = 0 AND lineage < :lineage";
        for table in [
            "tpir_coverage",
            "tpir_pending_pages",
            "tpir_receive_observations",
            "tpir_spend_observations",
        ] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE revision_id IN ({older_provisional})"),
                named_params![":source": revision.source, ":lineage": lineage],
            )?;
        }
        conn.execute(
            "DELETE FROM tpir_receive_events
             WHERE NOT EXISTS (
                 SELECT 1 FROM tpir_receive_observations o WHERE o.receive_id = tpir_receive_events.id
             )",
            [],
        )?;
        conn.execute(
            "DELETE FROM tpir_spend_events
             WHERE NOT EXISTS (
                 SELECT 1 FROM tpir_spend_observations o WHERE o.spend_id = tpir_spend_events.id
             )",
            [],
        )?;
    }
    Ok(id)
}

/// Refuses an event that disagrees with another event of the same transaction, across every
/// account it touches. A transaction is mined in one block. Being coinbase is a property of the
/// transaction, and a coinbase transaction spends no outputs. `coinbase` is the event's
/// classification for a receive, and `None` for a spend by the transaction.
#[cfg(feature = "transparent-inputs")]
fn check_transaction(
    conn: &rusqlite::Connection,
    txid: &[u8; 32],
    mined: u32,
    coinbase: Option<bool>,
) -> Result<(), SqliteClientError> {
    let coinbase_conflict: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_receive_events
             WHERE txid = :txid
             AND (CASE WHEN :coinbase IS NULL THEN coinbase = 1 ELSE coinbase != :coinbase END)
         ) OR (
             :coinbase IS 1 AND EXISTS (SELECT 1 FROM tpir_spend_events WHERE spending_txid = :txid)
         )",
        named_params![":txid": &txid[..], ":coinbase": coinbase],
        |row| row.get(0),
    )?;
    if coinbase_conflict {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::TransactionCoinbase(TxId::from_bytes(*txid)),
        )));
    }
    let conflicting: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_receive_events WHERE txid = :txid AND mined_height != :mined
         ) OR EXISTS (
             SELECT 1 FROM tpir_spend_events
             WHERE spending_txid = :txid AND mined_height != :mined
         )",
        named_params![":txid": &txid[..], ":mined": mined],
        |row| row.get(0),
    )?;
    if conflicting {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::TransactionPlacement(TxId::from_bytes(*txid)),
        )));
    }
    Ok(())
}

#[cfg(feature = "transparent-inputs")]
fn apply_receive(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
    revision_id: i64,
    receive: &ReceiveEvent,
) -> Result<(), SqliteClientError> {
    let outpoint = &receive.outpoint;
    let script = script_bytes(&receive.address);
    let value = i64::try_from(u64::from(receive.value)).expect("Zatoshis fit in i64");
    let mined = u32::from(receive.mined_height);
    let contradiction = || {
        reject(CommitRejection::Integrity(
            IntegrityFailure::ReceiveContent(outpoint.clone()),
        ))
    };

    let stored = conn
        .query_row(
            "SELECT id, account_id, script, value_zat, coinbase, mined_height
             FROM tpir_receive_events
             WHERE txid = :txid AND output_index = :output_index",
            named_params![":txid": &outpoint.hash()[..], ":output_index": outpoint.n()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                ))
            },
        )
        .optional()?;
    let id = match stored {
        None => conn.query_row(
            "INSERT INTO tpir_receive_events (
                 account_id, txid, output_index, script, value_zat, coinbase, mined_height
             )
             VALUES (:account_id, :txid, :output_index, :script, :value, :coinbase, :mined)
             RETURNING id",
            named_params![
                ":account_id": account_ref.0,
                ":txid": &outpoint.hash()[..],
                ":output_index": outpoint.n(),
                ":script": script,
                ":value": value,
                ":coinbase": receive.coinbase,
                ":mined": mined,
            ],
            |row| row.get::<_, i64>(0),
        )?,
        Some((id, stored_account, stored_script, stored_value, stored_coinbase, placement)) => {
            if (
                stored_account,
                &stored_script,
                stored_value,
                stored_coinbase,
            ) != (account_ref.0, &script, value, receive.coinbase)
            {
                return Err(contradiction());
            }
            match placement {
                Some(h) if h != mined => {
                    return Err(reject(CommitRejection::Integrity(
                        IntegrityFailure::ReceivePlacement(outpoint.clone()),
                    )));
                }
                Some(_) => {}
                None => {
                    conn.execute(
                        "UPDATE tpir_receive_events SET mined_height = :mined WHERE id = :id",
                        named_params![":mined": mined, ":id": id],
                    )?;
                }
            }
            id
        }
    };
    check_transaction(conn, outpoint.hash(), mined, Some(receive.coinbase))?;
    let spend_names_other_script: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_spend_events
             WHERE prevout_txid = :txid AND prevout_output_index = :output_index
             AND prevout_script != :script
         )",
        named_params![
            ":txid": &outpoint.hash()[..],
            ":output_index": outpoint.n(),
            ":script": script,
        ],
        |row| row.get(0),
    )?;
    if spend_names_other_script {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::SpendAddress(outpoint.clone()),
        )));
    }
    let spent_earlier: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_spend_events
             WHERE prevout_txid = :txid AND prevout_output_index = :output_index
             AND mined_height < :mined
         )",
        named_params![
            ":txid": &outpoint.hash()[..],
            ":output_index": outpoint.n(),
            ":mined": mined,
        ],
        |row| row.get(0),
    )?;
    if spent_earlier {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::SpendBeforeOutput(outpoint.clone()),
        )));
    }
    conn.execute(
        "INSERT INTO tpir_receive_observations (receive_id, revision_id)
         VALUES (:id, :revision_id)
         ON CONFLICT DO NOTHING",
        named_params![":id": id, ":revision_id": revision_id],
    )?;
    Ok(())
}

#[cfg(feature = "transparent-inputs")]
fn apply_spend(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
    revision_id: i64,
    spend: &SpendEvent,
) -> Result<(), SqliteClientError> {
    let prevout = &spend.prevout;
    let script = script_bytes(&spend.prevout_address);
    let mined = u32::from(spend.mined_height);
    let txid = spend.spending_txid.as_ref().to_vec();
    let identity = || (spend.spending_txid, spend.input_index);

    let stored = conn
        .query_row(
            "SELECT id, account_id, prevout_txid, prevout_output_index, prevout_script,
                    mined_height
             FROM tpir_spend_events
             WHERE spending_txid = :txid AND input_index = :input_index",
            named_params![":txid": txid, ":input_index": spend.input_index],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                ))
            },
        )
        .optional()?;
    let id = match stored {
        None => conn.query_row(
            "INSERT INTO tpir_spend_events (
                 account_id, spending_txid, input_index, prevout_txid, prevout_output_index,
                 prevout_script, mined_height
             )
             VALUES (
                 :account_id, :txid, :input_index, :prevout_txid, :prevout_output_index,
                 :script, :mined
             )
             RETURNING id",
            named_params![
                ":account_id": account_ref.0,
                ":txid": txid,
                ":input_index": spend.input_index,
                ":prevout_txid": &prevout.hash()[..],
                ":prevout_output_index": prevout.n(),
                ":script": script,
                ":mined": mined,
            ],
            |row| row.get::<_, i64>(0),
        )?,
        Some((id, stored_account, stored_txid, stored_index, stored_script, placement)) => {
            if (
                stored_account,
                &stored_txid[..],
                stored_index,
                &stored_script,
            ) != (account_ref.0, &prevout.hash()[..], prevout.n(), &script)
            {
                let (spending_txid, input_index) = identity();
                return Err(reject(CommitRejection::Integrity(
                    IntegrityFailure::SpendContent {
                        spending_txid,
                        input_index,
                    },
                )));
            }
            match placement {
                Some(h) if h != mined => {
                    let (spending_txid, input_index) = identity();
                    return Err(reject(CommitRejection::Integrity(
                        IntegrityFailure::SpendPlacement {
                            spending_txid,
                            input_index,
                        },
                    )));
                }
                Some(_) => {}
                None => {
                    conn.execute(
                        "UPDATE tpir_spend_events SET mined_height = :mined WHERE id = :id",
                        named_params![":mined": mined, ":id": id],
                    )?;
                }
            }
            id
        }
    };
    check_transaction(conn, spend.spending_txid.as_ref(), mined, None)?;
    let receive_names_other_script: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_receive_events
             WHERE txid = :txid AND output_index = :output_index AND script != :script
         )",
        named_params![
            ":txid": &prevout.hash()[..],
            ":output_index": prevout.n(),
            ":script": script,
        ],
        |row| row.get(0),
    )?;
    if receive_names_other_script {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::SpendAddress(prevout.clone()),
        )));
    }
    let output_later: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_receive_events
             WHERE txid = :txid AND output_index = :output_index AND mined_height > :mined
         )",
        named_params![
            ":txid": &prevout.hash()[..],
            ":output_index": prevout.n(),
            ":mined": mined,
        ],
        |row| row.get(0),
    )?;
    if output_later {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::SpendBeforeOutput(prevout.clone()),
        )));
    }
    let conflicting: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_spend_events
             WHERE prevout_txid = :prevout_txid AND prevout_output_index = :prevout_output_index
             AND mined_height IS NOT NULL
             AND id != :id
         )",
        named_params![
            ":prevout_txid": &prevout.hash()[..],
            ":prevout_output_index": prevout.n(),
            ":id": id,
        ],
        |row| row.get(0),
    )?;
    if conflicting {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::ConflictingSpends(prevout.clone()),
        )));
    }
    conn.execute(
        "INSERT INTO tpir_spend_observations (spend_id, revision_id)
         VALUES (:id, :revision_id)
         ON CONFLICT DO NOTHING",
        named_params![":id": id, ":revision_id": revision_id],
    )?;
    Ok(())
}

#[cfg(feature = "transparent-inputs")]
fn open_page(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
    revision_id: i64,
    target: &ChainPoint,
    page: &PageRequest,
) -> Result<(), SqliteClientError> {
    let stored = conn
        .query_row(
            "SELECT id, from_height, through_height FROM tpir_pending_pages
             WHERE account_id = :account_id AND revision_id = :revision_id AND page = :page",
            named_params![
                ":account_id": account_ref.0,
                ":revision_id": revision_id,
                ":page": page.page,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            },
        )
        .optional()?;
    let scripts: BTreeSet<Vec<u8>> = page.addresses.iter().map(script_bytes).collect();
    if let Some((id, from, through)) = stored {
        // Replaying an opening is harmless; reopening it with another range or address set is
        // not, since merging would describe work no request asked for.
        let stored_scripts = conn
            .prepare_cached("SELECT script FROM tpir_pending_page_scripts WHERE page_id = :id")?
            .query_map(named_params![":id": id], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if (from, through) != (u32::from(page.from), u32::from(page.through))
            || stored_scripts != scripts
        {
            return Err(reject(CommitRejection::Invalid(InvalidCommit::Page)));
        }
        return Ok(());
    }
    for address in &page.addresses {
        let covered: bool = conn.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM tpir_coverage
                 WHERE account_id = :account_id AND revision_id = :revision_id
                 AND script = :script AND supported = 1
                 AND from_height <= :through AND through_height >= :from
             )",
            named_params![
                ":account_id": account_ref.0,
                ":revision_id": revision_id,
                ":script": script_bytes(address),
                ":from": u32::from(page.from),
                ":through": u32::from(page.through),
            ],
            |row| row.get(0),
        )?;
        if covered {
            return Err(reject(CommitRejection::Invalid(
                InvalidCommit::PendingPageOverlap(*address),
            )));
        }
    }
    let page_id: i64 = conn.query_row(
        "INSERT INTO tpir_pending_pages (
             account_id, revision_id, page, from_height, through_height,
             target_height, target_hash
         )
         VALUES (
             :account_id, :revision_id, :page, :from_height, :through_height,
             :target_height, :target_hash
         )
         RETURNING id",
        named_params![
            ":account_id": account_ref.0,
            ":revision_id": revision_id,
            ":page": page.page,
            ":from_height": u32::from(page.from),
            ":through_height": u32::from(page.through),
            ":target_height": u32::from(target.height),
            ":target_hash": target.hash.0.to_vec(),
        ],
        |row| row.get(0),
    )?;
    for script in scripts {
        conn.execute(
            "INSERT INTO tpir_pending_page_scripts (page_id, script)
             VALUES (:page_id, :script)",
            named_params![":page_id": page_id, ":script": script],
        )?;
    }
    Ok(())
}

#[cfg(feature = "transparent-inputs")]
fn record_range(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
    revision_id: i64,
    anchor: &ChainPoint,
    range: &AddressRange,
    supported: bool,
) -> Result<(), SqliteClientError> {
    let script = script_bytes(&range.address);
    let (from, through) = (u32::from(range.from), u32::from(range.through));
    let contradicted: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_coverage
             WHERE account_id = :account_id AND revision_id = :revision_id AND script = :script
             AND supported != :supported
             AND from_height <= :through AND through_height >= :from
         )",
        named_params![
            ":account_id": account_ref.0,
            ":revision_id": revision_id,
            ":script": script,
            ":supported": supported,
            ":from": from,
            ":through": through,
        ],
        |row| row.get(0),
    )?;
    if contradicted {
        return Err(reject(CommitRejection::Invalid(
            InvalidCommit::SupportContradiction(range.address),
        )));
    }
    if supported {
        let blocked: bool = conn.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM tpir_pending_pages p
                 JOIN tpir_pending_page_scripts s ON s.page_id = p.id
                 WHERE p.account_id = :account_id AND p.revision_id = :revision_id
                 AND s.script = :script
                 AND p.from_height <= :through AND p.through_height >= :from
             )",
            named_params![
                ":account_id": account_ref.0,
                ":revision_id": revision_id,
                ":script": script,
                ":from": from,
                ":through": through,
            ],
            |row| row.get(0),
        )?;
        if blocked {
            return Err(reject(CommitRejection::Invalid(
                InvalidCommit::PendingPageOverlap(range.address),
            )));
        }
    }
    conn.execute(
        "INSERT INTO tpir_coverage (
             account_id, script, from_height, through_height, anchor_height, anchor_hash,
             revision_id, supported
         )
         SELECT :account_id, :script, :from, :through, :anchor_height, :anchor_hash,
                :revision_id, :supported
         WHERE NOT EXISTS (
             SELECT 1 FROM tpir_coverage
             WHERE account_id = :account_id AND script = :script
             AND from_height = :from AND through_height = :through
             AND revision_id = :revision_id AND supported = :supported
         )",
        named_params![
            ":account_id": account_ref.0,
            ":script": script,
            ":from": from,
            ":through": through,
            ":anchor_height": u32::from(anchor.height),
            ":anchor_hash": anchor.hash.0.to_vec(),
            ":revision_id": revision_id,
            ":supported": supported,
        ],
    )?;
    Ok(())
}

/// Validates and applies `commit` to the candidate ledger, atomically.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn apply_commit<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    configured: Option<TransparentLedgerMode>,
    commit: TransparentLedgerCommit<AccountUuid>,
) -> Result<CommitOutcome, SqliteClientError> {
    check_well_formed(&commit).map_err(|e| reject(CommitRejection::Invalid(e)))?;
    atomically(conn, |conn| {
        // Private recovery must be authorized both by this handle and durably.
        let handle = resolve_mode(conn, configured)?;
        let durable = durable_policy(conn)?.map(|policy| policy.mode);
        if handle == TransparentLedgerMode::Public
            || durable.is_none_or(|mode| mode == TransparentLedgerMode::Public)
        {
            return Err(SqliteClientError::TransparentRecoveryNotEnabled);
        }
        ensure_policy_generation(conn, commit.context.policy_generation)?;

        let stale = |reason| reject(CommitRejection::Stale(reason));
        let watch = Watch::load(conn, params, commit.context.account)?
            .ok_or_else(|| stale(StaleCommit::AccountUnknown))?;
        let account_ref = watch.account.internal_id();

        let target = commit.context.target;
        if fully_scanned_height(conn)?.is_none_or(|scanned| target.height > scanned)
            || !is_local_block(conn, &target)?
        {
            return Err(stale(StaleCommit::TargetNotAccepted));
        }
        if !is_local_block(conn, &commit.anchor)? {
            return Err(stale(StaleCommit::AnchorNotAccepted));
        }

        let named = commit
            .receives
            .iter()
            .map(|r| r.address)
            .chain(commit.spends.iter().map(|s| s.prevout_address))
            .chain(commit.coverage.iter().map(|r| r.address))
            .chain(commit.unsupported.iter().map(|r| r.address))
            .chain(
                commit
                    .opened_pages
                    .iter()
                    .flat_map(|p| p.addresses.iter().copied()),
            );
        for address in named {
            if !watch.addresses.contains_key(&address) {
                return Err(stale(StaleCommit::AddressNotWatched(address)));
            }
        }

        let revision_id = accept_revision(conn, &commit.revision)?;

        for page in &commit.completed_pages {
            let removed = conn.execute(
                "DELETE FROM tpir_pending_pages
                 WHERE account_id = :account_id AND revision_id = :revision_id AND page = :page",
                named_params![
                    ":account_id": account_ref.0,
                    ":revision_id": revision_id,
                    ":page": page,
                ],
            )?;
            if removed == 0 {
                return Err(stale(StaleCommit::UnknownPage(page.clone())));
            }
        }
        for page in &commit.opened_pages {
            open_page(conn, account_ref, revision_id, &target, page)?;
        }

        for receive in &commit.receives {
            apply_receive(conn, account_ref, revision_id, receive)?;
        }
        for spend in &commit.spends {
            apply_spend(conn, account_ref, revision_id, spend)?;
        }

        for range in &commit.coverage {
            record_range(conn, account_ref, revision_id, &commit.anchor, range, true)?;
        }
        for range in &commit.unsupported {
            record_range(conn, account_ref, revision_id, &commit.anchor, range, false)?;
        }

        let mut window_grew = false;
        for (slot, needed) in watch
            .window_needs(conn, gap_limits)?
            .into_iter()
            .enumerate()
        {
            if let Some(needed) = needed.filter(|_| watch.derivable[slot]) {
                conn.execute(
                    "INSERT INTO tpir_candidate_windows (account_id, key_scope, end_index)
                     VALUES (:account_id, :key_scope, :end_index)
                     ON CONFLICT (account_id, key_scope)
                     DO UPDATE SET end_index = MAX(end_index, excluded.end_index)",
                    named_params![
                        ":account_id": account_ref.0,
                        ":key_scope": KeyScope::try_from(WINDOW_SCOPES[slot])?.encode(),
                        ":end_index": needed,
                    ],
                )?;
                window_grew = true;
            }
        }

        // Builds without the recovery lifecycle would leave this state stale across rewinds;
        // once any exists, they must fail closed.
        conn.execute(
            "UPDATE tpir_meta
             SET min_reader_version = MAX(min_reader_version, :version)
             WHERE id = 0",
            named_params![":version": super::TPIR_READER_VERSION],
        )?;

        Ok(CommitOutcome { window_grew })
    })
}

/// Merges inclusive `(from, through)` ranges into disjoint, non-adjacent ranges.
#[cfg(feature = "transparent-inputs")]
fn merge(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = vec![];
    for (from, through) in ranges {
        match merged.last_mut() {
            Some(last) if from <= last.1.saturating_add(1) => last.1 = last.1.max(through),
            _ => merged.push((from, through)),
        }
    }
    merged
}

/// Returns `account`'s candidate diagnostics. The caller provides the read snapshot.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn candidate_recovery<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    configured: Option<TransparentLedgerMode>,
    account: AccountUuid,
) -> Result<CandidateRecovery<AccountUuid>, SqliteClientError> {
    resolve_mode(conn, configured)?;
    let watch = Watch::load(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
    let account_ref = watch.account.internal_id();
    let target = local_target(conn)?;

    let mut supported: BTreeMap<Vec<u8>, Vec<(u32, u32)>> = BTreeMap::new();
    let mut unsupported: Vec<(Vec<u8>, u32, u32)> = vec![];
    let mut stmt = conn.prepare_cached(
        "SELECT script, from_height, through_height, supported
         FROM tpir_coverage WHERE account_id = :account_id",
    )?;
    let mut rows = stmt.query(named_params![":account_id": account_ref.0])?;
    while let Some(row) = rows.next()? {
        let (script, from, through): (Vec<u8>, u32, u32) = (row.get(0)?, row.get(1)?, row.get(2)?);
        if row.get::<_, bool>(3)? {
            supported.entry(script).or_default().push((from, through));
        } else {
            unsupported.push((script, from, through));
        }
    }
    let supported: BTreeMap<_, _> = supported
        .into_iter()
        .map(|(script, ranges)| (script, merge(ranges)))
        .collect();

    // Continuous coverage from the required start, per watched address.
    let required = u32::from(watch.required_from);
    let mut covered_through: Option<u32> = target.map(|t| u32::from(t.height));
    // A target below the required start needs no coverage yet: the interval is empty.
    let needs_coverage = target.is_some_and(|t| u32::from(t.height) >= required);
    for address in watch.addresses.keys().filter(|_| needs_coverage) {
        let reach = supported
            .get(&script_bytes(address))
            .and_then(|ranges| {
                ranges
                    .iter()
                    .find(|(from, through)| *from <= required && required <= *through)
            })
            .map(|(_, through)| *through);
        covered_through = match (covered_through, reach) {
            (Some(c), Some(r)) => Some(c.min(r)),
            _ => None,
        };
    }
    // Only the part of an unsupported range the account must cover blocks it: from the
    // required start through the target. The row is kept, so lowering the birthday brings
    // earlier parts back into scope.
    let unsupported_uncovered = target.is_some_and(|target| {
        let target = u32::from(target.height);
        unsupported.iter().any(|(script, from, through)| {
            let (from, through) = ((*from).max(required), (*through).min(target));
            from <= through
                && !supported
                    .get(script)
                    .is_some_and(|ranges| ranges.iter().any(|(f, t)| *f <= from && through <= *t))
        })
    });

    let pending_pages: usize = conn.query_row(
        "SELECT COUNT(*) FROM tpir_pending_pages WHERE account_id = :account_id",
        named_params![":account_id": account_ref.0],
        |row| row.get::<_, i64>(0),
    )? as usize;

    let mut stmt = conn.prepare_cached(
        "SELECT txid, output_index, script, value_zat, coinbase, mined_height
         FROM tpir_receive_events
         WHERE account_id = :account_id AND mined_height IS NOT NULL
         ORDER BY txid, output_index",
    )?;
    let receives = stmt
        .query_and_then(named_params![":account_id": account_ref.0], |row| {
            Ok::<_, SqliteClientError>(ReceiveEvent {
                outpoint: OutPoint::new(row.get(0)?, row.get(1)?),
                address: address_from_script(row.get(2)?)?,
                value: Zatoshis::from_nonnegative_i64(row.get(3)?).map_err(|_| {
                    SqliteClientError::CorruptedData("invalid receive value".into())
                })?,
                coinbase: row.get(4)?,
                mined_height: height(row.get(5)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare_cached(
        "SELECT spending_txid, input_index, prevout_txid, prevout_output_index, prevout_script,
                mined_height
         FROM tpir_spend_events
         WHERE account_id = :account_id AND mined_height IS NOT NULL
         ORDER BY spending_txid, input_index",
    )?;
    let spends = stmt
        .query_and_then(named_params![":account_id": account_ref.0], |row| {
            Ok::<_, SqliteClientError>(SpendEvent {
                spending_txid: TxId::from_bytes(row.get(0)?),
                input_index: row.get(1)?,
                prevout: OutPoint::new(row.get(2)?, row.get(3)?),
                prevout_address: address_from_script(row.get(4)?)?,
                mined_height: height(row.get(5)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let received: BTreeSet<_> = receives
        .iter()
        .map(|r| (*r.outpoint.hash(), r.outpoint.n()))
        .collect();
    let spent: BTreeSet<_> = spends
        .iter()
        .map(|s| (*s.prevout.hash(), s.prevout.n()))
        .collect();
    let unresolved_spends = spent.difference(&received).count();
    let unspent_receives: Vec<&ReceiveEvent> = receives
        .iter()
        .filter(|r| !spent.contains(&(*r.outpoint.hash(), r.outpoint.n())))
        .collect();
    // Partially recovered receives can legitimately exceed any real balance, so a sum above
    // `MAX_MONEY` is reported as unrepresentable rather than as an error.
    let recovered_unverified = unspent_receives
        .iter()
        .try_fold(Zatoshis::ZERO, |sum, r| sum + r.value);

    let window_underivable = watch
        .window_needs(conn, gap_limits)?
        .iter()
        .zip(watch.derivable)
        .any(|(needed, derivable)| needed.is_some() && !derivable);

    let mut blockers = vec![];
    match target {
        None => blockers.push(CandidateBlocker::ChainUnknown),
        Some(target) => {
            if covered_through.is_none_or(|c| c < u32::from(target.height)) {
                blockers.push(CandidateBlocker::IncompleteCoverage);
            }
        }
    }
    if pending_pages > 0 {
        blockers.push(CandidateBlocker::PendingPages);
    }
    if unresolved_spends > 0 {
        blockers.push(CandidateBlocker::UnresolvedSpends);
    }
    if unsupported_uncovered {
        blockers.push(CandidateBlocker::UnsupportedRanges);
    }
    if window_underivable {
        blockers.push(CandidateBlocker::WindowUnderivable);
    }

    Ok(CandidateRecovery {
        account,
        target,
        covered_through: covered_through.map(height),
        blockers,
        watched_addresses: watch.addresses.len(),
        pending_pages,
        unresolved_spends,
        unspent: unspent_receives
            .iter()
            .map(|r| r.outpoint.clone())
            .collect(),
        recovered_unverified,
        receives,
        spends,
    })
}

#[cfg(all(test, feature = "transparent-inputs"))]
mod tests {
    use std::collections::BTreeMap;

    use transparent::{
        address::TransparentAddress,
        keys::{NonHardenedChildIndex, TransparentKeyScope},
    };
    use zcash_client_backend::data_api::transparent_ledger::WatchOrigin;

    use super::{atomically, note_origin};
    use crate::error::SqliteClientError;

    #[test]
    fn derivation_outranks_a_standalone_import() {
        let address = TransparentAddress::PublicKeyHash([1; 20]);
        let derived = WatchOrigin::Derived {
            scope: TransparentKeyScope::EXTERNAL,
            index: NonHardenedChildIndex::ZERO,
        };
        for order in [
            [WatchOrigin::Standalone, derived],
            [derived, WatchOrigin::Standalone],
        ] {
            let mut addresses = BTreeMap::new();
            for origin in order {
                note_origin(&mut addresses, address, origin);
            }
            assert_eq!(addresses[&address], derived);
        }
    }

    #[test]
    fn failed_commit_rolls_back() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // A deferred foreign key violation makes COMMIT itself fail.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE written (x INTEGER);
             CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (
                 parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED
             );",
        )
        .unwrap();
        let result = atomically(&conn, |conn| {
            conn.execute_batch("INSERT INTO written VALUES (1); INSERT INTO child VALUES (42);")?;
            Ok(())
        });
        assert!(matches!(result, Err(SqliteClientError::DbError(_))));
        assert!(
            conn.is_autocommit(),
            "the failed transaction must not stay open"
        );
        let written: i64 = conn
            .query_row("SELECT COUNT(*) FROM written", [], |row| row.get(0))
            .unwrap();
        assert_eq!(written, 0);
    }
}
