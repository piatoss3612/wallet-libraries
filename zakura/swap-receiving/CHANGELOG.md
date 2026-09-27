# Changelog

## [Unreleased]

- Add unpublished swap receiving-key derivation, shared purpose/index identities,
  and refund memo helpers, with independent KDF vectors and an Ironwood
  receive/reconstruct/spend proof test.
- Add shared completion policy with durable grace and reconciliation deadlines,
  receipt and coverage checks, shared-key decisions, and reorg invalidation.
  Expose a separate hard scan deadline for wallets that hand off to PIR regardless
  of receipt resolution. SQLite now persists bounded watches and recovery targets.
- Add transport-independent incoming-note authentication and commitment-path
  validation for privately discovered swap payments.
