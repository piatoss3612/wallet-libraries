# Changelog

## [Unreleased]

### Added

- `sweep`, behind the `wallet` feature, which runs one account's receiver-directory
  restore sweeps of its one-time keys for any store implementing the backend's
  `one_time_ivk::restore::OneTimeIvkRestoreWrite`, with `Transport`, `WriteLock`,
  `Swept`, `Error` and `MAINNET_GENESIS`. `wallet` enables the backend's
  `orchard` feature.
- `fetch_seen` and `Seen`: a publication's swap provider seen sets, which a wallet
  checks before issuing a swap address. They need no `wallet` feature.
- `begin_early_restore`, `EarlyRestore`, `LookedUp`, `FIRST_WINDOW`, `GAP` and `MAX_KEYS`,
  behind the `wallet` feature: before a restore scan, registers an account's one-time keys
  from a publication's public filters, then looks up only the paid ones while the scan runs,
  so the wallet finds each payment as the scan covers its block.
