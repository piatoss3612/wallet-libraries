//! Restore sweep steps around the directory and note-data lookups an app performs.
//!
//! For each due [`DiscoveryWork`](super::DiscoveryWork) item, lease it against the
//! publication with [`WalletDb::begin_swap_discovery_attempt`]. Unless its lookup
//! is already queued,
//! look its receiver up in the directory, retrieve note data for the positions
//! [`WalletDb::swap_note_data_needed`] returns, and queue the lookup with
//! [`WalletDb::queue_swap_directory_lookup`]. Then call [`WalletDb::apply_swap_sweep`].
use std::{
    borrow::{Borrow, BorrowMut},
    collections::BTreeMap,
};

use orchard::tree::{MerkleHashOrchard, MerklePath};
use rusqlite::{Connection, OptionalExtension, params};
use zakura_swap_receiving::{lifecycle::ChainAnchor, recovery::EncryptedNote};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::consensus::{BlockHeight, Parameters};

use super::{Error, KeyId, PaymentApplication, PendingPayment, corrupt, payments::key_ref};
use crate::{AccountUuid, WalletDb, wallet};

/// A directory publication more than this many blocks behind the wallet's scanned
/// tip is stale: finishing sweeps at it would leave a long rescan behind.
pub const MAX_PUBLICATION_LAG: u32 = 100;

/// A payment the receiver directory reports for a receiver, from public block data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPayment {
    /// Height of the block containing the payment.
    pub height: u32,
    /// Hash of that block.
    pub block_hash: [u8; 32],
    /// Transaction ID.
    pub txid: [u8; 32],
    /// Transaction index within the block.
    pub tx_index: u32,
    /// Action index within the transaction.
    pub action_index: u32,
    /// Note commitment tree position.
    pub position: u64,
    /// The action's input nullifier, not the received note's spend nullifier.
    pub action_nullifier: [u8; 32],
    /// Note commitment.
    pub cmx: [u8; 32],
    /// Ephemeral key.
    pub ephemeral_key: [u8; 32],
    /// The first 52 bytes of the note ciphertext.
    pub ciphertext_prefix: [u8; 52],
}

/// Why a sweep step must wait for more scanning or a newer publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepDeferral {
    /// The publication's block is not on the wallet's scanned chain.
    UnknownAnchor,
    /// The publication is more than [`MAX_PUBLICATION_LAG`] blocks behind the wallet.
    StalePublication,
    /// The publication does not reach the sweep's target block.
    TargetNotReached,
}

impl std::fmt::Display for SweepDeferral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnknownAnchor => "the directory publication's block is not on the scanned chain",
            Self::StalePublication => "the directory publication is too far behind the wallet",
            Self::TargetNotReached => "the directory publication does not reach the sweep's target",
        })
    }
}

impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// The anchor of a directory publication covering blocks through `height`, as the
    /// wallet's own chain records it. `through` is the wallet's fully scanned tip.
    pub fn swap_publication_anchor(
        &self,
        height: BlockHeight,
        through: ChainAnchor,
    ) -> Result<ChainAnchor, Error> {
        let unknown = Error::SweepDeferred(SweepDeferral::UnknownAnchor);
        if height > through.height {
            return Err(unknown);
        }
        let hash = wallet::get_block_hash(self.conn.borrow(), height)?.ok_or(unknown)?;
        if u32::from(through.height) - u32::from(height) > MAX_PUBLICATION_LAG {
            return Err(Error::SweepDeferred(SweepDeferral::StalePublication));
        }
        Ok(ChainAnchor {
            height,
            hash: hash.0,
        })
    }

    /// The commitment tree positions of `payments`, the directory's lookup for `key`,
    /// whose note data [`WalletDb::queue_swap_directory_lookup`] needs. Payments
    /// already imported or queued need none. A payment that contradicts imported or
    /// queued data is an error.
    pub fn swap_note_data_needed(
        &self,
        account: AccountUuid,
        key: KeyId,
        payments: &[DirectoryPayment],
    ) -> Result<Vec<u64>, Error> {
        let (_, missing) = self.sort_directory_payments(account, key, payments)?;
        Ok(missing.iter().map(|(p, _)| p.position).collect())
    }

    /// Splits `payments` into the queued candidates they repeat and payments not yet
    /// known, with their positions, skipping imported ones. A payment that contradicts
    /// an imported output is an error rather than another payment.
    #[allow(clippy::type_complexity)]
    fn sort_directory_payments<'a>(
        &self,
        account: AccountUuid,
        key: KeyId,
        payments: &'a [DirectoryPayment],
    ) -> Result<(Vec<PendingPayment>, Vec<(&'a DirectoryPayment, u32)>), Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let queued = self.pending_swap_payments(account, key)?;
        let mut kept = Vec::new();
        let mut missing = Vec::new();
        for payment in payments {
            let position = u32::try_from(payment.position)
                .map_err(|_| corrupt("directory payment position out of range"))?;
            let txid = TxId::from_bytes(payment.txid);
            let block_hash = BlockHash(payment.block_hash);
            let height = BlockHeight::from(payment.height);
            let imported: Option<(Option<i64>, u32, u32)> = conn
                .query_row(
                    "SELECT n.receiving_key_id, t.mined_height, n.commitment_tree_position
                     FROM ironwood_received_notes n
                     JOIN transactions t ON t.id_tx = n.transaction_id
                     WHERE t.txid = ?1 AND n.action_index = ?2 AND t.mined_height IS NOT NULL
                       AND n.commitment_tree_position IS NOT NULL",
                    params![txid.as_ref(), payment.action_index],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((owner, mined, imported_at)) = imported {
                if owner != Some(id)
                    || mined != payment.height
                    || imported_at != position
                    || wallet::get_block_hash(conn, height)? != Some(block_hash)
                {
                    return Err(corrupt("conflicting recovered payment identity"));
                }
                continue;
            }
            match queued
                .iter()
                .find(|p| p.txid == txid && p.action_index == payment.action_index)
            {
                Some(old)
                    if old.position == position
                        && old.height == height
                        && old.block_hash == block_hash
                        && old.encrypted_note.matches_compact(
                            payment.action_nullifier,
                            payment.cmx,
                            payment.ephemeral_key,
                            payment.ciphertext_prefix,
                        ) =>
                {
                    kept.push(old.clone())
                }
                Some(_) => return Err(corrupt("directory payment conflicts with a queued one")),
                None => missing.push((payment, position)),
            }
        }
        Ok((kept, missing))
    }
}

impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Queues the directory's complete lookup of `key` at `anchor`, from
    /// [`WalletDb::swap_publication_anchor`], which must reach the sweep's target. Each
    /// new payment is joined with its note data in `note_data`: the 528 ciphertext
    /// bytes after the directory's prefix, by position. Nothing is credited yet.
    pub fn queue_swap_directory_lookup(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        anchor: ChainAnchor,
        payments: &[DirectoryPayment],
        note_data: &BTreeMap<u64, [u8; 528]>,
    ) -> Result<(), Error> {
        self.transactionally(|db| {
            let id = key_ref(db.conn.0, account, key)?;
            let target: Option<u32> = db
                .conn
                .0
                .query_row(
                    "SELECT target_height FROM ironwood_swap_sweeps WHERE receiving_key_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if target.is_some_and(|target| u32::from(anchor.height) < target) {
                return Err(Error::SweepDeferred(SweepDeferral::TargetNotReached));
            }
            let (mut candidates, missing) = db.sort_directory_payments(account, key, payments)?;
            for (payment, position) in missing {
                let suffix = note_data
                    .get(&payment.position)
                    .ok_or_else(|| corrupt("missing note data for a directory payment"))?;
                candidates.push(PendingPayment {
                    txid: TxId::from_bytes(payment.txid),
                    action_index: payment.action_index,
                    height: payment.height.into(),
                    block_hash: BlockHash(payment.block_hash),
                    tx_index: payment
                        .tx_index
                        .try_into()
                        .map_err(|_| corrupt("directory payment index out of range"))?,
                    position,
                    encrypted_note: EncryptedNote::from_parts(
                        payment.action_nullifier,
                        payment.cmx,
                        payment.ephemeral_key,
                        payment.ciphertext_prefix,
                        suffix,
                    ),
                });
            }
            db.queue_swap_lookup(account, key, anchor, &candidates)
        })
    }

    /// Applies `key`'s queued payments with inclusion paths at `publication`, then
    /// finishes its sweep at its lookup's anchor. `witness` returns the 32 sibling
    /// hashes for a commitment tree position and note commitment, or `None` when the
    /// publication has none. `through` is the wallet's fully scanned tip. Returns
    /// [`PaymentApplication::Applied`] once the sweep is finished, or why a payment
    /// must wait; payments applied before it stay applied.
    pub fn apply_swap_sweep(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        through: ChainAnchor,
        publication: ChainAnchor,
        mut witness: impl FnMut(u32, [u8; 32]) -> Option<[[u8; 32]; 32]>,
    ) -> Result<PaymentApplication, Error> {
        for candidate in self.pending_swap_payments(account, key)? {
            let path = witness(candidate.position, candidate.encrypted_note.commitment())
                .and_then(|siblings| merkle_path(candidate.position, siblings));
            let Some(path) = path else {
                return Ok(PaymentApplication::AwaitingWitness);
            };
            let applied = self.apply_pending_swap_payment(
                account,
                key,
                &candidate,
                through,
                Some((publication, &path)),
            )?;
            if applied != PaymentApplication::Applied {
                return Ok(applied);
            }
        }
        let coverage = self
            .swap_lookup_coverage(account, key)?
            .ok_or(Error::SweepDeferred(SweepDeferral::UnknownAnchor))?;
        self.finish_swap_discovery_attempt(account, key, coverage)?;
        Ok(PaymentApplication::Applied)
    }
}

/// The inclusion path at `position`, if every sibling is a valid hash.
fn merkle_path(position: u32, siblings: [[u8; 32]; 32]) -> Option<MerklePath> {
    let hashes = siblings
        .iter()
        .map(|bytes| Option::from(MerkleHashOrchard::from_bytes(bytes)))
        .collect::<Option<Vec<_>>>()?;
    Some(MerklePath::from_parts(position, hashes.try_into().ok()?))
}
