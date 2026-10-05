# Changelog

## [Unreleased]

- Add unpublished swap receiving-key derivation, shared purpose/index identities,
  and refund memo helpers, with independent KDF vectors and an Ironwood
  receive/reconstruct/spend proof test.
- Add the shared completion policy (24-hour grace, 7-day limit) and
  direction-aware NEAR status normalization, `lifecycle::near_observation`,
  including refunded amounts and exact-output leftovers. Durable scheduling and
  receipt accounting live in SQLite.
- Add transport-independent incoming-note authentication and commitment-path
  validation for privately discovered swap payments.
- `RefundMemo` stores only the refund index, and no longer takes a network.
  Recovery reads the deposit address from the funding transaction's single
  transparent output. Bytes after the index are reserved and ignored, so
  prerelease records that appended the address still decode.
