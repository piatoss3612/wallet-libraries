use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};
use transparent::{address::TransparentAddress, bundle::OutPoint};
use transparent_events::{FeeState, TransparentEvent};
use transparent_filter::{MAINNET_GENESIS_DISPLAY, NETWORK, ShardMap, ShardMapEntry};
use transparent_wallet::transport::{FilterSource, ShardTransport};
use transparent_wallet::{
    Acceptance, Anchor, ChainView, Completion, IncompleteReason, ScriptEntry, ScriptOrigin,
    StaticScripts, SyncReport, WalletStore, WorkLimits,
};
use transparent_wallet_store::SqliteStore;
use zcash_client_backend::data_api::transparent_ledger::{
    AddressRange, ChainPoint, PageRequest, PublicationAnchor, ReceiveEvent, RecoveryRevision,
    SpendEvent, TransactionMetadata, TransparentLedgerCommit, TransparentWatchSet,
    WholeTransactionFee,
};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};

/// The only shard schema this adapter reads.
///
/// Every companion is bound to it, and a pass refuses a service whose init
/// names another schema before requesting any manifest or private query.
pub const SCHEMA: &str = "transparent-shard-v11";

/// Identity and finite resource bounds for one account's recovery passes.
///
/// The caller supplies the transports for each pass; nothing here is dialed.
#[derive(Clone, Debug)]
pub struct RecoveryConfig {
    /// Caller-chosen source identity, independent of a server's assertions.
    pub source: Vec<u8>,
    /// Stable wallet/account identity; use a different companion store per account.
    pub account_binding: Vec<u8>,
    /// Identity label bound into the companion: the origin whose publication the
    /// caller's transports retrieve. Transports are the caller's; this is not dialed.
    pub origin: String,
    /// Maximum watched scripts and retained companion scripts.
    pub scripts: usize,
    /// Maximum shard-map entries per pass.
    pub shards: usize,
    /// Maximum candidate event records exported per pass.
    pub events: usize,
    /// Private query budget. Exhaustion retains continuation and reports [`Outcome::More`].
    pub queries: u64,
    /// Private payload budget, including setup. One atomic response may cross it.
    pub private_bytes: u64,
}

/// Why a pass stopped. Only [`Outcome::Complete`] covers the whole target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every watched script is covered from its required height through the target.
    Complete,
    /// The publication ends below the target. The pass may still have covered
    /// through the publication's end; a later pass can finish once it catches up.
    Behind,
    /// A query, byte or pending-page budget stopped the pass. The companion keeps
    /// the continuation, so the next pass resumes.
    More,
    /// The service refused for capacity throughout its retry budget.
    Overloaded,
    /// Progress needs more than a retry: an unknown chain block, spends the
    /// watch set cannot resolve, or script discovery past its bound.
    Stalled,
}

/// How far a pass covered the watch set, independent of the commits it returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Height through which every watched script is covered from its required
    /// height, provisional tail coverage included.
    pub covered_through: u64,
    /// Why the pass stopped.
    pub outcome: Outcome,
}

/// Maps the reference client's report onto the adapter's narrower contract.
///
/// A `clamped` pass synced to the publication's end below the wallet's target,
/// so even its completion is [`Outcome::Behind`].
fn progress(report: &SyncReport, clamped: bool) -> Progress {
    let outcome = match &report.completion {
        Completion::Complete if clamped => Outcome::Behind,
        Completion::Complete => Outcome::Complete,
        Completion::Incomplete { reason, .. } => match reason {
            IncompleteReason::QueryBudget
            | IncompleteReason::ByteBudget
            | IncompleteReason::PendingLimit => Outcome::More,
            IncompleteReason::PublicationBehind { .. } => Outcome::Behind,
            IncompleteReason::Overloaded { .. } => Outcome::Overloaded,
            IncompleteReason::ChainUnknown { .. }
            | IncompleteReason::UnresolvedSpends
            | IncompleteReason::DiscoveryUnbounded => Outcome::Stalled,
        },
    };
    Progress {
        covered_through: report.covered_through,
        outcome,
    }
}

/// A failed pass grants no candidate progress or source authority.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// Configuration, accepted-chain, context or source-content failure.
    #[error("transparent PIR recovery: {0}")]
    Invalid(String),
    /// Reference retrieval or companion persistence failure.
    #[error("transparent PIR recovery: {0}")]
    Failure(String),
}
fn failure(error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::Failure(error.to_string())
}
fn require(ok: bool, message: &str) -> Result<(), RecoveryError> {
    if ok {
        Ok(())
    } else {
        Err(RecoveryError::Invalid(message.into()))
    }
}

/// Candidate observations and reference progress. Apply through the existing wallet writer.
///
/// Neither the batch nor a server withdrawal is authority to change local facts
/// or promote an account.
pub struct RecoveryBatch<AccountId> {
    /// Source-bound normalized commits, including unfinished page work.
    pub commits: Vec<TransparentLedgerCommit<AccountId>>,
    /// How far this pass covered the watch set, and why it stopped. Anything but
    /// [`Outcome::Complete`] is never synchronized.
    pub progress: Progress,
    /// Revisions previously exported but no longer named by the current map.
    /// The companion retains them until a batch is acknowledged.
    reconciliation: Vec<RecoveryRevision>,
    token: [u8; 32],
}

/// Durable reference retrieval and normalization for one wallet/account.
///
/// The companion store retains cache and page continuation. It is deliberately
/// separate from wallet financial state, whose writer independently validates each
/// normalized commit. No method here changes qualification, activation or spending.
pub struct ReferenceRecovery {
    config: RecoveryConfig,
    store: SqliteStore,
    catalog: Connection,
    pending_export: Option<[u8; 32]>,
}

fn address_script(address: TransparentAddress) -> Vec<u8> {
    match address {
        TransparentAddress::PublicKeyHash(hash) => {
            [vec![0x76, 0xa9, 20], hash.to_vec(), vec![0x88, 0xac]].concat()
        }
        TransparentAddress::ScriptHash(hash) => {
            [vec![0xa9, 20], hash.to_vec(), vec![0x87]].concat()
        }
    }
}
fn block(height: u64) -> Result<BlockHeight, RecoveryError> {
    Ok(BlockHeight::from(u32::try_from(height).map_err(failure)?))
}
pub(crate) fn block_hash(display: &str) -> Result<BlockHash, RecoveryError> {
    let mut bytes = hex::decode(display).map_err(failure)?;
    require(bytes.len() == 32, "invalid publication block hash")?;
    bytes.reverse();
    Ok(BlockHash::from_slice(&bytes))
}
fn metadata(
    meta: Option<transparent_events::TransactionMetadata>,
) -> Result<Option<TransactionMetadata>, RecoveryError> {
    meta.map(|meta| {
        Ok(TransactionMetadata {
            fee: match meta.fee {
                FeeState::Exact(value) => {
                    WholeTransactionFee::Exact(Zatoshis::from_u64(value).map_err(failure)?)
                }
                FeeState::Unknown => WholeTransactionFee::Unknown,
                FeeState::NotApplicable => WholeTransactionFee::NotApplicable,
            },
            transparent_input_count: meta.transparent_input_count,
            has_shielded_components: meta.has_shielded_components,
        })
    })
    .transpose()
}
fn page_id(shard: u64, digest: &str, script: &[u8], first: u32) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(b"transparent-reference-page-v1");
    hash.update(shard.to_le_bytes());
    hash.update(digest.as_bytes());
    hash.update(script);
    hash.update(first.to_le_bytes());
    hash.finalize().to_vec()
}
/// The companion's binding: length-prefixed source, account, origin and schema.
fn binding(parts: [&[u8]; 4]) -> Vec<u8> {
    let mut hash = Sha256::new();
    for value in parts {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    hash.finalize().to_vec()
}

/// The lowest height a pass needs the wallet's chain for: the lowest required
/// height in `watch`, capped at `target`, or 0 when nothing is watched.
fn required_floor<A>(watch: &TransparentWatchSet<A>, target: u64) -> u64 {
    watch
        .addresses
        .iter()
        .map(|entry| u64::from(u32::from(entry.required_from)))
        .min()
        .map_or(0, |lowest| lowest.min(target))
}

/// The caller's chain, accepting every block below the watch set's floor. Passed
/// to `sync_into` only.
///
/// A wallet holds no blocks below its birthday, while a publication may start far
/// below it (the live map starts at genesis). Leniency there is sound because, at
/// wallet-pir 648264bb, `sync_into` asks below the floor only for rollback
/// anchors:
///
/// - It plans only shards meeting `[required_from, target]` (`sync.rs:870-895`),
///   so every coverage endpoint it checks (`sync.rs:1208-1223`, `:2032-2048`,
///   `sync_ahead.rs:255-258`) and every stored coverage end its reorg scan asks
///   about (`sync.rs:609-662`) is at or above the floor. Required heights only
///   move earlier, so no script the companion retains starts below the floor.
/// - The block a rollback rewinds to may lie below it: the reorg fallback
///   `map.start_height - 1` (`:648`), a replaced tail (`:734-741`) or a revision
///   withdrawn mid-sync (`:1030-1035`), each just below a shard's start and each
///   resolved by `accepted_at` (`:2370-2390`), which takes the hash from the map
///   when the view has none. Only block 0 has no map entry, so [`Self::hash_at`]
///   answers it with the map's genesis hash.
///
/// Nothing below the floor is exported or cataloged (see `normalize`), and the
/// target and every shard anchor are checked against the caller's chain alone.
/// Re-verify these call sites on every wallet-pir pin bump.
struct BelowFloor<'a, C> {
    chain: &'a C,
    /// The floor: every block below it is accepted.
    below: u64,
    /// The map's genesis hash, already checked to be mainnet's.
    genesis: &'a str,
}

