//! The spend journal: every Ironwood nullifier of every transaction the block scan
//! recognizes, with the compact transaction that revealed it.
//!
//! A spend of a one-time-key note always carries an output to the spending account's
//! internal address (see `zcash_client_backend::fees::orchard::InputView::is_one_time`),
//! so the scan recognizes it. For a block covered for an account, meaning stored by a
//! scan that used that account's keys, the journal therefore holds every nullifier
//! that could spend one of the account's one-time notes. Entries and coverage above a
//! rewind's rescan floor are removed with those blocks (see [`truncate`]).

use std::ops::Range;

use prost::Message;
use rusqlite::{Connection, named_params};
use zcash_client_backend::data_api::ScannedBlock;
use zcash_protocol::consensus::BlockHeight;

use crate::{AccountUuid, error::SqliteClientError};

/// One recognized transaction, taken from a scanned block before storage consumes it.
pub(crate) struct Entry {
    height: BlockHeight,
    block_hash: [u8; 32],
    tx_index: u16,
    txid: [u8; 32],
    ironwood_start_position: u64,
    compact_tx: Vec<u8>,
    nullifiers: Vec<[u8; 32]>,
}

/// What a batch of scanned blocks adds to the journal: its recognized transactions, and
/// the ranges it covers for each account that scanned them.
pub(crate) struct Batch {
    entries: Vec<Entry>,
    coverage: Vec<(AccountUuid, Range<BlockHeight>)>,
}

/// Collects the journal entries and coverage of `blocks`, a contiguous batch.
pub(crate) fn collect(blocks: &[ScannedBlock<AccountUuid>]) -> Batch {
    let entries = blocks
        .iter()
        .flat_map(|block| {
            block.transactions().iter().filter_map(move |tx| {
                let scanned = tx.scanned_compact()?;
                let compact = scanned.tx();
                Some(Entry {
                    height: block.height(),
                    block_hash: block.block_hash().0,
                    tx_index: tx.block_index().into(),
                    txid: *tx.txid().as_ref(),
                    ironwood_start_position: scanned.ironwood_start_position().into(),
                    compact_tx: compact.encode_to_vec(),
                    // The scanner validated every nullifier before it recognized the
                    // transaction.
                    nullifiers: compact
                        .ironwood_actions
                        .iter()
                        .map(|action| action.nf().expect("validated by the scan").to_bytes())
                        .collect(),
                })
            })
        })
        .collect();
    // Each block names the accounts whose keys scanned it, so one batch can cover different
    // ranges for different accounts. A block scanned without that record, as the full-block
    // scanner produces, covers nothing.
    let mut coverage: Vec<(AccountUuid, Range<BlockHeight>)> = vec![];
    for block in blocks {
        for account in block.scanned_accounts() {
            match coverage
                .iter_mut()
                .rev()
                .find(|(a, range)| a == account && range.end == block.height())
            {
                Some((_, range)) => range.end = block.height() + 1,
                None => coverage.push((*account, block.height()..block.height() + 1)),
            }
        }
    }
    Batch { entries, coverage }
}

/// Records `batch` after its blocks are stored, in the same database transaction.
pub(crate) fn record(conn: &Connection, batch: Batch) -> Result<(), SqliteClientError> {
    let mut entry = conn.prepare_cached(
        "INSERT OR REPLACE INTO ironwood_spend_journal
            (block_height, tx_index, block_hash, txid, ironwood_start_position, compact_tx)
         VALUES (:height, :tx_index, :block_hash, :txid, :start, :compact_tx)",
    )?;
    let mut clear = conn.prepare_cached(
        "DELETE FROM ironwood_spend_journal_nullifiers
         WHERE block_height = :height AND tx_index = :tx_index",
    )?;
    let mut nullifier = conn.prepare_cached(
        "INSERT INTO ironwood_spend_journal_nullifiers
            (block_height, tx_index, action_index, nf)
         VALUES (:height, :tx_index, :action_index, :nf)",
    )?;
    for e in batch.entries {
        let height = u32::from(e.height);
        entry.execute(named_params! {
            ":height": height,
            ":tx_index": e.tx_index,
            ":block_hash": e.block_hash,
            ":txid": e.txid,
            ":start": e.ironwood_start_position,
            ":compact_tx": e.compact_tx,
        })?;
        // A block scanned again replaces its entries.
        clear.execute(named_params! {":height": height, ":tx_index": e.tx_index})?;
        for (action_index, nf) in e.nullifiers.iter().enumerate() {
            nullifier.execute(named_params! {
                ":height": height,
                ":tx_index": e.tx_index,
                ":action_index": action_index,
                ":nf": nf,
            })?;
        }
    }
    for (account, range) in batch.coverage {
        cover(conn, account, range)?;
    }
    Ok(())
}

/// Marks `range` covered for `account`, merging it with the ranges it overlaps or
/// touches. An account deleted since the scan began is skipped.
fn cover(
    conn: &Connection,
    account: AccountUuid,
    range: Range<BlockHeight>,
) -> Result<(), SqliteClientError> {
    let params = named_params! {
        ":uuid": account.0,
        ":start": u32::from(range.start),
        ":end": u32::from(range.end),
    };
    conn.execute(
        "INSERT INTO ironwood_journal_coverage (account_id, start_height, end_height)
         SELECT a.id,
                MIN(:start, IFNULL(MIN(c.start_height), :start)),
                MAX(:end, IFNULL(MAX(c.end_height), :end))
         FROM accounts a
         LEFT JOIN ironwood_journal_coverage c
           ON c.account_id = a.id AND c.start_height <= :end AND c.end_height >= :start
         WHERE a.uuid = :uuid
         GROUP BY a.id
         ON CONFLICT (account_id, start_height) DO UPDATE SET end_height = excluded.end_height",
        params,
    )?;
    // Drop the ranges the merged one absorbed.
    conn.execute(
        "DELETE FROM ironwood_journal_coverage AS c
         WHERE c.account_id = (SELECT id FROM accounts WHERE uuid = :uuid)
           AND EXISTS (
               SELECT 1 FROM ironwood_journal_coverage m
               WHERE m.account_id = c.account_id
                 AND m.start_height <= c.start_height AND m.end_height >= c.end_height
                 AND (m.start_height, m.end_height) != (c.start_height, c.end_height)
           )",
        named_params! {":uuid": account.0},
    )?;
    Ok(())
}

/// Removes the entries and coverage above `rescan_floor`, whose blocks a rewind may
/// replace.
pub(crate) fn truncate(
    conn: &Connection,
    rescan_floor: BlockHeight,
) -> Result<(), rusqlite::Error> {
    let floor = u32::from(rescan_floor);
    conn.execute(
        "DELETE FROM ironwood_spend_journal WHERE block_height > ?1",
        [floor],
    )?;
    conn.execute(
        "DELETE FROM ironwood_journal_coverage WHERE start_height > ?1",
        [floor],
    )?;
    conn.execute(
        "UPDATE ironwood_journal_coverage SET end_height = ?1 + 1 WHERE end_height > ?1 + 1",
        [floor],
    )?;
    Ok(())
}
