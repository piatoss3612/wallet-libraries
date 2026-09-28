//! Storage-neutral contract for privately recovered transparent ledger state.
//!
//! Recovery sources submit normalized receive/spend events, source-bound coverage, and
//! resumable progress. The store decides whether a submission is rejected, isolated as
//! candidate state, or projected into wallet state; a source cannot choose its destination.
//! No transport, filter layout, shard, or PIR protocol type appears here. See
//! `docs/transparent-pir-ledger-architecture.md` for the invariants these types carry.

#[cfg(feature = "test-dependencies")]
use ambassador::delegatable_trait;
use transparent::{address::Script, bundle::OutPoint};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};

use super::{Balance, WalletRead, WalletWrite, wallet::ConfirmationsPolicy};

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
    /// Balances and inputs derive from promoted private ledger state.
    Private,
    /// No current authority can be established; transparent inputs are unavailable.
    Unavailable,
}

/// The largest accepted length, in bytes, of an opaque source, revision, or page identifier.
pub const MAX_OPAQUE_ID_LEN: usize = 64;

/// Why an opaque identifier was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpaqueIdError {
    /// Identifiers must not be empty.
    Empty,
    /// The identifier exceeded [`MAX_OPAQUE_ID_LEN`] bytes.
    TooLong {
        /// The rejected length.
        len: usize,
    },
}

macro_rules! opaque_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(Vec<u8>);

        impl $name {
            /// Accepts between 1 and [`MAX_OPAQUE_ID_LEN`] bytes. The store compares
            /// identifiers bytewise and never interprets them.
            pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, OpaqueIdError> {
                let bytes = bytes.into();
                match bytes.len() {
                    0 => Err(OpaqueIdError::Empty),
                    len if len > MAX_OPAQUE_ID_LEN => Err(OpaqueIdError::TooLong { len }),
                    _ => Ok(Self(bytes)),
                }
            }

            /// Returns the identifier bytes.
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }
    };
}

opaque_id!(
    /// Identifies a recovery source, such as one publication service.
    SourceId
);
opaque_id!(
    /// Identifies one publication revision of a source.
    RevisionId
);
opaque_id!(
    /// Identifies one resumable unit of retrieval work within a revision.
    PageId
);

/// Whether a publication revision can still be replaced by its publisher.
///
/// Sealed is not chain finality: a reorg invalidates affected sealed coverage too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PublicationStatus {
    /// The publisher will not replace this revision's contents.
    Sealed,
    /// A later revision may replace this one; its coverage must be revalidated, not extended.
    Provisional,
}

/// A block height and hash asserted by a publication.
///
/// Unlike [`ChainPoint`], this is not local chain evidence: it may lie beyond the wallet's
/// accepted chain, and a matching hash never substitutes for local acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicationAnchor {
    /// The asserted height.
    pub height: BlockHeight,
    /// The asserted block hash.
    pub hash: BlockHash,
}

/// The publication that supplied a commit's events and coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRevision {
    /// The recovery source.
    pub source: SourceId,
    /// The publication revision.
    pub revision: RevisionId,
    /// Whether the revision is sealed or provisional.
    pub status: PublicationStatus,
    /// The publication's asserted anchor, retained alongside each accepted endpoint.
    pub anchor: PublicationAnchor,
}

/// Immutable content of a transparent output paying a watched script.
///
/// Its identity is the transaction identifier plus output index. Coinbase classification
/// is explicit; a missing transaction index never makes an output non-coinbase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiveEvent {
    /// The received output.
    pub outpoint: OutPoint,
    /// The exact output script.
    pub script: Script,
    /// The output value.
    pub value: Zatoshis,
    /// Whether the output belongs to a coinbase transaction.
    pub is_coinbase: bool,
}

impl ReceiveEvent {
    /// Returns the receive identity: transaction identifier and output index.
    pub fn identity(&self) -> (TxId, u32) {
        (TxId::from_bytes(*self.outpoint.hash()), self.outpoint.n())
    }
}

/// Immutable content of a transaction input spending a watched output.
///
/// Its identity is the spending transaction identifier plus input index. The spent outpoint
/// is checked content, so two different outpoints claimed for one input are a contradiction.
/// A spend may arrive before its output and remains unresolved until the output arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendEvent {
    /// The spending transaction.
    pub spending_txid: TxId,
    /// The input index within the spending transaction.
    pub input_index: u32,
    /// The output consumed by this input.
    pub spent: OutPoint,
    /// The watched script of the consumed output, which identifies the owning account even
    /// when the spend arrives before its output. Checked against the output once it arrives.
    pub spent_script: Script,
}

impl SpendEvent {
    /// Returns the spend identity: spending transaction identifier and input index.
    pub fn identity(&self) -> (TxId, u32) {
        (self.spending_txid, self.input_index)
    }
}

