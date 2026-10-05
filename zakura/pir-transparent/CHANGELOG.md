# Changelog

## [Unreleased]

### Added

- `SCHEMA` (`transparent-shard-v11`), the only shard schema the adapter reads.
- `RecoveryBatch::retired_revisions()` exposes withdrawal notifications without
  granting authority to withdraw wallet evidence. `acknowledge_applied` refuses
  batches with retirements; `acknowledge_reconciled` explicitly confirms successful
  trusted wallet reconciliation and application of the batch's commits.
- `Progress { covered_through, outcome }` and `Outcome { Complete, Behind,
  More, Overloaded, Stalled }`, the adapter's own report of a pass.
- Re-exports of `ChainView`, `FilterSource`, `ShardTransport`, `ShardRequest`,
  `Table`, `refusal`, `StaleRevision`, `Overloaded` and `BoxError`, so an
  application can implement its transports without naming wallet-pir crates.
- `WalletChain`, a `ChainView` over the blocks a wallet scanned
  (`WalletRead::get_block_hash`) that answers only through the watch set's
  target and is `Unknown` above it or where the wallet holds no block.
- `BatchState { Ready, Pending, Withdrawn(WithdrawnCause) }`,
  `WithdrawnCause { Regression, Equivocation, ChangedSealed, Retired }` and
  `RecoveryBatch::state`. Commits are returned, and a batch can be
  acknowledged, only when the state is `Ready`.
- `RecoveryError::PublicationChanged`, for a publication whose set identity no
  longer continues the companion's, including a map refreshed mid-pass.
  Recreating the companion is then safe. Any other divergence the reference
  client reports is a `RecoveryError::Failure`, retried with the same
  companion.

### Changed

- `ReferenceRecovery::recover` takes the caller's `FilterSource` and
  `ShardTransport` for each pass. Before any retrieval it checks the target,
  the script limits, that the filter source does not use parent filters, the
  shard limit, and that the service's init names `SCHEMA`.
- `RecoveryConfig::origin` replaces `filter_origin` and `shard_origin`. It is
  an identity label and is never dialed. The companion binding now covers the
  source, account binding, origin and `SCHEMA`, so companions created before
  this change are refused at open and must be recreated.
- `RecoveryBatch::progress` replaces `report`. The companion classifies the
  exported revisions a map no longer publishes itself, and a batch lists only
  those its commits resolve; their export intents stay in the companion until
  trusted reconciliation is acknowledged.
- The wallet-pir crates move to `648264bb4801ae8faf7a61f4638ea049edb167cf`,
  whose client accepts maps and manifests that carry txid display fields.
- `recover` refuses, before the service's init, a shard map whose network or
  genesis block is not Zcash mainnet's.
- A pass needs the chain view only from the watch set's floor, its lowest
  required height. Shards ending below it are neither exported nor cataloged,
  and the reference client may roll back below it without a wallet hash, so a
  map that starts at genesis serves a wallet with a later birthday. A watch set
  with no addresses sends no request and completes at its target.
- When the publication ends below the target, a pass syncs to the map's end if
  that end is at or above the floor, the chain view accepts its terminal block,
  and the companion holds no anchor or event above it. Completing there reports
  `Outcome::Behind` with `covered_through` at the map's end, and the commits
  keep the watch set's context. A pass that cannot clamp, as for a wallet born
  above the map's end, also reports `Behind`, including on a companion no
  earlier pass has bound.
- Revision identities are stable across companions. `source` covers the
  companion binding, the set-identity fields that never change while the
  publication continues, the shard's geometry and that geometry's seal
  parameters, and the shard id, so a new geometry tier changes no existing
  source. `revision` covers the manifest digest and seal state, and `lineage`
  is the published revision number plus one instead of a per-companion
  counter, so a recreated companion reproduces the wallet's triples.
- Companions are format `transparent-reference-companion-v2`, with a
  `pir_bridge_catalog` table. Earlier companions are refused at open with
  `companion format v1; recreate`.
- A pass builds its batch from the shard map its sync finished with and fetches
  no second map.
- A lagging unsealed revision (also below a revision seen sealed), a map
  missing a shard, stored facts naming a revision the map no longer names, a
  shard ending on a block the chain view does not hold, or an exported tail
  without a retrieved successor make the batch `Pending` instead of failing
  the pass. A sealed regression, an equivocating revision, a changed sealed
  revision or a retired shard make it `Withdrawn`.
- Each pass prunes the catalog to published, exported and per-source newest
  rows, the store's filter and setup caches to revisions the map names, and
  the store's commit log to its last entry.

### Removed

- `transparent-shard-v10` support.
- `RecoveryConfig::{schema, timeout, response_bytes}`, the `observer`
  argument of `recover`, and the adapter's fixed HTTP user agent.
- `reqwest` from the normal dependency graph. Only the `recover-activity`
  example builds the reference HTTP transports.
- The companion's `lineage` binding key, its `pir_bridge_revisions` table and
  the 65,536-revision catalog limit.
