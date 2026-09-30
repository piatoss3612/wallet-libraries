use super::*;

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