/// An event with the block that mined it on the source's chain.
///
/// Placement is separate from event identity: re-mining after an accepted rewind changes
/// placement without making a different event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed<E> {
    /// The immutable event content.
    pub event: E,
    /// The block containing the event's transaction.
    pub mined: ChainPoint,
}

/// A checked, continuous interval for one watched script.
///
/// A checked interval with no events is still coverage. Coverage cannot span missing pages,
/// unsupported scripts, or ranges the source has not validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageInterval {
    /// The watched script this interval covers.
    pub script: Script,
    /// The first covered height, inclusive.
    pub from: BlockHeight,
    /// The accepted endpoint, inclusive.
    pub through: ChainPoint,
}

/// Why a source cannot cover a watched script or range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnsupportedReason {
    /// The source does not index this script type.
    ScriptType,
    /// The source has no history for the range, such as heights before its first publication.
    HistoryUnavailable,
}

/// A watched script range the commit's source explicitly cannot cover.
///
/// This is distinct from work not yet attempted: it blocks completeness for the owning
/// account until another source covers the range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedCoverage {
    /// The watched script.
    pub script: Script,
    /// The first unsupported height, inclusive.
    pub from: BlockHeight,
    /// The last unsupported height, inclusive; `None` extends through the commit target.
    pub to: Option<BlockHeight>,
    /// Why the source cannot cover the range.
    pub reason: UnsupportedReason,
}

/// Bounded resumable retrieval work, keyed by the commit's source revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPage {
    /// The page identity within the revision.
    pub page: PageId,
    /// The first height the page covers, inclusive.
    pub from: BlockHeight,
    /// The last height the page covers, inclusive.
    pub to: BlockHeight,
}

/// Pending-page progress recorded atomically with a commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingPageUpdate {
    /// Pages discovered but not yet fully retrieved.
    pub opened: Vec<PendingPage>,
    /// Previously opened pages whose contents this commit completes.
    pub completed: Vec<PageId>,
}

/// Whether a commit targets isolated candidate state or an activated account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LedgerLifecycle {
    /// Isolated recovery that cannot change balances, spends, locks, or address state.
    Candidate,
    /// Authoritative recovery for a promoted account, projected atomically.
    Active,
}

/// The watch-set generation an operation observed for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccountWatchGeneration<AccountId> {
    /// The account whose scripts were enumerated.
    pub account: AccountId,
    /// The account's watch-set generation at enumeration time.
    pub watch_generation: u64,
}

/// The immutable operation context a recovery run captured before any network I/O.
///
/// The store rechecks all of it inside the commit transaction. Stale context is retried
/// from a new snapshot, never committed under obsolete authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransparentLedgerContext<AccountId> {
    /// The mode the operation was authorized under.
    pub mode: TransparentLedgerMode,
    /// The applied policy generation at capture time.
    pub policy_generation: u64,
    /// The fixed accepted chain point the run recovers through.
    pub target: ChainPoint,
    /// Candidate or active destination; the store checks it against account lifecycle.
    pub lifecycle: LedgerLifecycle,
    /// The watch-set generation observed for each account in the run.
    pub accounts: Vec<AccountWatchGeneration<AccountId>>,
}

/// One normalized, atomically applied recovery result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransparentLedgerCommit<AccountId> {
    /// The context captured before retrieval.
    pub context: TransparentLedgerContext<AccountId>,
    /// The publication that supplied this result.
    pub source: SourceRevision,
    /// Validated receives. Only events through `context.target` may enter canonical state.
    pub receives: Vec<Placed<ReceiveEvent>>,
    /// Validated spends, including spends whose outputs have not yet arrived.
    pub spends: Vec<Placed<SpendEvent>>,
    /// Completed coverage, including checked intervals without events.
    pub coverage: Vec<CoverageInterval>,
    /// Ranges the source explicitly cannot cover.
    pub unsupported: Vec<UnsupportedCoverage>,
    /// Resumable page progress.
    pub pages: PendingPageUpdate,
}

/// Counts of what a successful commit recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitSummary {
    /// Receives recorded or confirmed idempotently.
    pub receives: usize,
    /// Spends recorded or confirmed idempotently.
    pub spends: usize,
    /// Coverage intervals recorded.
    pub coverage_intervals: usize,
}

/// Why a commit was not applied. A rejected commit writes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommitRejection {
    /// The store cannot accept ledger commits in its current state.
    Unavailable,
    /// The context's mode differs from the handle's configured mode.
    ModeMismatch,
    /// The applied policy changed after the context was captured.
    StalePolicy,
    /// Account, watch-set, lifecycle, or chain state changed after capture.
    StaleContext,
    /// The commit contradicts accepted content or placement; trust in the session ends.
    Integrity,
}

