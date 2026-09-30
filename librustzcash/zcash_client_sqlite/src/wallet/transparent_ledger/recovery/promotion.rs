//! Promotion of an account's complete candidate ledger to private authority.

use super::*;

/// Promotes `account` to private authority, atomically; see
/// `TransparentLedgerWrite::promote_transparent_account`.
#[cfg(feature = "transparent-inputs")]
pub(crate) fn promote<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    configured: Option<TransparentLedgerMode>,
    account: AccountUuid,
) -> Result<(), SqliteClientError> {
    atomically(conn, |conn| {
        let handle = resolve_mode(conn, configured)?;
        let durable = durable_policy(conn)?.map(|policy| policy.mode);
        if handle != TransparentLedgerMode::PrivateRequired
            || durable != Some(TransparentLedgerMode::PrivateRequired)
        {
            return Err(SqliteClientError::TransparentRecoveryNotEnabled);
        }
        let watch = Watch::load(conn, params, account)?.ok_or(SqliteClientError::AccountUnknown)?;
        let account_ref = watch.account.internal_id();
        let ledger = AccountLedger {
            account_ref,
            lifecycle: lifecycle(conn, account_ref)?,
            quarantined: account_quarantined(conn, account_ref)?,
            status: recovery_status(conn, gap_limits, &watch)?,
        };
        if ledger.lifecycle == AccountLifecycle::Active {
            return Ok(());
        }
        let tip = chain_tip_height(conn)?;
        let blockers: Vec<_> = ledger_blockers(conn, &ledger, tip)?
            .into_iter()
            .filter(|b| *b != RecoveryBlocker::NotActivated)
            .collect();
        if !blockers.is_empty() {
            return Err(SqliteClientError::TransparentPromotionBlocked(blockers));
        }

        // Window addresses become the wallet's own, so that their outputs can be projected.
        for (slot, scope) in WINDOW_SCOPES.into_iter().enumerate() {
            let (start, end) = (watch.production_end[slot], watch.candidate_end[slot]);
            if start < end {
                generate_address_range(
                    conn,
                    params,
                    account_ref,
                    scope,
                    UnifiedAddressRequest::unsafe_custom(Allow, Allow, Require),
                    NonHardenedChildIndex::from_index(start).expect("below WINDOW_LIMIT")
                        ..NonHardenedChildIndex::from_index(end)
                            .expect("a window at WINDOW_LIMIT blocks promotion"),
                    false,
                )?;
            }
        }
        // So does the legacy external receiver, which the watch set includes without a row.
        // Storing an index that already has a row is a no-op.
        if let Some((_, index)) = get_legacy_transparent_address(params, conn, account)?
            && let Some(end) = index
                .index()
                .checked_add(1)
                .and_then(NonHardenedChildIndex::from_index)
        {
            generate_address_range(
                conn,
                params,
                account_ref,
                TransparentKeyScope::EXTERNAL,
                UnifiedAddressRequest::unsafe_custom(Allow, Allow, Require),
                index..end,
                false,
            )?;
        }
        conn.execute(
            "DELETE FROM tpir_candidate_windows WHERE account_id = :account_id",
            named_params![":account_id": account_ref.0],
        )?;

        for receive in placed_receives(conn, account_ref)? {
            projection::project_receive(conn, params, gap_limits, account, &receive)?;
        }
        for spend in placed_spends(conn, account_ref)? {
            projection::project_spend(conn, params, gap_limits, &spend)?;
        }

        conn.execute(
            "INSERT INTO tpir_active_accounts (account_id) VALUES (:account_id)",
            named_params![":account_id": account_ref.0],
        )?;
        super::super::require_reader_version(conn, super::super::ACTIVATION_READER_VERSION)
    })
}