impl<C: ChainView> ChainView for BelowFloor<'_, C> {
    fn is_accepted(&self, height: u64, hash_display_hex: &str) -> Acceptance {
        if height < self.below {
            Acceptance::Accepted
        } else {
            self.chain.is_accepted(height, hash_display_hex)
        }
    }

    fn tip(&self) -> Option<Anchor> {
        self.chain.tip()
    }

    fn hash_at(&self, height: u64) -> Option<String> {
        if height < self.below {
            (height == 0).then(|| self.genesis.to_owned())
        } else {
            self.chain.hash_at(height)
        }
    }
}

/// The anchor a pass syncs to, and whether it was clamped below `target`.
///
/// When the publication ends at `t` below the wallet's target, a pass to the
/// target can only report the publication behind. It syncs to the map's last end
/// instead, and so covers everything published, only when all of these hold:
/// `t` is at or above the `floor`; neither the companion's `stored` anchor nor a
/// `retained` event lies above `t`, so the client neither refuses a regressed
/// anchor nor a target below retained events (a lagging replica); and `chain`
/// accepts the map's terminal block at `t`. Otherwise it passes the target
/// unchanged.
fn sync_target(
    map: &ShardMap,
    target: &Anchor,
    floor: u64,
    stored: Option<&Anchor>,
    retained: u64,
    chain: &impl ChainView,
) -> (Anchor, bool) {
    let Some(last) = map.shards.last() else {
        return (target.clone(), false);
    };
    let t = last.end_height;
    if t < target.height
        && t >= floor
        && stored.is_none_or(|anchor| t >= anchor.height)
        && t >= retained
        && chain.is_accepted(t, &last.terminal_block_hash) == Acceptance::Accepted
    {
        let clamped = Anchor {
            height: t,
            hash: last.terminal_block_hash.clone(),
        };
        (clamped, true)
    } else {
        (target.clone(), false)
    }
}

impl ReferenceRecovery {
    /// Open a compatible companion store and fence its account, origin and schema binding.
    pub fn open(path: impl AsRef<Path>, config: RecoveryConfig) -> Result<Self, RecoveryError> {
        require(
            !config.source.is_empty()
                && config.source.len() <= 256
                && !config.account_binding.is_empty()
                && config.account_binding.len() <= 256,
            "source/account binding must be nonempty and at most 256 bytes",
        )?;
        require(
            config.scripts > 0
                && config.shards > 0
                && config.events > 0
                && config.queries > 0
                && config.private_bytes > 0,
            "recovery bounds must be positive",
        )?;
        let url = url::Url::parse(&config.origin).map_err(failure)?;
        require(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid explicit HTTP origin",
        )?;
        let store = SqliteStore::open(&path).map_err(failure)?;
        let mut catalog = Connection::open(path).map_err(failure)?;
        catalog
            .busy_timeout(Duration::from_secs(5))
            .map_err(failure)?;
        catalog.execute_batch("CREATE TABLE IF NOT EXISTS pir_bridge_binding (key TEXT PRIMARY KEY, value BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS pir_bridge_revisions (source BLOB NOT NULL CHECK(length(source)=32), revision BLOB NOT NULL CHECK(length(revision)=32),
                lineage INTEGER NOT NULL CHECK(lineage>0), sealed INTEGER NOT NULL, height INTEGER NOT NULL CHECK(height>=0 AND height<=4294967295), hash BLOB NOT NULL CHECK(length(hash)=32),
                exported INTEGER NOT NULL DEFAULT 0, current INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(source,revision), UNIQUE(source,lineage));").map_err(failure)?;
        let tx = catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        let binding = binding([
            &config.source,
            &config.account_binding,
            config.origin.as_bytes(),
            SCHEMA.as_bytes(),
        ]);
        let prior: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM pir_bridge_binding WHERE key='account-source'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(failure)?;
        require(
            prior.as_ref().is_none_or(|value| *value == binding),
            "companion account/source/origin/schema binding mismatch",
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO pir_bridge_binding VALUES ('account-source',?1)",
            [binding],
        )
        .map_err(failure)?;
        tx.commit().map_err(failure)?;
        Ok(Self {
            config,
            store,
            catalog,
            pending_export: None,
        })
    }

    fn revision(
        &mut self,
        identity: &[u8],
        entry: &ShardMapEntry,
    ) -> Result<RecoveryRevision, RecoveryError> {
        let mut hash = Sha256::new();
        hash.update(b"transparent-reference-source-v1");
        hash.update(&self.config.source);
        hash.update(identity);
        hash.update(entry.shard_id.to_le_bytes());
        let source = hash.finalize().to_vec();
        let mut hash = Sha256::new();
        hash.update(entry.manifest_digest.as_bytes());
        hash.update([u8::from(entry.sealed)]);
        let revision = hash.finalize().to_vec();
        let height = block(entry.end_height)?;
        let block_hash = block_hash(&entry.terminal_block_hash)?;
        let tx = self
            .catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        let existing: Option<(u64, bool, u32, Vec<u8>)> = tx.query_row(
            "SELECT lineage,sealed,height,hash FROM pir_bridge_revisions WHERE source=?1 AND revision=?2",
            params![source, revision], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional().map_err(failure)?;
        let lineage = if let Some((lineage, sealed, known_height, known_hash)) = existing {
            require(
                sealed == entry.sealed
                    && known_height == u32::from(height)
                    && known_hash == block_hash.0,
                "revision identity contradicts stored publication",
            )?;
            lineage
        } else {
            let count: u64 = tx
                .query_row("SELECT COUNT(*) FROM pir_bridge_revisions", [], |r| {
                    r.get(0)
                })
                .map_err(failure)?;
            require(count < 65536, "retained revision catalog limit reached")?;
            let prior: u64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(lineage),0) FROM pir_bridge_revisions WHERE source=?1",
                    [&source],
                    |r| r.get(0),
                )
                .map_err(failure)?;
            let lineage = prior
                .checked_add(1)
                .ok_or_else(|| RecoveryError::Invalid("lineage overflow".into()))?;
            require(lineage <= i64::MAX as u64, "lineage overflow")?;
            tx.execute("INSERT INTO pir_bridge_revisions(source,revision,lineage,sealed,height,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                params![source,revision,lineage,entry.sealed,u32::from(height),block_hash.0.as_slice()]).map_err(failure)?;
            lineage
        };
        tx.execute(
            "UPDATE pir_bridge_revisions SET current=1 WHERE source=?1 AND revision=?2",
            params![source, revision],
        )
        .map_err(failure)?;
        tx.commit().map_err(failure)?;
        Ok(RecoveryRevision {
            source,
            revision,
            lineage,
            sealed: entry.sealed,
            publication: PublicationAnchor {
                height,
                hash: block_hash,
            },
        })
    }

