//! Storage-neutral contract for transparent ledger configuration and financial authority.
//!
//! This is the preparatory surface of the private transparent ledger: explicit handle modes,
//! durable policy transitions, and an honest balance-and-authority snapshot. Recovery commits,
//! promotion, and their supporting types are added with the recovery and activation work that
//! implements them; see `docs/transparent-pir-ledger-architecture.md` and
//! `docs/transparent-pir-ledger-design-notes.md`.

#[cfg(feature = "test-dependencies")]
use ambassador::delegatable_trait;
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::consensus::BlockHeight;

use super::{Balance, WalletRead, wallet::ConfirmationsPolicy};

/// A locally accepted block: its height and the hash the wallet holds for that height.
///
/// Coverage through `H` can support a transaction targeting `H + 1`; the future block has
/// no accepted hash to check. A publisher's asserted hash never substitutes for a local one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChainPoint {
    /// Height of the accepted block.
    pub height: BlockHeight,
    /// The wallet's hash for the block at `height`.
    pub hash: BlockHash,
}

/// Source authorization for transparent discovery and financial authority.
///
/// Every handle performing transparent discovery or financial authorization must be
/// configured explicitly. No mode is implied by an empty or newly created wallet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransparentLedgerMode {
    /// Public transparent discovery remains authoritative.
    Public,
    /// Public discovery remains authoritative while private recovery runs in isolation for
    /// qualification. This is not a privacy claim.
    PrivateShadow,
    /// Public transparent discovery is forbidden, including while private recovery is
    /// unavailable or incomplete. Transparent inputs require private authority.
    PrivateRequired,
}

impl TransparentLedgerMode {
    /// Returns whether public discovery retains transparent financial authority.
    pub fn retains_public_authority(self) -> bool {
        match self {
            Self::Public | Self::PrivateShadow => true,
            Self::PrivateRequired => false,
        }
    }
}

/// The durable transparent policy as last applied to the wallet.
///
/// `generation` increments by one on each mode change. Outstanding public follow-on work is
/// stamped with the generation that produced it; a mismatched generation is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AppliedTransparentPolicy {
    /// The mode durably applied to the wallet.
    pub mode: TransparentLedgerMode,
    /// Monotonic counter of mode transitions; unchanged by same-mode reapplication.
    pub generation: u64,
}

/// A transparent follow-on detail that cannot be recovered over a public request under the
/// current policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrivateTransparentDetail {
    /// A parent-transaction retrieval queued for a transparent input, withheld from public
    /// enhancement dispatch.
    ParentTransaction {
        /// The parent transaction to retrieve.
        txid: TxId,
    },
    /// A mixed transaction whose Enhance response indicated transparent data; public LWD
    /// enhancement is forbidden while the transaction remains unresolved (no stored raw).
    /// The sticky route-2 marker is not itself completion; storing full data, or restoring
    /// public authority for newly stamped work, ends the pending private detail.
    MixedTransaction {
        /// The mixed transaction.
        txid: TxId,
    },
}

/// The source of current transparent financial authority for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransparentAuthority {
    /// Balances and inputs derive from public discovery.
    Public,
    /// No current authority can be established; transparent inputs are unavailable.
    Unavailable,
}

/// A transparent balance split by coinbase classification, using the existing confirmation
/// and lock categories of [`Balance`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransparentLedgerBalance {
    /// Non-coinbase outputs.
    pub regular: Balance,
    /// Coinbase outputs, subject to maturity.
    pub coinbase: Balance,
}

/// Where a last-known amount came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LastKnownSource {
    /// Rows admitted under public authority before private authority applied.
    LegacyPublic,
    /// Legacy public rows together with rows recorded by local transaction construction after
    /// private authority applied, such as a shielded-funded payment to an own transparent
    /// receiver.
    LegacyPublicAndLocal,
}

/// A prior amount that is informational only; it never authorizes a spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastKnownBalance {
    /// The amount as last established.
    pub balance: TransparentLedgerBalance,
    /// Its provenance.
    pub source: LastKnownSource,
    /// The accepted point it was established at, when one was verified.
    pub at: Option<ChainPoint>,
}

/// Financial recovery progress for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryCompletion {
    /// Public authority applies; private recovery completion is not required.
    NotApplicable,
    /// Authority cannot be established until the listed blockers clear.
    Blocked,
}

