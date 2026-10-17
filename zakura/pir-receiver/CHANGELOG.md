# Changelog

## [Unreleased]

### Added

- `discover`, behind the `wallet` feature, which runs one account's discovery of its
  one-time keys for any store implementing the backend's
  `one_time_ivk::restore::OneTimeIvkRestoreWrite`, with `Transport`, `WriteLock`,
  `Discovered`, `Error`, `FIRST_WINDOW`, `GAP`, `KEYS_PER_RUN` and `MAINNET_GENESIS`. It checks
  candidate keys against a publication's filters, looks up the paid ones over PIR several at
  once, each paging from the cursor the wallet keeps, and sends late payments their
  inclusion paths. `wallet` enables the backend's `orchard` feature.
- `fetch_seen` and `Seen`: a publication's swap provider seen sets, which a wallet
  checks before issuing a swap address. They need no `wallet` feature.

### Changed

- Builds on the library's `zakura-receiver-directory` and `zakura-receiver-pir`
  instead of wallet-pir's receiver crates, re-exported as `receiver_directory` and
  `receiver_pir`. Their lookups return records checked structurally only, so the
  sweep validates receivers, every looked-up payment and the common witness file
  with `zakura-receiver-crypto`; refusals keep their directory errors. Only the
  records of the looked-up receiver are checked cryptographically; other records
  sharing a row are parsed structurally. A forged payment is refused after its
  lookup reads the pages it can in one run. The re-exported crates return unchecked
  data.