    /// Recover a finite pass through the caller's transports and independently
    /// accepted chain.
    ///
    /// `filters` and `transport` must reach the origin bound into this companion.
    /// Before any filter or private retrieval, in order: the watch set's target
    /// must be accepted by `chain`; the watch set and the companion's retained
    /// scripts must be within the script limit; `filters` must not use the parent
    /// filter experiment; the shard map must be within the shard limit and name
    /// Zcash mainnet's network and genesis block; and the service's init must name
    /// [`SCHEMA`]. A failed check returns [`RecoveryError::Invalid`] without
    /// further requests. A watch set with no addresses needs nothing retrieved:
    /// once its target is accepted, the pass sends no request and returns a batch
    /// with no commits, [`Outcome::Complete`] at the target.
    ///
    /// `chain` answers for every block at or above the watch set's floor, its
    /// lowest required height; below it the pass needs no wallet hashes and
    /// exports nothing. When the publication ends below the target, the pass
    /// syncs to the publication's end if `chain` accepts it and the companion
    /// holds nothing above it; completing there reports [`Outcome::Behind`].
    /// Otherwise the pass is [`Outcome::Behind`] at once. Commits keep the watch
    /// set's context either way.
    pub fn recover<A, C, F, T>(
        &mut self,
        watch: &TransparentWatchSet<A>,
        chain: &C,
        filters: &mut F,
        transport: &mut T,
    ) -> Result<RecoveryBatch<A>, RecoveryError>
    where
        A: Copy + std::fmt::Debug,
        C: ChainView,
        F: FilterSource,
        T: ShardTransport,
    {
        self.pending_export = None;
        let context = watch
            .context()
            .ok_or_else(|| RecoveryError::Invalid("no locally accepted recovery target".into()))?;
        let target = Anchor {
            height: u64::from(u32::from(context.target.height)),
            hash: context.target.hash.to_string(),
        };
        require(
            chain.is_accepted(target.height, &target.hash) == Acceptance::Accepted,
            "target is not independently accepted",
        )?;
        if watch.addresses.is_empty() {
            // Every watched script is covered, vacuously. A sync would need the
            // wallet's hash for every shard from the map's start, since nothing
            // raises the floor, and the reference client reports a sync with no
            // scripts as unbounded discovery.
            return self.empty(Progress {
                covered_through: target.height,
                outcome: Outcome::Complete,
            });
        }
        require(
            watch.addresses.len() <= self.config.scripts
                && self.store.scripts().map_err(failure)?.len() <= self.config.scripts,
            "script limit exceeded",
        )?;
        let addresses: BTreeMap<Vec<u8>, _> = watch
            .addresses
            .iter()
            .map(|entry| (address_script(entry.address), entry))
            .collect();
        require(
            addresses.len() == watch.addresses.len(),
            "duplicate watch address",
        )?;
        // The parent experiment's selective child requests leak coarse activity.
        require(
            !filters.uses_parents(),
            "parent filter sources are not supported",
        )?;
        let (raw_map, map_bytes) = filters.shard_map().map_err(failure)?;
        let map: ShardMap = serde_json::from_slice(&raw_map).map_err(failure)?;
        require(
            map.shards.len() <= self.config.shards,
            "publication shard limit exceeded",
        )?;
        // The wallet's chain is mainnet's; another chain's map cannot cover it.
        require(
            map.network == NETWORK && map.genesis_hash == MAINNET_GENESIS_DISPLAY,
            "publication is not for Zcash mainnet",
        )?;
        let (raw_init, _) = transport.init().map_err(failure)?;
        let geometry = transparent_wallet::parse_init(&raw_init).map_err(failure)?;
        require(
            geometry.schema == SCHEMA,
            "service serves an unsupported shard schema",
        )?;
        let floor = required_floor(watch, target.height);
        let stored = self.store.anchor().map_err(failure)?;
        let retained = self
            .store
            .events()
            .map_err(failure)?
            .iter()
            .map(|held| u64::from(held.event.height()))
            .max()
            .unwrap_or(0);
        let (sync_anchor, clamped) =
            sync_target(&map, &target, floor, stored.as_ref(), retained, chain);
        let below_floor = BelowFloor {
            chain,
            below: floor,
            genesis: &map.genesis_hash,
        };
        let mut scripts = StaticScripts(
            watch
                .addresses
                .iter()
                .map(|entry| ScriptEntry {
                    script: address_script(entry.address),
                    origin: ScriptOrigin::Derived,
                    required_from: u64::from(u32::from(entry.required_from)),
                })
                .collect(),
        );
        let limits = WorkLimits {
            max_queries: Some(self.config.queries),
            max_private_bytes: Some(self.config.private_bytes),
        };
        let report = transparent_wallet::sync_into(
            &mut self.store,
            &map,
            map_bytes,
            &geometry,
            &below_floor,
            &mut scripts,
            filters,
            transport,
            &limits,
            &sync_anchor,
        )
        .map_err(failure)?;
        // A refresh may have replaced the first map. Export only provenance still
        // named in an independently validated current map; stale facts fail closed.
        let (raw_map, _) = filters.shard_map().map_err(failure)?;
        let current_map: ShardMap = serde_json::from_slice(&raw_map).map_err(failure)?;
        require(
            current_map.shards.len() <= self.config.shards,
            "publication shard limit exceeded",
        )?;
        self.normalize(watch, current_map, progress(&report, clamped), chain)
    }

