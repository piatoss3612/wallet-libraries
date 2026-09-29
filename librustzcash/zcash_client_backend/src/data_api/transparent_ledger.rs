//! Storage-neutral contract for transparent ledger configuration and financial authority.
//!
//! This is the preparatory surface of the private transparent ledger: explicit handle modes
//! and an honest balance-and-authority snapshot. Recovery commits, promotion, and their
//! supporting types are added with the recovery and activation work that implements them;
//! see `docs/transparent-pir-ledger-architecture.md` and `docs/transparent-pir-baseline.md`.

#[cfg(feature = "test-dependencies")]
use ambassador::delegatable_trait;
use zcash_primitives::block::BlockHash;
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
