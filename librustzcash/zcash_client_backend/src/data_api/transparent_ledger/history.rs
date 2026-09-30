//! History completeness: what the wallet knows about one account's side of a transaction, and
//! what may still be missing.
//!
//! A history entry shows known activity honestly while discovery is incomplete. A missing output
//! or spend row does not mean the effect is absent, a missing fee is not zero, and a partial net
//! amount is not the transaction's final delta.

use zcash_primitives::transaction::TxId;
use zcash_protocol::{PoolType, consensus::BlockHeight, value::Zatoshis};

use super::PrivateTransparentDetail;

/// Whether every effect of a transaction on an account within one pool is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EffectCompleteness {
    /// No owned output or spend in this pool can be missing.
    Complete,
    /// Public transparent discovery holds authority for this pool. The wallet treats its results
    /// as authoritative, but their completeness is not verified.
    PublicDiscovery,
    /// Discovery has not covered this transaction. Owned outputs or spends may be missing, so the
    /// known amounts are partial.
    Incomplete,
}

impl EffectCompleteness {
    /// Whether the known effects can be treated as final: complete, or authoritative under public
    /// discovery.
    pub fn is_settled(self) -> bool {
        match self {
            Self::Complete | Self::PublicDiscovery => true,
            Self::Incomplete => false,
        }
    }
}

/// An account's known effects within one pool of a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolEffect {
    /// The pool.
    pub pool: PoolType,
    /// The known value the account received in this pool, including change.
    pub received: Zatoshis,
    /// The known value of the account's outputs spent in this pool.
    pub spent: Zatoshis,
    /// Whether these amounts are all of the account's effects in this pool.
    pub completeness: EffectCompleteness,
}

/// Whether a transaction's payment details are known: its recipients, payment amounts, and
/// memos.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DetailCompleteness {
    /// The wallet constructed and stored the transaction; or every effect is settled, every memo
    /// of the account's outputs has been retrieved, and either the account only received or the
    /// value it spent is accounted for by what it received back, its recorded outputs to others,
    /// and the fee. Stored transaction data alone does not suffice: outputs the wallet cannot
    /// decrypt are not recorded.
    Complete,
    /// Only discovered effects are known. Missing output rows do not mean there were no external
    /// payments, and a missing memo is not an empty one.
    Incomplete,
}

/// The fee of a transaction, as far as it concerns the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeState {
    /// The account spent funds in the transaction and its fee is recorded.
    Known(Zatoshis),
    /// The account spent funds, or may have, but the fee is not recorded. Never zero.
    Unknown,
    /// The account provably spent nothing in the transaction, so it paid no fee.
    NotApplicable,
}

/// How the account's view of a transaction was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryClassification {
    /// The wallet created the transaction, so its local record states the intent. Creation
    /// evidence without stored construction details, such as an outbox, certifies no effect or
    /// detail by itself.
    LocalIntent,
    /// Reconstructed from discovered evidence that is settled in every pool, where the account
    /// only received or the value it spent is accounted for. A missing memo alone does not make a
    /// transaction provisional.
    Reconstructed,
    /// Reconstructed from incomplete evidence. Later discovery or enhancement can change it; a
    /// provisional net debit is not a final payment amount.
    Provisional,
}

/// One account's history view of one transaction, from one database read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionHistoryDetails {
    /// The transaction.
    pub txid: TxId,
    /// The accepted-chain height the transaction is mined at, if any. This placement is
    /// independent of the completeness of the payment details.
    pub mined_height: Option<BlockHeight>,
    /// One entry for every pool this build supports, including pools without known effects.
    pub effects: Vec<PoolEffect>,
    /// Whether the transaction's recipients, payment amounts, and memos are known.
    pub payment_details: DetailCompleteness,
    /// The fee, as far as it concerns the account.
    pub fee: FeeState,
    /// How the account's view was established.
    pub classification: HistoryClassification,
    /// Transparent follow-on details for this transaction that the current policy withholds from
    /// public retrieval.
    pub pending_private_details: Vec<PrivateTransparentDetail>,
}
