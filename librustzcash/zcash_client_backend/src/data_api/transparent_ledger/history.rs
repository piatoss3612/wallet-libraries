//! History completeness: what the wallet knows about one account's side of a transaction, and
//! what may still be missing.
//!
//! A history entry shows known activity honestly while discovery is incomplete. A missing output
//! or spend row does not mean the effect is absent, a missing fee is not zero, and a partial net
//! amount is not the transaction's final delta.

use transparent::{address::TransparentAddress, bundle::OutPoint};
use zcash_primitives::transaction::TxId;
use zcash_protocol::{PoolType, consensus::BlockHeight, value::Zatoshis};

use super::{
    AccountMovement, AggregatePayment, PrivateTransparentDetail, TransactionMetadataEvidence,
};

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
    /// Reconstructed as a net movement only. Every effect is complete and the account's spent
    /// value equals its receipts plus the exact whole-transaction fee, so the account's movement
    /// is final; but the wallet lacks the full transaction, and no available evidence excludes
    /// another party's self-balanced shielded participation (a foreign shielded spend paying an
    /// equal foreign output that looks like padding). Whether the account's debit was the fee or
    /// a payment while the other party paid the fee is therefore not established. The fee is not
    /// attributed to the account and no aggregate payment is inferred. A privately recovered
    /// transparent-to-shielded self-transfer whose transparent inputs are all the account's is
    /// reported this way.
    NetReconstructed,
    /// Reconstructed from incomplete evidence. Later discovery or enhancement can change it; a
    /// provisional net debit is not a final payment amount.
    Provisional,
}

/// A transparent receiver's role in the wallet's existing activity presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransparentOutputScope {
    /// An ordinary externally derived receiver.
    External,
    /// An internally derived change receiver.
    Internal,
    /// An ephemeral funding receiver.
    Ephemeral,
    /// An independently imported receiver whose ownership is known.
    Foreign,
}

/// A currently supported wallet-owned output, separate from financial sender attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedTransparentOutput<AccountId> {
    /// Transaction and transparent output index.
    pub outpoint: OutPoint,
    /// Output value, never reduced by a fee.
    pub value: Zatoshis,
    /// The recovered transparent receiver.
    pub address: TransparentAddress,
    /// Its current wallet owner.
    pub recipient_account: AccountId,
    /// Unknown is not an external scope.
    pub scope: Option<TransparentOutputScope>,
    /// The single known wallet funder, only when its owned effects and the receiver's
    /// transparent effects are settled. This is the public wallet's display convention,
    /// not proof of sole transaction funding, per-output funding, or fee payment.
    pub inferred_funding_account: Option<AccountId>,
}

/// One account's history view of one transaction, from one database read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionHistoryDetails<AccountId> {
    /// Display-only Enhance service assertion that the transaction contains transparent
    /// outputs. `None` means no assertion has been recovered. This transaction-wide fact
    /// can classify activity independently of payment-detail completeness; it does not
    /// establish recipients, output ownership, or account payment/fee attribution.
    pub has_transparent_outputs: Option<bool>,
    /// Current owned outputs of the transaction, including those owned by other wallet accounts.
    /// Display-only reconciliation never creates sent notes or completes payment evidence.
    pub owned_transparent_outputs: Vec<OwnedTransparentOutput<AccountId>>,
    /// Distinct accounts with currently supported spends, including qualified active private
    /// spend evidence whose parent output has not yet been recovered. No order implies funding
    /// priority, and this list cannot exclude outside participants.
    pub known_wallet_funders: Vec<AccountId>,
    /// Whole-transaction facts, separate from the account-related fee.
    pub transaction_metadata: Option<TransactionMetadataEvidence>,
    /// Aggregate outgoing amount with explicit completeness.
    pub aggregate_payment: AggregatePayment,
    /// Known account movement and whether every effect is established.
    pub account_movement: AccountMovement,
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
