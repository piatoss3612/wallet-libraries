# Changelog

## [Unreleased]

## [0.0.1-rc0] - 2026-09-27

Initial release candidate for policy-bound transaction status observation.

### Added

- `StatusReader`, which selects one public or private status source per batch
  without cross-source fallback.
- A lightwalletd source that validates the transaction ID returned by
  `GetTransaction` and discards the transaction payload.
- Typed source-opening, observation, transport, and local-storage errors.
