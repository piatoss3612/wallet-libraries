use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};
use transparent::{address::TransparentAddress, bundle::OutPoint};
use transparent_events::{FeeState, TransparentEvent};
use transparent_filter::{ShardMap, ShardMapEntry};
use transparent_wallet::http::{HttpFilterSource, HttpObserver, HttpOptions, HttpShardTransport};
use transparent_wallet::transport::FilterSource;
use transparent_wallet::{
    Acceptance, Anchor, ChainView, ScriptEntry, ScriptOrigin, StaticScripts, SyncReport,
    WalletStore, WorkLimits,
};
use transparent_wallet_store::SqliteStore;
use zcash_client_backend::data_api::transparent_ledger::{
    AddressRange, ChainPoint, PageRequest, PublicationAnchor, ReceiveEvent, RecoveryRevision,
    SpendEvent, TransactionMetadata, TransparentLedgerCommit, TransparentWatchSet,
    WholeTransactionFee,
};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};

/// Explicit endpoints and finite resource bounds for one recovery pass.
#[derive(Clone, Debug)]
pub struct RecoveryConfig {
    /// Caller-chosen source identity, independent of a server's assertions.
    pub source: Vec<u8>,
    /// Stable wallet/account identity; use a different companion store per account.
    pub account_binding: Vec<u8>,
    /// Explicit public filter origin.
    pub filter_origin: String,
    /// Explicit private shard origin.
    pub shard_origin: String,
    /// Selected protocol schema; v10 metadata remains unavailable.
    pub schema: String,
    /// Shared deadline for all attempts of each HTTP call.
    pub timeout: Duration,
    /// Maximum decoded response body bytes per HTTP attempt.
    pub response_bytes: usize,
    /// Maximum watched scripts and retained companion scripts.
    pub scripts: usize,
    /// Maximum shard-map entries per pass.
    pub shards: usize,
    /// Maximum candidate event records exported per pass.
    pub events: usize,
    /// Private query budget. Exhaustion retains continuation and reports incomplete.
    pub queries: u64,
    /// Private payload budget, including setup. One atomic response may cross it.
    pub private_bytes: u64,
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
/// `reconciliation` is durable and must be resolved through existing trusted wallet
/// qualification/rewind controls before applying commits. Neither it nor a server
/// withdrawal is authority to change local facts or promote an account.
pub struct RecoveryBatch<AccountId> {
    /// Source-bound normalized commits, including unfinished page work.
    pub commits: Vec<TransparentLedgerCommit<AccountId>>,
    /// Revisions previously exported but now absent or changed in reference state.
    pub reconciliation: Vec<RecoveryRevision>,
    /// Independent reference retrieval outcome; incomplete is never synchronized.
    pub report: SyncReport,
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
fn block_hash(display: &str) -> Result<BlockHash, RecoveryError> {
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

impl ReferenceRecovery {
    /// Open a compatible companion store and fence its account and endpoint binding.
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
                && config.private_bytes > 0
                && config.response_bytes > 0
                && !config.timeout.is_zero(),
            "recovery bounds must be positive",
        )?;
        require(
            matches!(
                config.schema.as_str(),
                "transparent-shard-v10" | "transparent-shard-v11"
            ),
            "unsupported selected schema",
        )?;
        for origin in [&config.filter_origin, &config.shard_origin] {
            let url = url::Url::parse(origin).map_err(failure)?;
            require(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "invalid explicit HTTP origin",
            )?;
        }
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
        let mut hash = Sha256::new();
        for value in [
            &config.source,
            &config.account_binding,
            &config.filter_origin.as_bytes().to_vec(),
            &config.shard_origin.as_bytes().to_vec(),
            &config.schema.as_bytes().to_vec(),
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        let binding = hash.finalize().to_vec();
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
            "companion account/source/schema binding mismatch",
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

    /// Recover a finite pass using the caller's independently accepted chain.
    /// No source I/O occurs without an accepted local target and bounded watch set.
    pub fn recover<A: Copy + std::fmt::Debug>(
        &mut self,
        watch: &TransparentWatchSet<A>,
        chain: &impl ChainView,
        observer: Option<HttpObserver>,
    ) -> Result<RecoveryBatch<A>, RecoveryError> {
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
        let options = HttpOptions {
            timeout: self.config.timeout,
            user_agent: "zakura-transparent-recovery".into(),
        };
        let mut filters = HttpFilterSource::new(&self.config.filter_origin, &options)
            .map_err(failure)?
            .with_response_limit(self.config.response_bytes)
            .with_prefetch_concurrency(1)
            .with_transient_retry_attempts(3);
        let mut transport = HttpShardTransport::new(&self.config.shard_origin, &options)
            .map_err(failure)?
            .with_response_limit(self.config.response_bytes)
            .with_concurrency(1)
            .with_transient_retry_attempts(3);
        if let Some(observer) = observer {
            filters = filters.with_observer(observer.clone());
            transport = transport.with_observer(observer);
        }
        let (raw_map, map_bytes) = filters.shard_map().map_err(failure)?;
        let map: ShardMap = serde_json::from_slice(&raw_map).map_err(failure)?;
        require(
            map.shards.len() <= self.config.shards,
            "publication shard limit exceeded",
        )?;
        let geometry = transport.geometry().map_err(failure)?;
        require(
            geometry.schema == self.config.schema,
            "selected schema disagrees with service",
        )?;
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
            chain,
            &mut scripts,
            &mut filters,
            &mut transport,
            &limits,
            &target,
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
        self.normalize(watch, current_map, report, chain)
    }

    fn normalize<A: Copy>(
        &mut self,
        watch: &TransparentWatchSet<A>,
        map: ShardMap,
        report: SyncReport,
        chain: &impl ChainView,
    ) -> Result<RecoveryBatch<A>, RecoveryError> {
        let context = watch.context().expect("validated before retrieval");
        let identity = self
            .store
            .set_identity()
            .map_err(failure)?
            .ok_or_else(|| RecoveryError::Invalid("missing reference identity".into()))?;
        map.check_shape().map_err(failure)?;
        require(
            identity.continues(&transparent_wallet::SetIdentity::of_schema(
                &map,
                &self.config.schema,
            )),
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
        let mut commits = BTreeMap::new();
        for entry in &map.shards {
            if entry.start_height > u64::from(u32::from(context.target.height)) {
                continue;
            }
            let height = entry
                .end_height
                .min(u64::from(u32::from(context.target.height)));
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
            if u64::from(stored.event.height()) < u64::from(u32::from(address.required_from))
                || stored.event.height() > u32::from(context.target.height)
            {
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
                let through = range
                    .end_height
                    .min(u64::from(u32::from(context.target.height)));
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
            let through = entry
                .end_height
                .min(u64::from(u32::from(context.target.height)));
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
            reconciliation,
            report,
            token,
        })
    }

    /// Acknowledge only after trusted reconciliation and every wallet commit succeeded.
    /// Clears retired export intents; current intents were persisted before return.
    /// A crash before this acknowledgement replays the same candidate facts. It
    /// never advances a wallet's qualification, coverage or financial authority.
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
        tx.execute(
            "UPDATE pir_bridge_revisions SET exported=0 WHERE current=0",
            [],
        )
        .map_err(failure)?;
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
    use transparent_wallet::{Completion, Ledger, SetIdentity, StaticChain, StoredEvent};
    use zcash_client_backend::data_api::transparent_ledger::{
        AccountLifecycle, WatchOrigin, WatchedAddress,
    };

    fn config() -> RecoveryConfig {
        RecoveryConfig {
            source: b"explicit-fixture-source".to_vec(),
            account_binding: vec![1],
            filter_origin: "http://127.0.0.1:1".into(),
            shard_origin: "http://127.0.0.1:1".into(),
            schema: "transparent-shard-v11".into(),
            timeout: Duration::from_secs(1),
            response_bytes: 64 * 1024 * 1024,
            scripts: 40,
            shards: 2000,
            events: 100_000,
            queries: 64,
            private_bytes: 64 * 1024 * 1024,
        }
    }
    fn map() -> ShardMap {
        serde_json::from_str(include_str!("../tests/fixtures/shard-map.json")).unwrap()
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
    fn report() -> SyncReport {
        SyncReport {
            ledger: Ledger::new(),
            charges: Default::default(),
            matched_shards: vec![],
            unproductive_matches: 0,
            covered_through: 0,
            settled_through: 0,
            provisional: vec![],
            map_refreshes: 0,
            completion: Completion::Incomplete {
                reason: transparent_wallet::IncompleteReason::QueryBudget,
                pending: 0,
            },
            rolled_back_to: None,
            replaced_revisions: vec![],
            scripts_added: 0,
            commits: 0,
        }
    }

    #[test]
    fn companion_binding_rejects_account_endpoint_and_schema_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("companion.sqlite");
        drop(ReferenceRecovery::open(&path, config()).unwrap());
        for which in 0..3 {
            let mut different = config();
            match which {
                0 => different.account_binding = vec![2],
                1 => different.shard_origin.push_str("/different"),
                _ => different.schema = "transparent-shard-v10".into(),
            }
            assert!(ReferenceRecovery::open(&path, different).is_err());
        }
        ReferenceRecovery::open(&path, config()).unwrap();
    }

    #[test]
    fn missing_or_unaccepted_targets_and_oversized_watch_sets_fail_before_http() {
        let dir = tempfile::tempdir().unwrap();
        let mut adapter =
            ReferenceRecovery::open(dir.path().join("companion.sqlite"), config()).unwrap();
        let mut watch = watch();
        assert!(matches!(
            adapter.recover(&watch, &StaticChain::default(), None),
            Err(RecoveryError::Invalid(_))
        ));
        watch.target = None;
        assert!(matches!(
            adapter.recover(&watch, &StaticChain::default(), None),
            Err(RecoveryError::Invalid(_))
        ));
        let mut watch = self::watch();
        watch.addresses = vec![watch.addresses[0]; 41];
        assert!(matches!(
            adapter.recover(&watch, &StaticChain::from_map(&map()), None),
            Err(RecoveryError::Invalid(_))
        ));
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
            .normalize(&watch, map.clone(), report(), &StaticChain::from_map(&map))
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
            .normalize(&watch, map.clone(), report(), &StaticChain::from_map(&map))
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
                report(),
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
                report(),
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
            .normalize(&watch, map.clone(), report(), &StaticChain::from_map(&map))
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
                report(),
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
                report(),
                &StaticChain::from_map(&replacement),
            )
            .unwrap();
        assert_eq!(repeated.reconciliation, vec![possibly_applied]);
        adapter.acknowledge_applied(&repeated).unwrap();
        let acknowledged = adapter
            .normalize(
                &watch,
                replacement.clone(),
                report(),
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
}
