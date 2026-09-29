use super::*;
use rusqlite::OptionalExtension as _;

/// Runs `f` atomically: in a new immediate transaction, or under a savepoint inside the
/// caller's transaction. Nothing `f` wrote survives its failure, including a failure to
/// commit or release.
#[cfg(feature = "transparent-inputs")]
pub(super) fn atomically<T>(
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

/// Validates and applies `commit` to the candidate ledger, atomically.
///
/// An integrity failure applies none of the commit's facts but quarantines, in the same
/// transaction, the commit's source, its account, and every account holding evidence from the
/// source, and removes those accounts' pending pages.
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
        if source_quarantined(conn, &commit.revision.source)? {
            return Err(reject(CommitRejection::Refused(
                RefusedCommit::SourceQuarantined,
            )));
        }
        if account_quarantined(conn, account_ref)? {
            return Err(reject(CommitRejection::Refused(
                RefusedCommit::AccountQuarantined,
            )));
        }

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

        // The facts apply under a nested savepoint, so an integrity failure can discard them
        // all while its quarantine commits.
        match atomically(conn, |conn| apply_facts(conn, gap_limits, &watch, &commit)) {
            Err(SqliteClientError::TransparentLedgerCommitRejected(
                rejection @ CommitRejection::Integrity(_),
            )) => {
                quarantine(conn, &commit.revision.source, account_ref)?;
                Ok(Err(reject(rejection)))
            }
            result => result.map(Ok),
        }
    })?
}

/// Applies a validated commit's revision, pages, events, ranges, and window growth.
#[cfg(feature = "transparent-inputs")]
fn apply_facts(
    conn: &rusqlite::Connection,
    gap_limits: &GapLimits,
    watch: &Watch,
    commit: &TransparentLedgerCommit<AccountUuid>,
) -> Result<CommitOutcome, SqliteClientError> {
    let stale = |reason| reject(CommitRejection::Stale(reason));
    let account_ref = watch.account.internal_id();
    let target = commit.context.target;

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
    super::super::require_reader_version(conn, super::super::RECOVERY_READER_VERSION)?;

    Ok(CommitOutcome { window_grew })
}

/// Whether `source` is quarantined.
#[cfg(feature = "transparent-inputs")]
pub(super) fn source_quarantined(
    conn: &rusqlite::Connection,
    source: &[u8],
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM tpir_quarantined_sources WHERE source = :source)",
        named_params![":source": source],
        |row| row.get(0),
    )?)
}

/// Whether `account_ref` is quarantined.
#[cfg(feature = "transparent-inputs")]
pub(super) fn account_quarantined(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
) -> Result<bool, SqliteClientError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tpir_quarantined_accounts WHERE account_id = :account_id
         )",
        named_params![":account_id": account_ref.0],
        |row| row.get(0),
    )?)
}

/// Quarantines `source`, `account_ref`, and every account holding coverage, pages, or observed
/// events from `source`, and removes the quarantined accounts' pending pages.
#[cfg(feature = "transparent-inputs")]
fn quarantine(
    conn: &rusqlite::Connection,
    source: &[u8],
    account_ref: AccountRef,
) -> Result<(), SqliteClientError> {
    conn.execute(
        "INSERT OR IGNORE INTO tpir_quarantined_sources (source) VALUES (:source)",
        named_params![":source": source],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO tpir_quarantined_accounts (account_id)
         SELECT :account_id
         UNION SELECT account_id FROM tpir_coverage
             WHERE revision_id IN (SELECT id FROM tpir_revisions WHERE source = :source)
         UNION SELECT account_id FROM tpir_pending_pages
             WHERE revision_id IN (SELECT id FROM tpir_revisions WHERE source = :source)
         UNION SELECT e.account_id FROM tpir_receive_events e
             JOIN tpir_receive_observations o ON o.receive_id = e.id
             WHERE o.revision_id IN (SELECT id FROM tpir_revisions WHERE source = :source)
         UNION SELECT e.account_id FROM tpir_spend_events e
             JOIN tpir_spend_observations o ON o.spend_id = e.id
             WHERE o.revision_id IN (SELECT id FROM tpir_revisions WHERE source = :source)",
        named_params![":account_id": account_ref.0, ":source": source],
    )?;
    conn.execute(
        "DELETE FROM tpir_pending_pages
         WHERE account_id IN (SELECT account_id FROM tpir_quarantined_accounts)",
        [],
    )?;
    super::super::require_reader_version(conn, super::super::ACTIVATION_READER_VERSION)
}