/// The result of submitting a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Events, coverage, and progress were recorded together.
    Committed(CommitSummary),
    /// Nothing was recorded.
    Rejected(CommitRejection),
}

/// The context a caller expects when requesting promotion of one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromotionContext<AccountId> {
    /// The account to promote.
    pub account: AccountId,
    /// The applied policy generation the caller observed.
    pub policy_generation: u64,
    /// The account's watch-set generation the caller observed.
    pub watch_generation: u64,
    /// The accepted decision point coverage must reach.
    pub decision_point: ChainPoint,
}

/// Why promotion did not occur. A rejected promotion changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromotionRejection {
    /// The store cannot promote accounts in its current state.
    Unavailable,
    /// The applied policy changed after the context was captured.
    StalePolicy,
    /// Account, watch-set, or chain state changed after capture.
    StaleContext,
    /// Recovery is incomplete for the listed reasons.
    NotReady(Vec<RecoveryBlocker>),
}

/// The result of requesting promotion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// The account's private ledger became authoritative through `through`.
    Promoted {
        /// The accepted point through which the promoted account is covered.
        through: ChainPoint,
    },
    /// Nothing changed.
    Rejected(PromotionRejection),
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
    /// A previously authorized private ledger state.
    Ledger,
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

/// The net of currently recovered candidate events.
///
/// This is unverified until coverage is complete. It can overstate or understate the true
/// balance and is not a lower bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveredNet {
    /// The unverified net value of recovered unspent receives.
    pub value: Zatoshis,
}

/// Financial recovery progress for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryCompletion {
    /// Public authority applies; private recovery completion is not required.
    NotApplicable,
    /// Private recovery has not started.
    NotStarted,
    /// Private recovery is running and has not covered the watch set.
    InProgress,
    /// The watch set is covered through the snapshot's decision point.
    Complete,
    /// Recovery cannot progress until the listed blockers clear.
    Blocked,
}

/// A reason financial recovery is incomplete. Display-only history gaps are not blockers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryBlocker {
    /// This build or configuration cannot perform private recovery.
    PrivateRecoveryUnavailable,
    /// No accepted chain point is known locally.
    ChainUnknown,
    /// The publication has not reached the accepted chain point.
    PublicationLag,
    /// Retrieval work remains outstanding.
    PendingPages,
    /// Spends were recovered before their outputs.
    UnresolvedSpends,
    /// Some watched script or history is unsupported by the source.
    UnsupportedHistory,
    /// Recovered content contradicted accepted state.
    IntegrityFailure,
    /// This build cannot read the wallet's transparent state.
    TransparentSupportUnavailable,
}

/// Counts that explain an account's recovery state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryDiagnostics {
    /// Outstanding pending pages relevant to the account.
    pub pending_pages: u64,
    /// Recovered spends whose outputs are not yet known.
    pub unresolved_spends: u64,
    /// Watched scripts the source cannot cover.
    pub unsupported_scripts: u64,
}

/// The single atomic balance-and-recovery result for one account's transparent funds.
///
/// Every field comes from one database read. Unavailable is not zero: an absent
/// `authorized` balance means no current authority, never an empty wallet. This describes
/// transparent financial recovery only, not whole-wallet history completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransparentLedgerSnapshot<AccountId> {
    /// The account described.
    pub account: AccountId,
    /// The handle's configured mode.
    pub mode: TransparentLedgerMode,
    /// The current source of financial authority.
    pub authority: TransparentAuthority,
    /// The accepted chain point captured for recovery, when one exists.
    pub target: Option<ChainPoint>,
    /// Continuous coverage of the account's current watch set.
    pub covered_through: Option<ChainPoint>,
    /// Continuous coverage from sealed publications only.
    pub settled_through: Option<ChainPoint>,
    /// The spendable-authority balance; absent when authority cannot be established.
    pub authorized: Option<TransparentLedgerBalance>,
    /// A prior amount shown for context; never current or spendable.
    pub last_known: Option<LastKnownBalance>,
    /// Unverified net of recovered candidate events.
    pub recovered_net: Option<RecoveredNet>,
    /// Financial recovery progress.
    pub completion: RecoveryCompletion,
    /// Reasons recovery is incomplete.
    pub blockers: Vec<RecoveryBlocker>,
    /// Counts explaining the recovery state.
    pub diagnostics: RecoveryDiagnostics,
}

/// Where a watched script's history must be recovered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryStart {
    /// A justified conservative lower bound. It may move earlier, never later.
    From(BlockHeight),
    /// The start is unknown; recovery must begin at genesis.
    Unknown,
}

