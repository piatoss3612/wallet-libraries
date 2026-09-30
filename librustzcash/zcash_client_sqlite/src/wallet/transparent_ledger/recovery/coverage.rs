//! Shared interval semantics for authority diagnostics and resumable scheduling.

use super::*;

/// Merges inclusive `(from, through)` ranges into disjoint, non-adjacent ranges.
#[cfg(feature = "transparent-inputs")]
pub(super) fn merge(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
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

pub(super) struct Coverage {
    pub(super) supported: BTreeMap<Vec<u8>, Vec<(u32, u32)>>,
    pub(super) unsupported: Vec<(Vec<u8>, u32, u32)>,
}

pub(super) fn read(
    conn: &rusqlite::Connection,
    account_ref: AccountRef,
) -> Result<Coverage, SqliteClientError> {
    // Reject damaged historical state instead of treating an obsolete anchor as coverage.
    let invalid: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM tpir_coverage c LEFT JOIN blocks b ON b.height = c.anchor_height
         WHERE c.account_id = ?1 AND (b.hash IS NULL OR b.hash != c.anchor_hash))",
        [account_ref.0], |row| row.get(0),
    )?;
    if invalid {
        return Err(SqliteClientError::CorruptedData(
            "transparent coverage anchor is not accepted; rewind and recover before use".into(),
        ));
    }
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

    Ok(Coverage {
        supported,
        unsupported,
    })
}

/// Complement of supported intervals inside an inclusive required interval. Uses u64 for
/// the cursor so a covered u32::MAX endpoint does not wrap or manufacture another gap.
pub(super) fn missing_ranges(from: u32, through: u32, covered: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut cursor = u64::from(from);
    let end = u64::from(through);
    let mut missing = vec![];
    for (start, stop) in merge(covered) {
        let start = u64::from(start);
        let stop = u64::from(stop);
        if stop < cursor {
            continue;
        }
        if start > end {
            break;
        }
        if start > cursor {
            missing.push((cursor as u32, (start - 1).min(end) as u32));
        }
        cursor = cursor.max(stop + 1);
        if cursor > end {
            break;
        }
    }
    if cursor <= end {
        missing.push((cursor as u32, through));
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_intervals_match_an_independent_point_oracle() {
        for mask in 0u32..256 {
            let covered: Vec<_> = (0..8)
                .filter(|h| mask & (1 << h) != 0)
                .map(|h| (h, h))
                .collect();
            for from in 0..9 {
                for through in from..9 {
                    let actual: BTreeSet<_> = missing_ranges(from, through, covered.clone())
                        .into_iter()
                        .flat_map(|(a, b)| a..=b)
                        .collect();
                    let expected: BTreeSet<_> = (from..=through)
                        .filter(|h| *h >= 8 || mask & (1 << h) == 0)
                        .collect();
                    assert_eq!(actual, expected, "mask={mask}, range={from}..={through}");
                }
            }
        }
    }
    #[test]
    fn complements_merge_overlap_adjacency_and_handle_maximum_height() {
        assert_eq!(
            missing_ranges(0, 10, vec![(2, 4), (4, 6), (7, 8), (2, 3)]),
            vec![(0, 1), (9, 10)]
        );
        assert_eq!(missing_ranges(5, 4, vec![]), vec![]);
        assert_eq!(
            missing_ranges(u32::MAX, u32::MAX, vec![]),
            vec![(u32::MAX, u32::MAX)]
        );
        assert!(missing_ranges(u32::MAX, u32::MAX, vec![(u32::MAX, u32::MAX)]).is_empty());
        assert_eq!(
            missing_ranges(0, u32::MAX, vec![(1, u32::MAX - 1)]),
            vec![(0, 0), (u32::MAX, u32::MAX)]
        );
    }
}
