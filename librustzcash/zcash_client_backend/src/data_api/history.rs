//! Storage-neutral vocabulary for honest, possibly partial, transaction history.
//!
//! Owned-effect discovery, classification, recipients, memos, fees, and mining status are
//! independent facets. Knowing one never completes another, and an empty work queue is not
//! evidence of completeness. These types define the contract only; stores expose them when
//! the history read path is integrated.

use zcash_protocol::{PoolType, value::Zatoshis};

use super::transparent_ledger::ChainPoint;

/// Whether the wallet's own spends and receives in one pool are fully known.
///
/// Partial net amounts are not final transaction deltas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnedEffectsCompleteness {
    /// Discovery for this pool is complete through the accepted point.
    Complete {
        /// The point through which discovery is complete.
        through: ChainPoint,
    },
    /// Some owned effects may still be missing.
    Incomplete,
}

/// The basis for a transaction's displayed classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClassificationBasis {
    /// The wallet recorded the user's intent when it constructed the transaction.
    LocalIntent,
    /// The classification was reconstructed from discovered evidence.
    Reconstructed {
        /// Whether missing effects or details could still change the classification.
        provisional: bool,
    },
}

/// Whether the known outputs are all of the transaction's payments.
///
/// Missing output rows do not mean there were no external payments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecipientCompleteness {
    /// Every output of the transaction is known.
    Complete,
    /// Outputs, including external payments, may be missing.
    Incomplete,
}

/// The state of one optional detail. Unknown, zero, and not applicable stay distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DetailState<T> {
    /// The detail is known.
    Known(T),
    /// The detail does not exist for this transaction or output.
    NotApplicable,
    /// Supported recovery is outstanding.
    Pending,
    /// No supported capability can recover the detail under the current policy.
    Unsupported,
    /// The detail is not known and no recovery is scheduled.
    Unknown,
}

/// Where a known fee came from. Server metadata is not authenticated by note decryption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeeProvenance {
    /// Recorded when the wallet constructed the transaction.
    LocalConstruction,
    /// Computed from raw transaction bytes and known input values.
    TransactionData,
    /// Asserted by an indexing service and trusted as such.
    ServiceAsserted,
}

/// A known fee and its provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeEvidence {
    /// The fee value.
    pub value: Zatoshis,
    /// Where the value came from.
    pub provenance: FeeProvenance,
}

/// Placement evidence on the accepted chain, independent of payment-detail completeness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MiningEvidence {
    /// Mined in the accepted block.
    Mined(ChainPoint),
    /// Known and not mined on the accepted chain.
    Unmined,
    /// No placement evidence is available.
    Unknown,
}

/// Owned-effect completeness for one pool a transaction touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolEffectsCompleteness {
    /// The pool.
    pub pool: PoolType,
    /// Whether the wallet's effects in that pool are fully known.
    pub completeness: OwnedEffectsCompleteness,
}

/// The completeness facets of one transaction in wallet history.
///
/// Per-output memo state is reported alongside each output as a [`DetailState`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionHistoryCompleteness {
    /// Owned-effect completeness for each pool the transaction touches. A mixed transaction
    /// can be complete in one pool and incomplete in another.
    pub owned_effects: Vec<PoolEffectsCompleteness>,
    /// The basis of the displayed classification.
    pub classification: ClassificationBasis,
    /// Whether all recipients are known.
    pub recipients: RecipientCompleteness,
    /// The fee, if known.
    pub fee: DetailState<FeeEvidence>,
    /// Placement on the accepted chain.
    pub mining: MiningEvidence,
}
