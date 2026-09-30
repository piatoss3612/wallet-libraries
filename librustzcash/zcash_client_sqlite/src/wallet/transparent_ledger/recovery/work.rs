//! Bounded scheduling view derived from the same coverage as financial authority.

use super::*;
use std::num::NonZeroUsize;
use zcash_client_backend::data_api::transparent_ledger::{
    TransparentRecoveryWork, TransparentRecoveryWorkBatch,
};

/// Caller supplies a single read snapshot. Completed work remains in SQLite, so repeated
/// batches resume without application-owned coverage or a persistent cursor.
pub(crate) fn recovery_work<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    configured: Option<TransparentLedgerMode>,
    account: AccountUuid,
    limit: NonZeroUsize,
) -> Result<TransparentRecoveryWorkBatch<AccountUuid>, SqliteClientError> {
    let ws = watch_set(conn, params, configured, account)?;
    let watch = Watch::load(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
    if account_quarantined(conn, watch.account.internal_id())? {
        return Err(reject(CommitRejection::Refused(
            RefusedCommit::AccountQuarantined,
        )));
    }
    let context = ws.context();
    let Some(target) = ws.target else {
        return Ok(TransparentRecoveryWorkBatch {
            context,
            items: vec![],
            has_more: false,
        });
    };
    let limit = limit.get().min(256);
    let mut items = vec![];
    let mut pages = ws.pending_pages;
    pages.sort_by(|a, b| {
        (&a.revision.source, a.revision.lineage, &a.request.page).cmp(&(
            &b.revision.source,
            b.revision.lineage,
            &b.request.page,
        ))
    });
    items.extend(
        pages
            .iter()
            .take(limit + 1)
            .cloned()
            .map(TransparentRecoveryWork::ResumePage),
    );
    if items.len() <= limit {
        let supported = coverage::read(conn, watch.account.internal_id())?.supported;
        'addresses: for address in ws.addresses {
            let mut occupied = supported
                .get(&script_bytes(&address.address))
                .cloned()
                .unwrap_or_default();
            occupied.extend(
                pages
                    .iter()
                    .filter(|p| p.request.addresses.contains(&address.address))
                    .map(|p| (u32::from(p.request.from), u32::from(p.request.through))),
            );
            for (from, through) in coverage::missing_ranges(
                u32::from(address.required_from),
                u32::from(target.height),
                occupied,
            ) {
                items.push(TransparentRecoveryWork::CheckRange(AddressRange {
                    address: address.address,
                    from: height(from),
                    through: height(through),
                }));
                if items.len() > limit {
                    break 'addresses;
                }
            }
        }
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok(TransparentRecoveryWorkBatch {
        context,
        items,
        has_more,
    })
}