    fn normalize<A: Copy>(
        &mut self,
        watch: &TransparentWatchSet<A>,
        map: ShardMap,
        progress: Progress,
        chain: &impl ChainView,
    ) -> Result<RecoveryBatch<A>, RecoveryError> {
        let context = watch.context().expect("validated before retrieval");
        let Some(identity) = self.store.set_identity().map_err(failure)? else {
            // The client stopped before binding this companion to a publication
            // (one behind the target, or an unknown target), so it holds no facts.
            return self.empty(progress);
        };
        map.check_shape().map_err(failure)?;
        require(
            identity.continues(&transparent_wallet::SetIdentity::of_schema(&map, SCHEMA)),
            "refreshed publication lineage diverged",
        )?;
        let prior: Option<Vec<u8>> = self
            .catalog
            .query_row(
                "SELECT value FROM pir_bridge_binding WHERE key='lineage'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(failure)?;
        let identity = prior.unwrap_or_else(|| identity.digest().into_bytes());
        self.catalog
            .execute(
                "INSERT OR IGNORE INTO pir_bridge_binding VALUES ('lineage',?1)",
                [&identity],
            )
            .map_err(failure)?;
        self.catalog
            .execute("UPDATE pir_bridge_revisions SET current=0", [])
            .map_err(failure)?;
        let target = u64::from(u32::from(context.target.height));
        let floor = required_floor(watch, target);
        let mut commits = BTreeMap::new();
        for entry in &map.shards {
            // Only shards meeting [floor, target]. One ending below the floor holds
            // nothing the watch set requires, and the wallet holds no hash for it.
            if entry.start_height > target || entry.end_height < floor {
                continue;
            }
            let height = entry.end_height.min(target);
            let hash = chain
                .hash_at(height)
                .ok_or_else(|| RecoveryError::Invalid("missing independent shard anchor".into()))?;
            require(
                chain.is_accepted(height, &hash) == Acceptance::Accepted
                    && (height != entry.end_height || hash == entry.terminal_block_hash),
                "publication anchor disagrees with accepted chain",
            )?;
            let anchor = ChainPoint {
                height: block(height)?,
                hash: block_hash(&hash)?,
            };
            let revision = self.revision(&identity, entry)?;
            commits.insert(
                entry.shard_id,
                TransparentLedgerCommit {
                    context,
                    revision,
                    anchor,
                    receives: vec![],
                    spends: vec![],
                    coverage: vec![],
                    unsupported: vec![],
                    opened_pages: vec![],
                    completed_pages: vec![],
                },
            );
        }
        let addresses: BTreeMap<_, _> = watch
            .addresses
            .iter()
            .map(|entry| (address_script(entry.address), entry))
            .collect();
        let events = self.store.events().map_err(failure)?;
        require(
            events.len() <= self.config.events,
            "candidate event export limit exceeded",
        )?;
        for stored in events {
            let Some(address) = addresses.get(&stored.script) else {
                continue;
            };
            let height = u64::from(stored.event.height());
            if height < u64::from(u32::from(address.required_from)) || height > target {
                continue;
            }
            let commit = commits.get_mut(&stored.shard_id).ok_or_else(|| {
                RecoveryError::Invalid("stored event absent from current map".into())
            })?;
            let entry = map
                .shards
                .iter()
                .find(|entry| entry.shard_id == stored.shard_id)
                .unwrap();
            require(
                stored.revision_digest == entry.manifest_digest,
                "stored event revision withdrawn; retrieve again",
            )?;
            match stored.event {
                TransparentEvent::Receive(event) => commit.receives.push(ReceiveEvent {
                    metadata: metadata(event.metadata)?,
                    outpoint: OutPoint::new(event.txid.0, event.output_index),
                    address: address.address,
                    value: Zatoshis::from_u64(event.value).map_err(failure)?,
                    coinbase: event.coinbase,
                    mined_height: BlockHeight::from(event.height),
                }),
                TransparentEvent::Spend(event) => commit.spends.push(SpendEvent {
                    metadata: metadata(event.metadata)?,
                    spending_txid: TxId::from_bytes(event.spending_txid.0),
                    input_index: event.input_index,
                    prevout: OutPoint::new(event.spent_txid.0, event.spent_output_index),
                    prevout_address: address.address,
                    mined_height: BlockHeight::from(event.height),
                }),
            }
        }
        for (script, address) in &addresses {
            for range in self.store.coverage(script).map_err(failure)? {
                let Some(commit) = commits.get_mut(&range.shard_id) else {
                    continue;
                };
                let entry = map
                    .shards
                    .iter()
                    .find(|entry| entry.shard_id == range.shard_id)
                    .unwrap();
                require(
                    range.revision_digest == entry.manifest_digest,
                    "coverage revision withdrawn; retrieve again",
                )?;
                let from = range
                    .start_height
                    .max(u64::from(u32::from(address.required_from)));
                let through = range.end_height.min(target);
                if from <= through {
                    commit.coverage.push(AddressRange {
                        address: address.address,
                        from: block(from)?,
                        through: block(through)?,
                    });
                }
            }
        }
        let mut pending = BTreeSet::new();
        for page in self.store.pending().map_err(failure)? {
            let Some(address) = addresses.get(&page.script) else {
                continue;
            };
            let entry = map
                .shards
                .iter()
                .find(|entry| entry.shard_id == page.shard_id)
                .ok_or_else(|| RecoveryError::Invalid("pending shard withdrawn".into()))?;
            require(
                page.revision_digest == entry.manifest_digest,
                "pending revision withdrawn",
            )?;
            let id = page_id(
                page.shard_id,
                &page.revision_digest,
                &page.script,
                page.first_page,
            );
            let from = entry
                .start_height
                .max(u64::from(u32::from(address.required_from)));
            let through = entry.end_height.min(target);
            if from <= through {
                commits
                    .get_mut(&page.shard_id)
                    .unwrap()
                    .opened_pages
                    .push(PageRequest {
                        page: id.clone(),
                        addresses: vec![address.address],
                        from: block(from)?,
                        through: block(through)?,
                    });
                pending.insert(id);
            }
        }
        for page in &watch.pending_pages {
            for commit in commits.values_mut() {
                if page.revision == commit.revision && !pending.contains(&page.request.page) {
                    // Completion needs coverage, not merely absence of pending work.
                    if page.request.addresses.iter().all(|address| {
                        commit.coverage.iter().any(|range| {
                            range.address == *address
                                && range.from <= page.request.from
                                && range.through >= page.request.through
                        })
                    }) {
                        commit.completed_pages.push(page.request.page.clone());
                    }
                }
            }
        }
        let mut statement = self.catalog.prepare("SELECT source,revision,lineage,sealed,height,hash FROM pir_bridge_revisions WHERE exported=1 AND current=0 ORDER BY source,lineage").map_err(failure)?;
        let reconciliation = statement
            .query_map([], |row| {
                Ok(RecoveryRevision {
                    source: row.get(0)?,
                    revision: row.get(1)?,
                    lineage: row.get(2)?,
                    sealed: row.get(3)?,
                    publication: PublicationAnchor {
                        height: BlockHeight::from(row.get::<_, u32>(4)?),
                        hash: BlockHash::from_slice(&row.get::<_, Vec<u8>>(5)?),
                    },
                })
            })
            .map_err(failure)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failure)?;
        drop(statement);
        let commits: Vec<_> = commits
            .into_values()
            .filter(|commit| {
                !commit.receives.is_empty()
                    || !commit.spends.is_empty()
                    || !commit.coverage.is_empty()
                    || !commit.opened_pages.is_empty()
                    || !commit.completed_pages.is_empty()
            })
            .collect();
        // Persist the conservative export intent before handing facts to the caller.
        // Wallet commits and this companion database cannot share a transaction:
        // a crash before acknowledgment must still report later withdrawal of
        // any batch that might have reached the wallet. Unapplied intents are safe
        // to reconcile through the same trusted wallet controls.
        let tx = self
            .catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        for commit in &commits {
            tx.execute(
                "UPDATE pir_bridge_revisions SET exported=1 WHERE source=?1 AND revision=?2",
                params![commit.revision.source, commit.revision.revision],
            )
            .map_err(failure)?;
        }
        tx.commit().map_err(failure)?;
        let mut hash = Sha256::new();
        hash.update(identity);
        hash.update(self.store.last_commit().map_err(failure)?.to_le_bytes());
        for commit in &commits {
            hash.update(&commit.revision.source);
            hash.update(&commit.revision.revision);
        }
        let token = hash.finalize().into();
        self.pending_export = Some(token);
        Ok(RecoveryBatch {
            commits,
            progress,
            reconciliation,
            token,
        })
    }

    /// A batch exporting nothing, whose token the next acknowledgment must present.
    fn empty<A>(&mut self, progress: Progress) -> Result<RecoveryBatch<A>, RecoveryError> {
        let last_commit = self.store.last_commit().map_err(failure)?;
        let token = Sha256::digest(last_commit.to_le_bytes()).into();
        self.pending_export = Some(token);
        Ok(RecoveryBatch {
            commits: vec![],
            progress,
            reconciliation: vec![],
            token,
        })
    }

