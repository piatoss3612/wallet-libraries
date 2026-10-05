# Changelog

## [Unreleased]

### Added

- `SCHEMA` (`transparent-shard-v11`), the only shard schema the adapter reads.
- `Progress { covered_through, outcome }` and `Outcome { Complete, Behind,
  More, Overloaded, Stalled }`, the adapter's own report of a pass.
- Re-exports of `ChainView`, `FilterSource`, `ShardTransport`, `ShardRequest`,
  `Table`, `refusal`, `StaleRevision`, `Overloaded` and `BoxError`, so an
  application can implement its transports without naming wallet-pir crates.

### Changed

- `ReferenceRecovery::recover` takes the caller's `FilterSource` and
  `ShardTransport` for each pass. Before any retrieval it checks the target,
  the script limits, that the filter source does not use parent filters, the
  shard limit, and that the service's init names `SCHEMA`.
- `RecoveryConfig::origin` replaces `filter_origin` and `shard_origin`. It is
  an identity label and is never dialed. The companion binding now covers the
  source, account binding, origin and `SCHEMA`, so companions created before
  this change are refused at open and must be recreated.
- `RecoveryBatch::progress` replaces `report`. Retired export intents stay in
  the companion and are cleared on acknowledgment, but are no longer returned.
- The wallet-pir crates move to `648264bb4801ae8faf7a61f4638ea049edb167cf`,
  whose client accepts maps and manifests that carry txid display fields.

### Removed

- `transparent-shard-v10` support.
- `RecoveryConfig::{schema, timeout, response_bytes}`, the `observer`
  argument of `recover`, and the adapter's fixed HTTP user agent.
- `reqwest` from the normal dependency graph. Only the `recover-activity`
  example builds the reference HTTP transports.
