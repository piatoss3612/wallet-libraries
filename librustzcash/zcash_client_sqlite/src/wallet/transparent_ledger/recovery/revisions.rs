//! Revision identity registration and trusted wallet-wide replacement.

use super::*;
use rusqlite::OptionalExtension as _;

/// Registers an observation without invalidating evidence or changing financial state.
/// Only qualified lineages make older provisional observations stale.
#[cfg(feature = "transparent-inputs")]
pub(super) fn register_revision(
    conn: &rusqlite::Connection,
    revision: &RecoveryRevision,
) -> Result<i64, SqliteClientError> {
    super::super::require_reader_version(conn, super::super::REVISION_READER_VERSION)?;
    let lineage = i64::try_from(revision.lineage).expect("checked by check_well_formed");
    let accepted: Option<i64> = conn.query_row(
        "SELECT MAX(r.lineage) FROM tpir_revisions r
         JOIN tpir_qualified_revisions q ON q.revision_id = r.id WHERE r.source = :source",
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
    Ok(id)
}

/// Withdraws older provisional evidence after the caller authorizes this exact revision.
/// Must run in the same transaction as registration and qualification.
pub(super) fn supersede_provisional(
    conn: &rusqlite::Connection,
    revision: &RecoveryRevision,
) -> Result<(), SqliteClientError> {
    let lineage = i64::try_from(revision.lineage).expect("validated revision lineage");
    let older_provisional = "SELECT id FROM tpir_revisions
         WHERE source = :source AND sealed = 0 AND lineage < :lineage";
    for table in [
        "tpir_transaction_metadata",
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
    projection::unproject_unobserved(conn)?;
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
    Ok(())
}