    /// Acknowledge only after every wallet commit of the latest batch succeeded.
    /// Clears the export intents of revisions that batch found retired; current
    /// intents were persisted before return. A crash before this acknowledgement
    /// replays the same candidate facts. It never advances a wallet's
    /// qualification, coverage or financial authority.
    pub fn acknowledge_applied<A>(
        &mut self,
        batch: &RecoveryBatch<A>,
    ) -> Result<(), RecoveryError> {
        require(
            self.pending_export == Some(batch.token),
            "export receipt is stale",
        )?;
        let tx = self
            .catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        for retired in &batch.reconciliation {
            tx.execute(
                "UPDATE pir_bridge_revisions SET exported=0 WHERE source=?1 AND revision=?2 AND current=0",
                params![retired.source, retired.revision],
            )
            .map_err(failure)?;
        }
        for commit in &batch.commits {
            tx.execute(
                "UPDATE pir_bridge_revisions SET exported=1 WHERE source=?1 AND revision=?2",
                params![commit.revision.source, commit.revision.revision],
            )
            .map_err(failure)?;
        }
        tx.commit().map_err(failure)?;
        self.pending_export = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use transparent_wallet::client::Table;
    use transparent_wallet::transport::BoxError;
    use transparent_wallet::{Ledger, SetIdentity, StaticChain, StoredEvent};
    use zcash_client_backend::data_api::transparent_ledger::{
        AccountLifecycle, WatchOrigin, WatchedAddress,
    };

    const MORE: Progress = Progress {
        covered_through: 0,
        outcome: Outcome::More,
    };
    const MAP: &[u8] = include_bytes!("../tests/fixtures/shard-map.json");

    fn config() -> RecoveryConfig {
        RecoveryConfig {
            source: b"explicit-fixture-source".to_vec(),
            account_binding: vec![1],
            origin: "http://127.0.0.1:1".into(),
            scripts: 40,
            shards: 2000,
            events: 100_000,
            queries: 64,
            private_bytes: 64 * 1024 * 1024,
        }
    }
    fn map() -> ShardMap {
        serde_json::from_slice(MAP).unwrap()
    }
    fn watch() -> TransparentWatchSet<u32> {
        let map = map();
        let last = map.shards.last().unwrap();
        TransparentWatchSet {
            account: 1,
            lifecycle: AccountLifecycle::Candidate,
            policy_generation: 0,
            target: Some(ChainPoint {
                height: block(last.end_height).unwrap(),
                hash: block_hash(&last.terminal_block_hash).unwrap(),
            }),
            addresses: vec![WatchedAddress {
                address: TransparentAddress::PublicKeyHash([7; 20]),
                origin: WatchOrigin::Standalone,
                required_from: block(map.start_height).unwrap(),
            }],
            pending_pages: vec![],
        }
    }
    fn report(completion: Completion) -> SyncReport {
        SyncReport {
            ledger: Ledger::new(),
            charges: Default::default(),
            matched_shards: vec![],
            unproductive_matches: 0,
            covered_through: 0,
            settled_through: 0,
            provisional: vec![],
            map_refreshes: 0,
            completion,
            rolled_back_to: None,
            replaced_revisions: vec![],
            scripts_added: 0,
            commits: 0,
        }
    }

    /// A public filter source that counts every call and serves only a shard map,
    /// the fixture's by default.
    struct CountingFilters {
        map: Vec<u8>,
        parents: bool,
        parent_checks: Cell<usize>,
        maps: usize,
        filters: usize,
    }
    impl Default for CountingFilters {
        fn default() -> Self {
            Self::serving(MAP.to_vec())
        }
    }
    impl CountingFilters {
        fn serving(map: Vec<u8>) -> Self {
            Self {
                map,
                parents: false,
                parent_checks: Cell::new(0),
                maps: 0,
                filters: 0,
            }
        }
        fn calls(&self) -> usize {
            self.parent_checks.get() + self.maps + self.filters
        }
    }
    impl FilterSource for CountingFilters {
        fn uses_parents(&self) -> bool {
            self.parent_checks.set(self.parent_checks.get() + 1);
            self.parents
        }
        fn shard_map(&mut self) -> Result<(Vec<u8>, u64), BoxError> {
            self.maps += 1;
            Ok((self.map.clone(), self.map.len() as u64))
        }
        fn filter(&mut self, _shard_id: u64) -> Result<(Vec<u8>, u64), BoxError> {
            self.filters += 1;
            Err("the counting source serves no filters".into())
        }
    }

    /// A private shard transport that counts every call and serves only an init.
    #[derive(Default)]
    struct CountingShards {
        schema: &'static str,
        inits: usize,
        manifests: usize,
        setups: usize,
        queries: usize,
    }
    impl CountingShards {
        fn serving(schema: &'static str) -> Self {
            Self {
                schema,
                ..Default::default()
            }
        }
        fn calls(&self) -> usize {
            self.inits + self.manifests + self.setups + self.queries
        }
    }
    impl ShardTransport for CountingShards {
        fn init(&mut self) -> Result<(Vec<u8>, u64), BoxError> {
            self.inits += 1;
            let init = serde_json::to_vec(&serde_json::json!({
                "schema": self.schema,
                "geometries": [],
            }))?;
            let cost = init.len() as u64;
            Ok((init, cost))
        }
        fn manifest(
            &mut self,
            _shard_id: u64,
            _revision: &str,
        ) -> Result<(Vec<u8>, u64), BoxError> {
            self.manifests += 1;
            Err("the counting transport serves no manifests".into())
        }
        fn setup(
            &mut self,
            _shard_id: u64,
            _revision: &str,
            _table: Table,
            _segment: u32,
        ) -> Result<(Vec<u8>, u64), BoxError> {
            self.setups += 1;
            Err("the counting transport serves no setup".into())
        }
        fn query(
            &mut self,
            _shard_id: u64,
            _revision: &str,
            _table: Table,
            _body: &[u8],
        ) -> Result<Vec<u8>, BoxError> {
            self.queries += 1;
            Err("the counting transport answers no queries".into())
        }
    }

    #[test]
    fn companion_binding_rejects_account_and_origin_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("companion.sqlite");
        drop(ReferenceRecovery::open(&path, config()).unwrap());
        for which in 0..3 {
            let mut different = config();
            match which {
                0 => different.account_binding = vec![2],
                1 => different.origin.push_str("/different"),
                _ => different.source = b"another-fixture-source".to_vec(),
            }
            assert!(matches!(
                ReferenceRecovery::open(&path, different),
                Err(RecoveryError::Invalid(_))
            ));
        }
        ReferenceRecovery::open(&path, config()).unwrap();
    }

