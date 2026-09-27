# Changelog

## [Unreleased]

## [0.0.1-rc0] - 2026-09-27

Initial release candidate for private transaction-status observation.

### Added

- The `status-pir-v3-native-two-mask-m29` row format and transport-neutral
  client.
- Wallet-verified anchor acceptance, bounded clock-skew checks, and exact
  request, session, and response length validation.
- Coverage-aware observations that keep absent records inconclusive without a
  conservative earliest-inclusion bound.

### Security

- Status rows and coverage are server assertions, not inclusion proofs.
  Applications must reconcile observations with wallet-verified chain state.