/// A script watched by transparent ledger recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedScript<AccountId> {
    /// The owning account.
    pub account: AccountId,
    /// The exact output script.
    pub script: Script,
    /// Where the script's history must be recovered from.
    pub required_from: RecoveryStart,
}

/// The watched scripts, with the generations a recovery run must capture in its context.
///
/// An account listed with no scripts has none enumerated; that is not evidence of an empty
/// history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedScriptSnapshot<AccountId> {
    /// The applied policy generation.
    pub policy_generation: u64,
    /// Each account's current watch-set generation.
    pub accounts: Vec<AccountWatchGeneration<AccountId>>,
    /// The watched scripts across all listed accounts.
    pub scripts: Vec<WatchedScript<AccountId>>,
}

/// Reads transparent ledger configuration and recovery state.
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

    /// Returns the transparent balance-and-recovery snapshot for `account`, from one read.
    ///
    /// `confirmations_policy` applies the existing confirmation rules to any authorized or
    /// last-known balance. A historical snapshot never authorizes a current spend.
    fn transparent_ledger_snapshot(
        &self,
        account: Self::AccountId,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<TransparentLedgerSnapshot<Self::AccountId>, Self::Error>;

    /// Returns the watched scripts with ownership, recovery bounds, and the policy and
    /// watch-set generations to place in a [`TransparentLedgerContext`], from one read.
    fn transparent_ledger_watched_scripts(
        &self,
    ) -> Result<WatchedScriptSnapshot<Self::AccountId>, Self::Error>;
}

/// Applies recovery results and guarded account promotion.
///
/// Recovery sources cannot mark results authoritative; the store checks account lifecycle
/// and current policy. Domain rejections are returned in `Ok`; only storage failures are
/// errors. There is no standalone ledger rewind: rollback accompanies wallet chain rewinds.
pub trait TransparentLedgerWrite: TransparentLedgerRead + WalletWrite {
    /// Records `commit` atomically after rechecking its operation context, or records nothing.
    fn apply_transparent_ledger_commit(
        &mut self,
        commit: TransparentLedgerCommit<<Self as WalletRead>::AccountId>,
    ) -> Result<CommitOutcome, <Self as WalletRead>::Error>;

    /// Promotes one account's recovered ledger to financial authority in one transaction,
    /// after rechecking coverage, anchors, pending work, and `context`; or changes nothing.
    fn promote_transparent_ledger_account(
        &mut self,
        context: PromotionContext<<Self as WalletRead>::AccountId>,
    ) -> Result<PromotionOutcome, <Self as WalletRead>::Error>;
}

#[cfg(test)]
mod tests {
    use transparent::bundle::OutPoint;
    use zcash_primitives::transaction::TxId;

    use super::{
        MAX_OPAQUE_ID_LEN, OpaqueIdError, PageId, ReceiveEvent, RevisionId, SourceId, SpendEvent,
        TransparentLedgerMode,
    };

    #[test]
    fn opaque_ids_are_bounded_and_nonempty() {
        assert_eq!(SourceId::new(Vec::new()), Err(OpaqueIdError::Empty));
        assert_eq!(
            RevisionId::new(vec![7; MAX_OPAQUE_ID_LEN + 1]),
            Err(OpaqueIdError::TooLong {
                len: MAX_OPAQUE_ID_LEN + 1
            })
        );
        let page = PageId::new(vec![7; MAX_OPAQUE_ID_LEN]).unwrap();
        assert_eq!(page.as_bytes(), &[7; MAX_OPAQUE_ID_LEN][..]);
    }

    #[test]
    fn only_private_required_drops_public_authority() {
        assert!(TransparentLedgerMode::Public.retains_public_authority());
        assert!(TransparentLedgerMode::PrivateShadow.retains_public_authority());
        assert!(!TransparentLedgerMode::PrivateRequired.retains_public_authority());
    }

    #[test]
    fn event_identities_exclude_checked_content() {
        let outpoint = OutPoint::new([1; 32], 3);
        let receive = ReceiveEvent {
            outpoint: outpoint.clone(),
            script: Default::default(),
            value: zcash_protocol::value::Zatoshis::const_from_u64(5),
            is_coinbase: false,
        };
        assert_eq!(receive.identity(), (TxId::from_bytes([1; 32]), 3));

        let spend = SpendEvent {
            spending_txid: TxId::from_bytes([2; 32]),
            input_index: 4,
            spent: outpoint,
            spent_script: Default::default(),
        };
        let conflicting = SpendEvent {
            spent: OutPoint::new([9; 32], 0),
            ..spend.clone()
        };
        assert_eq!(spend.identity(), conflicting.identity());
        assert_ne!(spend, conflicting);
    }
}