    #[test]
    fn a_companion_bound_to_another_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("companion.sqlite");
        drop(ReferenceRecovery::open(&path, config()).unwrap());
        let config = config();
        let rebind = |schema: &str| {
            Connection::open(&path)
                .unwrap()
                .execute(
                    "UPDATE pir_bridge_binding SET value=?1 WHERE key='account-source'",
                    [binding([
                        &config.source,
                        &config.account_binding,
                        config.origin.as_bytes(),
                        schema.as_bytes(),
                    ])],
                )
                .unwrap()
        };
        assert_eq!(rebind("transparent-shard-v10"), 1);
        assert!(matches!(
            ReferenceRecovery::open(&path, config.clone()),
            Err(RecoveryError::Invalid(_))
        ));
        assert_eq!(rebind(SCHEMA), 1);
        ReferenceRecovery::open(&path, config).unwrap();
    }

    #[test]
    fn missing_or_unaccepted_targets_and_oversized_watch_sets_fail_before_retrieval() {
        let dir = tempfile::tempdir().unwrap();
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let map = map();
        let unknown = StaticChain::default();
        let accepted = StaticChain::from_map(&map);
        let mut filters = CountingFilters::default();
        let mut shards = CountingShards::serving(SCHEMA);
        let mut missing = watch();
        missing.target = None;
        let mut oversized = watch();
        oversized.addresses = (0..=40)
            .map(|i| WatchedAddress {
                address: TransparentAddress::PublicKeyHash([i; 20]),
                ..oversized.addresses[0]
            })
            .collect();
        let mut duplicated = watch();
        duplicated.addresses.push(duplicated.addresses[0]);
        for (watch, chain) in [
            (&watch(), &unknown),
            (&missing, &accepted),
            (&oversized, &accepted),
            (&duplicated, &accepted),
        ] {
            assert!(matches!(
                adapter.recover(watch, chain, &mut filters, &mut shards),
                Err(RecoveryError::Invalid(_))
            ));
        }
        assert_eq!((filters.calls(), shards.calls()), (0, 0));

        // Scripts the companion already retains count against the same limit.
        let dir = tempfile::tempdir().unwrap();
        let mut crowded =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        crowded.store.bind_set(&SetIdentity::of(&map)).unwrap();
        crowded
            .store
            .add_scripts(
                &(0..=40)
                    .map(|i| ScriptEntry {
                        script: address_script(TransparentAddress::ScriptHash([i; 20])),
                        origin: ScriptOrigin::Imported,
                        required_from: map.start_height,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert!(matches!(
            crowded.recover(&watch(), &accepted, &mut filters, &mut shards),
            Err(RecoveryError::Invalid(_))
        ));
        assert_eq!((filters.calls(), shards.calls()), (0, 0));

        // Once every check passes, the same pass reaches the caller's transports.
        assert!(matches!(
            adapter.recover(&watch(), &accepted, &mut filters, &mut shards),
            Err(RecoveryError::Failure(_))
        ));
        assert_eq!((filters.maps, shards.inits), (1, 1));
        assert!(filters.filters > 0);
    }

    #[test]
    fn parent_filter_sources_are_refused_before_retrieval() {
        let dir = tempfile::tempdir().unwrap();
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let mut filters = CountingFilters {
            parents: true,
            ..Default::default()
        };
        let mut shards = CountingShards::serving(SCHEMA);
        assert!(matches!(
            adapter.recover(
                &watch(),
                &StaticChain::from_map(&map()),
                &mut filters,
                &mut shards
            ),
            Err(RecoveryError::Invalid(_))
        ));
        assert_eq!(filters.parent_checks.get(), 1);
        assert_eq!((filters.maps, filters.filters, shards.calls()), (0, 0, 0));
    }

    #[test]
    fn a_service_with_another_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let mut filters = CountingFilters::default();
        let mut shards = CountingShards::serving("transparent-shard-v10");
        assert!(matches!(
            adapter.recover(
                &watch(),
                &StaticChain::from_map(&map()),
                &mut filters,
                &mut shards
            ),
            Err(RecoveryError::Invalid(_))
        ));
        assert_eq!((filters.maps, shards.inits), (1, 1));
        assert_eq!(
            (
                filters.filters,
                shards.manifests,
                shards.setups,
                shards.queries
            ),
            (0, 0, 0, 0)
        );
        // Nothing reached the companion: it is still bound to no publication.
        assert!(adapter.store.set_identity().unwrap().is_none());
    }

    #[test]
    fn report_outcomes_map_to_progress() {
        let incomplete = |reason| Completion::Incomplete { reason, pending: 3 };
        for (completion, outcome) in [
            (Completion::Complete, Outcome::Complete),
            (incomplete(IncompleteReason::QueryBudget), Outcome::More),
            (incomplete(IncompleteReason::ByteBudget), Outcome::More),
            (incomplete(IncompleteReason::PendingLimit), Outcome::More),
            (
                incomplete(IncompleteReason::PublicationBehind { height: 9 }),
                Outcome::Behind,
            ),
            (
                incomplete(IncompleteReason::Overloaded { shard_id: 1 }),
                Outcome::Overloaded,
            ),
            (
                incomplete(IncompleteReason::ChainUnknown { height: 9 }),
                Outcome::Stalled,
            ),
            (
                incomplete(IncompleteReason::UnresolvedSpends),
                Outcome::Stalled,
            ),
            (
                incomplete(IncompleteReason::DiscoveryUnbounded),
                Outcome::Stalled,
            ),
        ] {
            let mut report = report(completion);
            report.covered_through = 77;
            assert_eq!(
                progress(&report, false),
                Progress {
                    covered_through: 77,
                    outcome,
                }
            );
            // A pass clamped to the publication's end is behind even when it
            // completes; an incomplete one keeps its reason.
            let clamped = if outcome == Outcome::Complete {
                Outcome::Behind
            } else {
                outcome
            };
            assert_eq!(
                progress(&report, true),
                Progress {
                    covered_through: 77,
                    outcome: clamped,
                }
            );
        }
    }

    #[test]
    fn metadata_conversion_preserves_exact_zero_unknown_and_coinbase() {
        for fee in [
            FeeState::Exact(0),
            FeeState::Unknown,
            FeeState::NotApplicable,
        ] {
            let converted = metadata(Some(transparent_events::TransactionMetadata {
                fee,
                transparent_input_count: 0,
                has_shielded_components: true,
            }))
            .unwrap()
            .unwrap();
            assert!(converted.has_shielded_components);
            match fee {
                FeeState::Exact(_) => {
                    assert_eq!(converted.fee, WholeTransactionFee::Exact(Zatoshis::ZERO))
                }
                FeeState::Unknown => assert_eq!(converted.fee, WholeTransactionFee::Unknown),
                FeeState::NotApplicable => {
                    assert_eq!(converted.fee, WholeTransactionFee::NotApplicable)
                }
            }
        }
        assert_eq!(metadata(None).unwrap(), None);
        assert!(
            metadata(Some(transparent_events::TransactionMetadata {
                fee: FeeState::Exact(u64::MAX),
                transparent_input_count: 1,
                has_shielded_components: false
            }))
            .is_err()
        );
    }

    #[test]
    fn normalized_events_preserve_attribution_and_local_shard_anchor_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("companion.sqlite");
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        let map = map();
        let entry = &map.shards[0];
        let watch = watch();
        let script = address_script(watch.addresses[0].address);
        adapter.store.bind_set(&SetIdentity::of(&map)).unwrap();
        adapter
            .store
            .add_scripts(&[ScriptEntry {
                script: script.clone(),
                origin: ScriptOrigin::Imported,
                required_from: map.start_height,
            }])
            .unwrap();
        let parent_meta = transparent_events::TransactionMetadata {
            fee: FeeState::Exact(0),
            transparent_input_count: 3,
            has_shielded_components: true,
        };
        let spend_meta = transparent_events::TransactionMetadata {
            fee: FeeState::Exact(10),
            transparent_input_count: 1,
            has_shielded_components: false,
        };
        let receive = TransparentEvent::Receive(transparent_events::ReceiveEvent {
            metadata: Some(parent_meta),
            height: entry.start_height as u32,
            txid: transparent_events::Txid([1; 32]),
            transaction_index: 1,
            output_index: 0,
            value: 100,
            coinbase: false,
        });
        let spend = TransparentEvent::Spend(transparent_events::SpendEvent {
            metadata: Some(spend_meta),
            height: entry.start_height as u32 + 1,
            spending_txid: transparent_events::Txid([2; 32]),
            transaction_index: 2,
            input_index: 0,
            spent_txid: transparent_events::Txid([1; 32]),
            spent_output_index: 0,
        });
        adapter
            .store
            .commit_shard(transparent_wallet::ShardCommit {
                source_anchor: None,
                shard_id: entry.shard_id,
                revision_digest: entry.manifest_digest.clone(),
                sealed: entry.sealed,
                start_height: entry.start_height,
                end_height: entry.end_height,
                terminal_block_hash: entry.terminal_block_hash.clone(),
                events: [receive, spend]
                    .into_iter()
                    .map(|event| StoredEvent {
                        script: script.clone(),
                        event,
                        shard_id: entry.shard_id,
                        revision_digest: entry.manifest_digest.clone(),
                    })
                    .collect(),
                covered_scripts: vec![script],
                pending_upsert: vec![],
                pending_complete: vec![],
            })
            .unwrap();
        let first = adapter
            .normalize(&watch, map.clone(), MORE, &StaticChain::from_map(&map))
            .unwrap();
        assert_eq!(first.commits.len(), 1);
        let commit = &first.commits[0];
        assert_eq!(commit.anchor.height, block(entry.end_height).unwrap());
        assert!(commit.anchor.height < commit.context.target.height);
        assert_eq!(
            commit.receives[0].metadata.unwrap().fee,
            WholeTransactionFee::Exact(Zatoshis::ZERO)
        );
        assert!(commit.receives[0].metadata.unwrap().has_shielded_components);
        assert_eq!(
            commit.spends[0].metadata.unwrap().fee,
            WholeTransactionFee::Exact(Zatoshis::const_from_u64(10))
        );
        assert_eq!(
            commit.spends[0].metadata.unwrap().transparent_input_count,
            1
        );
        adapter.acknowledge_applied(&first).unwrap();
        assert!(adapter.acknowledge_applied(&first).is_err());
        drop(adapter);
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        let again = adapter
            .normalize(&watch, map.clone(), MORE, &StaticChain::from_map(&map))
            .unwrap();
        assert_eq!(again.commits, first.commits);
        adapter
            .store
            .rollback_above(
                &Anchor {
                    height: map.start_height - 1,
                    hash: map.shards[0].parent_block_hash.clone(),
                },
                "controlled fixture reorg",
            )
            .unwrap();
        let mut replacement = map.clone();
        replacement.shards[0].manifest_digest = "ab".repeat(32);
        let changed = adapter
            .normalize(
                &watch,
                replacement.clone(),
                MORE,
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert_eq!(
            changed.reconciliation,
            vec![first.commits[0].revision.clone()]
        );
        drop(adapter);
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        let after_restart = adapter
            .normalize(
                &watch,
                replacement.clone(),
                MORE,
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert_eq!(after_restart.reconciliation, changed.reconciliation);
    }

    #[test]
    fn possible_wallet_export_survives_crash_before_ack_and_withdrawal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("companion.sqlite");
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        let map = map();
        let entry = &map.shards[0];
        let watch = watch();
        let script = address_script(watch.addresses[0].address);
        adapter.store.bind_set(&SetIdentity::of(&map)).unwrap();
        adapter
            .store
            .add_scripts(&[ScriptEntry {
                script: script.clone(),
                origin: ScriptOrigin::Imported,
                required_from: map.start_height,
            }])
            .unwrap();
        adapter
            .store
            .commit_shard(transparent_wallet::ShardCommit {
                source_anchor: None,
                shard_id: entry.shard_id,
                revision_digest: entry.manifest_digest.clone(),
                sealed: entry.sealed,
                start_height: entry.start_height,
                end_height: entry.end_height,
                terminal_block_hash: entry.terminal_block_hash.clone(),
                events: vec![],
                covered_scripts: vec![script],
                pending_upsert: vec![],
                pending_complete: vec![],
            })
            .unwrap();
        let batch = adapter
            .normalize(&watch, map.clone(), MORE, &StaticChain::from_map(&map))
            .unwrap();
        assert_eq!(batch.commits.len(), 1);
        let possibly_applied = batch.commits[0].revision.clone();
        // The application may have committed this batch, then died before acknowledging it.
        drop(adapter);
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        adapter
            .store
            .rollback_above(
                &Anchor {
                    height: map.start_height - 1,
                    hash: entry.parent_block_hash.clone(),
                },
                "controlled revision withdrawal",
            )
            .unwrap();
        let mut replacement = map;
        replacement.shards[0].manifest_digest = "ab".repeat(32);
        let withdrawn = adapter
            .normalize(
                &watch,
                replacement.clone(),
                MORE,
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert_eq!(withdrawn.reconciliation, vec![possibly_applied.clone()]);
        drop(adapter);
        let mut adapter = ReferenceRecovery::open(&path, config()).unwrap();
        let repeated = adapter
            .normalize(
                &watch,
                replacement.clone(),
                MORE,
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert_eq!(repeated.reconciliation, vec![possibly_applied]);
        adapter.acknowledge_applied(&repeated).unwrap();
        let acknowledged = adapter
            .normalize(
                &watch,
                replacement.clone(),
                MORE,
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert!(acknowledged.reconciliation.is_empty());
    }

    #[test]
    fn page_continuation_identity_binds_revision_script_shard_and_first_row() {
        let first = page_id(0, "aa", &[1], 2);
        for other in [
            page_id(1, "aa", &[1], 2),
            page_id(0, "bb", &[1], 2),
            page_id(0, "aa", &[2], 2),
            page_id(0, "aa", &[1], 3),
        ] {
            assert_ne!(first, other);
        }
    }

    /// A companion bound to the fixture map that retains `script` from `required_from`.
    fn seeded(dir: &tempfile::TempDir, script: &[u8], required_from: u64) -> ReferenceRecovery {
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        adapter.store.bind_set(&SetIdentity::of(&map())).unwrap();
        adapter
            .store
            .add_scripts(&[ScriptEntry {
                script: script.to_vec(),
                origin: ScriptOrigin::Imported,
                required_from,
            }])
            .unwrap();
        adapter
    }

    /// What a pass reading `entry` under `digest` through `end` commits for `script`.
    fn covered(
        entry: &ShardMapEntry,
        digest: &str,
        (end, terminal): (u64, &str),
        script: &[u8],
        events: Vec<TransparentEvent>,
    ) -> transparent_wallet::ShardCommit {
        transparent_wallet::ShardCommit {
            source_anchor: None,
            shard_id: entry.shard_id,
            revision_digest: digest.into(),
            sealed: entry.sealed,
            start_height: entry.start_height,
            end_height: end,
            terminal_block_hash: terminal.into(),
            events: events
                .into_iter()
                .map(|event| StoredEvent {
                    script: script.to_vec(),
                    event,
                    shard_id: entry.shard_id,
                    revision_digest: digest.into(),
                })
                .collect(),
            covered_scripts: vec![script.to_vec()],
            pending_upsert: vec![],
            pending_complete: vec![],
        }
    }

    #[test]
    fn below_floor_view_is_lenient_only_below_the_floor() {
        let floor = 1_000;
        let held = "11".repeat(32);
        let other = "22".repeat(32);
        let chain = StaticChain {
            hashes: BTreeMap::from([(floor, held.clone())]),
        };
        let view = BelowFloor {
            chain: &chain,
            below: floor,
            genesis: MAINNET_GENESIS_DISPLAY,
        };
        // Below the floor every block is accepted, and only block 0 has a hash.
        for height in [0, 1, floor - 1] {
            assert_eq!(view.is_accepted(height, &other), Acceptance::Accepted);
        }
        assert_eq!(view.hash_at(0).as_deref(), Some(MAINNET_GENESIS_DISPLAY));
        assert_eq!(view.hash_at(1), None);
        assert_eq!(view.hash_at(floor - 1), None);
        // From the floor up, the caller's chain alone answers.
        assert_eq!(view.is_accepted(floor, &held), Acceptance::Accepted);
        assert_eq!(view.is_accepted(floor, &other), Acceptance::Rejected);
        assert_eq!(view.is_accepted(floor + 1, &held), Acceptance::Unknown);
        assert_eq!(view.hash_at(floor), Some(held));
        assert_eq!(view.hash_at(floor + 1), None);
        assert_eq!(view.tip(), chain.tip());
        // A floor of 0, when nothing is watched, is never lenient.
        let strict = BelowFloor {
            chain: &chain,
            below: 0,
            genesis: MAINNET_GENESIS_DISPLAY,
        };
        assert_eq!(
            strict.is_accepted(0, MAINNET_GENESIS_DISPLAY),
            Acceptance::Unknown
        );
        assert_eq!(strict.hash_at(0), None);

        // The floor is the lowest required height, capped at the target.
        let mut watch = watch();
        let lowest = u64::from(u32::from(watch.addresses[0].required_from));
        watch.addresses.push(WatchedAddress {
            address: TransparentAddress::PublicKeyHash([8; 20]),
            required_from: block(lowest + 10).unwrap(),
            ..watch.addresses[0]
        });
        assert_eq!(required_floor(&watch, u64::MAX), lowest);
        assert_eq!(required_floor(&watch, lowest - 1), lowest - 1);
        watch.addresses.clear();
        assert_eq!(required_floor(&watch, u64::MAX), 0);
    }

    #[test]
    fn shards_ending_below_the_required_start_are_neither_exported_nor_cataloged() {
        let map = map();
        let (low, high) = (&map.shards[0], &map.shards[1]);
        let mut watch = watch();
        let address = watch.addresses[0].address;
        let script = address_script(address);
        let dir = tempfile::tempdir().unwrap();
        let mut adapter = seeded(&dir, &script, map.start_height);
        // The companion also holds coverage and a receive in the shard below the floor.
        let receive = TransparentEvent::Receive(transparent_events::ReceiveEvent {
            metadata: None,
            height: low.start_height as u32,
            txid: transparent_events::Txid([1; 32]),
            transaction_index: 1,
            output_index: 0,
            value: 100,
            coinbase: false,
        });
        for (entry, events) in [(low, vec![receive]), (high, vec![])] {
            let end = (entry.end_height, entry.terminal_block_hash.as_str());
            adapter
                .store
                .commit_shard(covered(entry, &entry.manifest_digest, end, &script, events))
                .unwrap();
        }
        let cataloged = |adapter: &ReferenceRecovery| -> u64 {
            adapter
                .catalog
                .query_row("SELECT COUNT(*) FROM pir_bridge_revisions", [], |r| {
                    r.get(0)
                })
                .unwrap()
        };

        // The wallet's chain starts at the floor, so it has no hash for the low shard.
        watch.addresses[0].required_from = block(high.start_height).unwrap();
        let chain = StaticChain {
            hashes: BTreeMap::from([(high.end_height, high.terminal_block_hash.clone())]),
        };
        let batch = adapter
            .normalize(&watch, map.clone(), MORE, &chain)
            .unwrap();
        assert_eq!(batch.commits.len(), 1);
        let commit = &batch.commits[0];
        assert_eq!(commit.anchor.height, block(high.end_height).unwrap());
        assert!(commit.receives.is_empty());
        assert_eq!(
            commit.coverage,
            vec![AddressRange {
                address,
                from: block(high.start_height).unwrap(),
                through: block(high.end_height).unwrap(),
            }]
        );
        assert_eq!(cataloged(&adapter), 1);

        // With the floor at the map's start, the same companion exports and
        // catalogs both shards, the receive included.
        watch.addresses[0].required_from = block(map.start_height).unwrap();
        let batch = adapter
            .normalize(&watch, map.clone(), MORE, &StaticChain::from_map(&map))
            .unwrap();
        assert_eq!(batch.commits.len(), 2);
        assert_eq!(batch.commits[0].receives.len(), 1);
        assert_eq!(cataloged(&adapter), 2);
    }

    #[test]
    fn a_pass_watching_nothing_sends_no_request() {
        let map = map();
        let last = map.shards.last().unwrap();
        let mut watch = watch();
        watch.addresses.clear();
        let target = u64::from(u32::from(watch.target.unwrap().height));
        // The wallet holds only its target, far above block 0, the floor of an
        // empty watch set.
        assert_eq!(required_floor(&watch, target), 0);
        let chain = StaticChain {
            hashes: BTreeMap::from([(last.end_height, last.terminal_block_hash.clone())]),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let mut filters = CountingFilters::default();
        let mut shards = CountingShards::serving(SCHEMA);
        let batch = adapter
            .recover(&watch, &chain, &mut filters, &mut shards)
            .unwrap();
        assert_eq!(
            batch.progress,
            Progress {
                covered_through: target,
                outcome: Outcome::Complete,
            }
        );
        assert!(batch.commits.is_empty());
        assert_eq!((filters.calls(), shards.calls()), (0, 0));
        assert!(adapter.store.set_identity().unwrap().is_none());
        adapter.acknowledge_applied(&batch).unwrap();
        // The target must still be accepted.
        assert!(matches!(
            adapter.recover(&watch, &StaticChain::default(), &mut filters, &mut shards),
            Err(RecoveryError::Invalid(_))
        ));
        assert_eq!((filters.calls(), shards.calls()), (0, 0));
    }

    #[test]
    fn sync_target_clamps_only_to_an_accepted_end_at_or_above_the_floor_and_anchor() {
        let map = map();
        let last = map.shards.last().unwrap();
        let t = last.end_height;
        let end = Anchor {
            height: t,
            hash: last.terminal_block_hash.clone(),
        };
        let accepted = StaticChain::from_map(&map);
        let floor = map.start_height;
        let target = Anchor {
            height: t + 10,
            hash: "99".repeat(32),
        };
        let unclamped = (target.clone(), false);

        // Behind, with every condition met: sync to the map's accepted end.
        assert_eq!(
            sync_target(&map, &target, floor, None, 0, &accepted),
            (end.clone(), true)
        );
        // A floor, companion anchor or retained event exactly at the end allows it.
        assert_eq!(
            sync_target(&map, &target, t, Some(&end), t, &accepted),
            (end.clone(), true)
        );
        // Ahead or level: the publication reaches the target.
        for reached in [t - 1, t] {
            let target = Anchor {
                height: reached,
                hash: "99".repeat(32),
            };
            assert_eq!(
                sync_target(&map, &target, floor, None, 0, &accepted),
                (target, false)
            );
        }
        // Below the floor: nothing the watch set requires is published yet.
        assert_eq!(
            sync_target(&map, &target, t + 1, None, 0, &accepted),
            unclamped
        );
        // An unaccepted terminal: the wallet's chain does not know the end.
        assert_eq!(
            sync_target(&map, &target, floor, None, 0, &StaticChain::default()),
            unclamped
        );
        // A map terminal on a stale branch: the wallet holds another block there.
        let mut stale = accepted.clone();
        stale.hashes.insert(t, "aa".repeat(32));
        assert_eq!(
            sync_target(&map, &target, floor, None, 0, &stale),
            unclamped
        );
        // A lagging replica: the companion already settled above the end, or
        // retains an event above it.
        let above = Anchor {
            height: t + 1,
            hash: "bb".repeat(32),
        };
        assert_eq!(
            sync_target(&map, &target, floor, Some(&above), 0, &accepted),
            unclamped
        );
        assert_eq!(
            sync_target(&map, &target, floor, None, t + 1, &accepted),
            unclamped
        );
    }

    #[test]
    fn a_pass_behind_the_publication_syncs_to_its_accepted_end() {
        let map = map();
        let last = map.shards.last().unwrap();
        let t = last.end_height;
        let mut watch = watch();
        let wallet_target = ChainPoint {
            height: block(t + 10).unwrap(),
            hash: BlockHash([9; 32]),
        };
        watch.target = Some(wallet_target);
        let mut chain = StaticChain::from_map(&map);
        chain.hashes.insert(t + 10, wallet_target.hash.to_string());
        let address = watch.addresses[0].address;
        let script = address_script(address);

        // A wallet born above the publication's end has nothing to clamp to.
        let dir = tempfile::tempdir().unwrap();
        let mut young =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let mut born = watch.clone();
        born.addresses[0].required_from = block(t + 1).unwrap();
        let mut filters = CountingFilters::default();
        let mut shards = CountingShards::serving(SCHEMA);
        let batch = young
            .recover(&born, &chain, &mut filters, &mut shards)
            .unwrap();
        assert_eq!(batch.progress.outcome, Outcome::Behind);
        assert!(batch.commits.is_empty());
        assert_eq!(young.store.anchor().unwrap(), None);
        assert_eq!((filters.filters, shards.calls()), (0, 1));

        // A companion already covering the script through the end needs no retrieval.
        let dir = tempfile::tempdir().unwrap();
        let mut adapter = seeded(&dir, &script, map.start_height);
        for entry in &map.shards {
            let end = (entry.end_height, entry.terminal_block_hash.as_str());
            adapter
                .store
                .commit_shard(covered(entry, &entry.manifest_digest, end, &script, vec![]))
                .unwrap();
        }
        let mut filters = CountingFilters::default();
        let mut shards = CountingShards::serving(SCHEMA);
        let batch = adapter
            .recover(&watch, &chain, &mut filters, &mut shards)
            .unwrap();
        assert_eq!((filters.filters, shards.calls()), (0, 1));
        // The client completed through the map's end, which is still behind the wallet.
        assert_eq!(
            batch.progress,
            Progress {
                covered_through: t,
                outcome: Outcome::Behind,
            }
        );
        assert_eq!(
            adapter.store.anchor().unwrap(),
            Some(Anchor {
                height: t,
                hash: last.terminal_block_hash.clone(),
            })
        );
        // Commits keep the wallet's target, and each anchor stays at its shard's end.
        assert_eq!(batch.commits.len(), map.shards.len());
        for (commit, entry) in batch.commits.iter().zip(&map.shards) {
            assert_eq!(commit.context.target, wallet_target);
            assert_eq!(
                commit.anchor,
                ChainPoint {
                    height: block(entry.end_height).unwrap(),
                    hash: block_hash(&entry.terminal_block_hash).unwrap(),
                }
            );
            assert_eq!(
                commit.coverage,
                vec![AddressRange {
                    address,
                    from: block(entry.start_height).unwrap(),
                    through: block(entry.end_height).unwrap(),
                }]
            );
        }
    }

    #[test]
    fn a_replaced_tail_rolls_back_below_the_floor_without_a_wallet_hash() {
        let map = map();
        let tail = map.shards.last().unwrap();
        // The companion covered an older tail revision, ending at a block the wallet accepts.
        let old_end = (tail.end_height - 38, "ef".repeat(32));
        // The wallet holds no block below the tail's start, where the rollback lands.
        let chain = StaticChain {
            hashes: BTreeMap::from([
                old_end.clone(),
                (tail.end_height, tail.terminal_block_hash.clone()),
            ]),
        };
        for (required_from, lenient) in [(tail.start_height, true), (map.start_height, false)] {
            let mut watch = watch();
            watch.addresses[0].required_from = block(required_from).unwrap();
            let script = address_script(watch.addresses[0].address);
            let dir = tempfile::tempdir().unwrap();
            let mut adapter = seeded(&dir, &script, required_from);
            let old = (old_end.0, old_end.1.as_str());
            adapter
                .store
                .commit_shard(covered(tail, &"cd".repeat(32), old, &script, vec![]))
                .unwrap();
            let mut filters = CountingFilters::default();
            let mut shards = CountingShards::serving(SCHEMA);
            // Below the floor the rollback is accepted and the pass goes on to the
            // tail's filter, which the counting source refuses. At or above it the
            // wallet's missing hash stops the pass before any filter request.
            let Err(RecoveryError::Failure(stopped)) =
                adapter.recover(&watch, &chain, &mut filters, &mut shards)
            else {
                panic!("the pass must fail at the filter or at the rollback");
            };
            let cause = if lenient {
                "transport:"
            } else {
                "no accepted rollback hash"
            };
            assert!(stopped.contains(cause), "{stopped}");
            assert_eq!(adapter.store.provisional().unwrap().is_empty(), lenient);
            assert_eq!(filters.filters, usize::from(lenient));
        }
    }

    #[test]
    fn a_map_for_another_network_or_genesis_is_refused_before_retrieval() {
        let fixture: serde_json::Value = serde_json::from_slice(MAP).unwrap();
        let mut testnet = fixture.clone();
        testnet["network"] = "test".into();
        let mut forked = fixture;
        forked["genesis_hash"] = "11".repeat(32).into();
        for served in [testnet, forked] {
            let dir = tempfile::tempdir().unwrap();
            let mut adapter =
                ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
            let mut filters = CountingFilters::serving(serde_json::to_vec(&served).unwrap());
            let mut shards = CountingShards::serving(SCHEMA);
            assert!(matches!(
                adapter.recover(
                    &watch(),
                    &StaticChain::from_map(&map()),
                    &mut filters,
                    &mut shards
                ),
                Err(RecoveryError::Invalid(_))
            ));
            assert_eq!((filters.maps, filters.filters, shards.calls()), (1, 0, 0));
            assert!(adapter.store.set_identity().unwrap().is_none());
        }
    }
}
