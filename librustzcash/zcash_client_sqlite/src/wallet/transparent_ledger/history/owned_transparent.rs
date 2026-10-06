//! Read-time display relationships between current owned outputs and wallet funding participants.
//! No relationship here allocates value, creates sent notes, or proves an account paid a fee.

use std::collections::HashMap;

use rusqlite::named_params;
use transparent::{address::TransparentAddress, bundle::OutPoint};
use zcash_client_backend::data_api::transparent_ledger::{
    OwnedTransparentOutput, TransactionHistoryDetails, TransparentLedgerMode,
    TransparentOutputScope,
};
use zcash_keys::{encoding::AddressCodec, keys::transparent::gap_limits::GapLimits};
use zcash_protocol::{PoolType, consensus};

use super::{read_account_history, transaction_facts, zatoshis};
use crate::{AccountUuid, error::SqliteClientError, wallet::encoding::KeyScope};

/// Enriches an involved account's entry with transaction-wide ownership and funding facts.
/// Only settled owned effects permit the single-known-funder display convention. Financial
/// fields remain untouched; all queries run inside the caller's read snapshot.
pub(super) fn reconcile<P: consensus::Parameters>(
    conn: &rusqlite::Connection,
    params: &P,
    gap_limits: &GapLimits,
    configured: Option<TransparentLedgerMode>,
    entry: &mut TransactionHistoryDetails<AccountUuid>,
) -> Result<(), SqliteClientError> {
    let Some(tx) = transaction_facts(conn, &entry.txid)? else {
        return Ok(());
    };
    let current_output = super::super::output_observation_condition("u");
    let mut funding = conn.prepare_cached(&format!(
        "SELECT DISTINCT a.uuid FROM v_received_outputs ro
         JOIN v_received_output_spends s ON s.pool = ro.pool
             AND s.received_output_id = ro.id_within_pool_table
         JOIN accounts a ON a.id = ro.account_id
         LEFT JOIN transparent_received_outputs u ON ro.pool = 0 AND u.id = ro.id_within_pool_table
         WHERE s.transaction_id = :tx AND (ro.pool != 0 OR ({current_output}))
         UNION
         SELECT a.uuid FROM tpir_spend_events e
         JOIN tpir_active_accounts active ON active.account_id = e.account_id
         JOIN accounts a ON a.id = e.account_id
         WHERE e.spending_txid = :txid AND e.mined_height = :mined_height
         AND NOT EXISTS (SELECT 1 FROM tpir_quarantined_accounts qa
                         WHERE qa.account_id = e.account_id)
         AND EXISTS (
             SELECT 1 FROM tpir_spend_observations o
             JOIN tpir_qualified_revisions q ON q.revision_id = o.revision_id
             JOIN tpir_revisions r ON r.id = o.revision_id
             WHERE o.spend_id = e.id
             AND NOT EXISTS (SELECT 1 FROM tpir_quarantined_sources qs
                             WHERE qs.source = r.source)
         )
         ORDER BY uuid"
    ))?;
    // A qualified active-ledger spend establishes participation before its parent output
    // (and therefore its value or canonical spend link) has been recovered.
    entry.known_wallet_funders = funding
        .query_map(
            named_params![
                ":tx": tx.id,
                ":txid": entry.txid.as_ref(),
                ":mined_height": tx.mined_height.map(u32::from),
            ],
            |r| r.get(0).map(AccountUuid),
        )?
        .collect::<Result<_, _>>()?;

    let mut effects = HashMap::new();
    let mut read_effects = |account| -> Result<_, SqliteClientError> {
        if let std::collections::hash_map::Entry::Vacant(slot) = effects.entry(account) {
            let history =
                read_account_history(conn, params, gap_limits, configured, account, &[entry.txid])?;
            slot.insert(history.into_iter().next());
        }
        Ok(effects
            .get(&account)
            .and_then(|e| e.as_ref())
            .map(|e| e.effects.clone()))
    };
    let inferred_funder = if !tx.constructed && entry.known_wallet_funders.len() == 1 {
        let account = entry.known_wallet_funders[0];
        read_effects(account)?
            .filter(|effects| effects.iter().all(|e| e.completeness.is_settled()))
            .map(|_| account)
    } else {
        None
    };
    let mut outputs = conn.prepare_cached(&format!(
        "SELECT u.output_index, u.value_zat, u.address, a.uuid, ad.key_scope
         FROM transparent_received_outputs u
         JOIN accounts a ON a.id = u.account_id
         LEFT JOIN addresses ad ON ad.id = u.address_id AND ad.account_id = u.account_id
         WHERE u.transaction_id = :tx AND ({current_output}) ORDER BY u.output_index"
    ))?;
    let rows = outputs.query_map(named_params![":tx": tx.id], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            AccountUuid(r.get(3)?),
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    for row in rows {
        let (index, value, address, recipient_account, scope) = row?;
        let scope = scope
            .map(KeyScope::decode)
            .transpose()?
            .map(|scope| match scope {
                KeyScope::Zip32(zip32::Scope::External) => TransparentOutputScope::External,
                KeyScope::Zip32(zip32::Scope::Internal) => TransparentOutputScope::Internal,
                KeyScope::Ephemeral => TransparentOutputScope::Ephemeral,
                KeyScope::Foreign => TransparentOutputScope::Foreign,
            });
        let receiver_settled = read_effects(recipient_account)?.is_some_and(|effects| {
            effects
                .iter()
                .any(|e| e.pool == PoolType::Transparent && e.completeness.is_settled())
        });
        entry
            .owned_transparent_outputs
            .push(OwnedTransparentOutput {
                outpoint: OutPoint::new(*entry.txid.as_ref(), index),
                value: zatoshis(value)?,
                address: TransparentAddress::decode(params, &address)?,
                recipient_account,
                scope,
                inferred_funding_account: inferred_funder.filter(|_| receiver_settled),
            });
    }
    Ok(())
}