/// A reason transparent financial authority is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryBlocker {
    /// This build or configuration cannot perform private recovery.
    PrivateRecoveryUnavailable,
    /// No accepted chain point is known locally.
    ChainUnknown,
    /// This build cannot read the wallet's transparent state.
    TransparentSupportUnavailable,
}

/// The single atomic balance-and-authority result for one account's transparent funds.
///
/// Every field comes from one database read. Unavailable is not zero: an absent
/// `authorized` balance means no current authority, never an empty wallet. This describes
/// transparent financial authority only, not whole-wallet history completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransparentLedgerSnapshot<AccountId> {
    /// The account described.
    pub account: AccountId,
    /// The handle's configured mode.
    pub mode: TransparentLedgerMode,
    /// The current source of financial authority.
    pub authority: TransparentAuthority,
    /// The spendable-authority balance; absent when authority cannot be established.
    pub authorized: Option<TransparentLedgerBalance>,
    /// A prior amount shown for context; never current or spendable.
    pub last_known: Option<LastKnownBalance>,
    /// Whether authority is established or blocked.
    pub completion: RecoveryCompletion,
    /// Reasons authority is unavailable.
    pub blockers: Vec<RecoveryBlocker>,
}

/// Reads transparent ledger configuration and financial authority.
///
/// Implementations reject unconfigured handles, including for an empty wallet, and must not
/// fabricate coverage or a spendable private balance from incomplete state.
#[cfg_attr(feature = "test-dependencies", delegatable_trait)]
pub trait TransparentLedgerRead: WalletRead {
    /// Returns the mode this handle operates under.
    ///
    /// Fails when the handle is unconfigured, or when its mode is weaker than a policy
    /// durably applied to the wallet. A stored stricter policy is never weakened by reading.
    fn transparent_ledger_mode(&self) -> Result<TransparentLedgerMode, Self::Error>;

    /// Returns the durable policy applied to the wallet, including its generation.
    ///
    /// The handle must already be configured. A stored `PrivateRequired` policy is never
    /// weakened by this read.
    fn applied_transparent_policy(&self) -> Result<AppliedTransparentPolicy, Self::Error>;

    /// Confirms that the durable policy generation still equals `expected`.
    ///
    /// Used as the commit check for an operation that captured the generation at the start of
    /// its SQLite transaction. Fails when another connection has since applied a transition.
    fn check_transparent_policy_generation(&self, expected: u64) -> Result<(), Self::Error>;

    /// Returns transparent follow-on details withheld from public dispatch under the current
    /// policy, such as parent-transaction retrieval and mixed-transaction markers.
    fn pending_private_transparent_details(
        &self,
    ) -> Result<Vec<PrivateTransparentDetail>, Self::Error>;

    /// Returns the transparent balance-and-authority snapshot for `account`, from one read.
    ///
    /// `confirmations_policy` applies the existing confirmation rules to any authorized or
    /// last-known balance. A historical snapshot never authorizes a current spend.
    fn transparent_ledger_snapshot(
        &self,
        account: Self::AccountId,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<TransparentLedgerSnapshot<Self::AccountId>, Self::Error>;
}

/// Writes transparent ledger policy transitions.
///
/// Recovery commits and promotion are added with the work that implements them. Implementations
/// typically also implement [`WalletWrite`](super::WalletWrite); the write trait itself only
/// requires the read contract so associated `Error` types stay unambiguous.
#[cfg_attr(feature = "test-dependencies", delegatable_trait)]
pub trait TransparentLedgerWrite: TransparentLedgerRead {
    /// Durably applies `mode` as the wallet's transparent policy.
    ///
    /// A mode change increments `policy_generation` by one in the same SQLite transaction.
    /// Re-applying the current mode does not increment it and does not revoke work. Applying
    /// [`TransparentLedgerMode::PrivateRequired`] also raises the minimum reader version so
    /// older readers fail closed. An explicit later transition back to `Public` or
    /// `PrivateShadow` is allowed; reads still never weaken a stored `PrivateRequired` policy
    /// via a weaker handle configuration.
    ///
    /// The handle must already be configured. Returns the policy after the write.
    fn apply_transparent_policy(
        &mut self,
        mode: TransparentLedgerMode,
    ) -> Result<AppliedTransparentPolicy, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::TransparentLedgerMode;

    #[test]
    fn only_private_required_drops_public_authority() {
        assert!(TransparentLedgerMode::Public.retains_public_authority());
        assert!(TransparentLedgerMode::PrivateShadow.retains_public_authority());
        assert!(!TransparentLedgerMode::PrivateRequired.retains_public_authority());
    }
}
