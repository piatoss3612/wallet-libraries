# Preparatory refactor for a transparent PIR ledger

Status: proposed implementation sequence; no production authority change.

## Purpose and fixed decisions

Prepare wallet-libraries and Vizor for the
[transparent-ledger architecture](transparent-pir-ledger-architecture.md) in
five dependency-ordered slices. That document owns the invariants, migration
lifecycle, and API semantics; this document owns the implementation order and
acceptance gates.

Public discovery remains authoritative during preparation. Exercise isolated
shadow recovery and private activation with fixtures before adding a real PIR
source. Keep the existing LRZ wallet core and database. Wallet-libraries owns
financial correctness; Vizor owns the shared private-queries setting,
scheduling, transport, and UI.

The existing private-queries setting will govern transparent recovery together
with Enhance and Status. There is no new user toggle. The policy applies to a
whole wallet database on its network; account recovery can finish independently.
Once production private authority is enabled, public transparent discovery stops
immediately and incomplete accounts pause transparent spending and shielding.
Eligible shielded-only operations continue. Rollback is supported only to
privacy-aware releases or by forward repair.

Use the current checkout and consumer pin as implementation baselines. At the
2026-09-28 review, wallet-libraries was `21569fb795`; Vizor's
`roman/ironwood-memo-pir` branch was `a3a2683ef` and pinned wallet-libraries
to `2e206f7894`. Recheck these before implementation. Upgrade the consumer pin
as a separately validated dependency change; do not attribute unrelated
dependency or consensus changes to this refactor.

## Implementation sequence

Each slice includes its own contract and regression tests. Slice 5 adds
cross-repository qualification; it is not where earlier safety tests begin.
No intermediate release may expose authoritative ledger projection without its
rewind and provenance handling.

### 1. Contract and migration foundation — wallet-libraries

Add `data_api::transparent_ledger` in the backend and its additive SQLite
schema through the normal migration machinery.

- Define `ChainPoint`, `TransparentLedgerMode`, and the single
  `TransparentLedgerSnapshot<AccountId>` specified in the architecture.
  Ledger read/write traits extend the existing wallet traits and reuse their
  account/error types.
- Provide watched-script snapshots with conservative recovery bounds and
  generations. Define normalized commits with expected policy/chain/watch
  context, and the guarded account-promotion interface.
- Store policy/lifecycle metadata, scripts, events and source observations,
  coverage, pending progress, and projection provenance. Keep protocol wire
  layouts out of these APIs.
- Classify existing remote records as legacy evidence without private coverage.
  Preserve independently established local transactions, outboxes, locks,
  address reservations, and payload provenance. Support multiple origins for
  one projected record.
- New ledger operations require explicit configuration; introducing schema and
  types alone does not change current production authority.

**Acceptance:** fresh and representative existing databases migrate without
seeds or changed public balances. Include multi-seed, imported-only,
hardware-first, pending-send, and previously rewound fixtures. Interrupted
migration and storage failure must remain recoverable without fabricating
coverage.

### 2. Atomic ledger lifecycle — wallet-libraries

Implement event persistence, projection, promotion, and rollback together.

- Make candidate commits isolated from production balances, spends, locks,
  address-use, and receive-address choice. Candidate window growth stays in the
  candidate watch set.
- Make authoritative commits persist events, complete coverage, page progress,
  source observations, and LRZ projection in one transaction. Partial pages
  cannot advance coverage; negative filter results can establish validated
  empty coverage.
- Separate immutable content from mining placement and publication observations.
  Support replay across revisions, spend-before-receive, and re-mining after
  rewind; reject contradictory contents or canonical spends.
- Implement promotion as a guarded transaction that reconciles the complete
  candidate state, preserves local overlays, materializes the projection, and
  changes account authority. Test fixtures cannot qualify production accounts.
- Integrate with all applicable LRZ rewind/rescan paths using their actual
  retained height and rescan floor. Integrate account deletion, import, and
  birthday lowering, including stale-work invalidation.
- Preserve explicit coinbase classification and history without raw payloads.
  Unknown metadata cannot become fabricated values or public lookup authority.

**Acceptance:** fixture receive/spend, replay, revision, promotion, and reorg
flows leave ledger and projection consistent across transaction failure and
restart. Shadow changes no production financial or address-allocation state.
Rewind preserves independent local evidence and issued-address history.

### 3. Financial integration — wallet-libraries

Connect the ledger to existing financial reads and transaction operations.

- Read balance, accepted point, authority, and coverage in one snapshot. Keep
  authorized, last-known, and recovered-but-unverified amounts distinguishable.
  Do not present incomplete recovery as zero or a lower-bound balance.
- Enforce coverage in existing individual-outpoint, address, batched, and
  value-bounded transparent input queries. Add no parallel selector.
- Distinguish recovery through accepted block `H` from LRZ transaction target
  `H + 1`. Require current accepted coverage and recheck context at financial
  authorization; historical snapshots do not authorize live spending.
- Retain confirmations, coinbase maturity, spend and lock rules. Revalidate
  transparent inputs at proposal consumption and hardware finalization.
  Preserve legitimate local chained outputs and their reservations.
- Keep existing public behavior when explicitly configured public. Private
  recovery blocks transparent input use without blocking independently
  eligible shielded-only operations.

**Acceptance:** every transparent selector and shielding path rejects incomplete
private coverage, including stale proposals. Test coinbase boundaries, locks,
mixed-pool selection, local chained outputs, and unchanged public-mode results.

### 4. Vizor policy, coordinator, and presentation

Integrate the new APIs into the existing application rather than replacing
its sync engine.

- Extend the existing private-queries policy and paused setting-transition flow.
  Configure foreground, read-only, reopened, transaction, and native/background
  handles consistently. Persist applied policy/generation and reconcile
  interrupted preference/database transitions conservatively before networking.
- Apply authorization to UTXO refresh, Ledger discovery, transparent history,
  software account discovery, and balance previews. Calls before DB creation
  resolve the same shared setting. An unsupported private preview reports
  unavailable.
- Prevent private ledger evidence from creating public payload, status, fee, or
  parent-transaction lookups. Preserve the separate work/evidence contracts of
  Enhance and Status.
- Add a dedicated bounded coordinator that captures a fixed accepted target and
  operation context, enumerates scripts, invokes a source, commits through
  wallet-libraries, and repeats on window growth. Completion comes from durable
  state.
- Initial sources are disabled and fixture implementations. Disabled means
  unavailable, never successful coverage. Fixtures are restricted to tests and
  development; production remains on its existing public path during preparation.
- Expose account recovery and transparent-operation availability through simple
  FFI results. Display last-known and unverified amounts accurately, retain
  unknown history metadata, and invalidate wallet-summary caches on ledger,
  policy, promotion, and lifecycle changes.
- Reuse existing HTTPS routing, cancellation, deadlines, and transport tests.
  Move only the common transport boundary when the real source needs it;
  publication revision handling stays source-specific.

**Acceptance:** fixture recovery drives Rust/Flutter balance, history, software
shielding, Ledger rounds, and Keystone PCZT behavior without application writes
to ledger/projection tables. A private transition revokes stale work across all
entry points and leaves eligible shielded-only operations available.

### 5. Shadow and migration qualification

Build an end-to-end fixture harness and a real-source-ready comparison boundary.

- Compare exact receive/spend, UTXO, and balance sets at a common accepted point;
  validate fixtures against an independent block-derived oracle. Legacy parity
  alone does not prove correctness or completeness.
- Exercise public-to-shadow, private activation before recovery, reuse of
  qualified shadow state, per-account promotion, interrupted activation,
  restart, and outage after activation.
- Capture requests at sync, import, preview, metadata, startup, and native
  boundaries. Private failures, missing data, cancellation, and setting races
  must produce no unauthorized public lookup.
- Test account import/deletion, earlier recovery bounds, same-height reorgs,
  actual rewind heights, and cache invalidation during recovery.
- Validate forward repair and the designated privacy-aware rollback release.
  Preserve pending sends, reservations, and the shared setting across recovery.
- Keep exact comparisons local; record only redacted aggregate qualification
  results and the tested application/library revisions.

**Acceptance:** no shadow financial side effects, no unexplained qualifying
discrepancies, no unauthorized public requests, and consistent restart/rewind
results. This qualifies the wallet integration, not a remote PIR service.

## Validation and completion

Run focused backend/SQLite tests with the transparent feature enabled, their
applicable existing balance/selection/migration regression suites, and Vizor
Rust and Dart tests for the changed interfaces. Validate the actual consumer
dependency graph and feature configuration when updating its pin. Heavy
regtest/device qualification is a separate explicitly scheduled release gate;
fixture success must not be reported as that gate passing.

The preparatory stage is complete when a deterministic source can recover
receives and spends, preserve unresolved work, project history and balances,
block or permit transparent input use based on coverage, promote an account,
restart, and rewind through the intended APIs. It must also demonstrate that
shadow is isolated and shared-policy transitions cover every disclosure entry
point.

The next stage supplies real filters, manifests, shards, PIR retrieval, and
publication verification. Production activation additionally requires protocol
known-answer/malformed-input validation, real-source shadow qualification,
independent publication verification, a controlled private cohort, and measured
mobile/network-route behavior.

## Deliberate exclusions

Preparation does not change production source authority, remove the public
source, replace the whole wallet core, introduce a public-source ledger migration,
or combine transparent, status, and payload work queues. It does not move
authoritative state into another database or create a general sync plugin
framework. It adds no standalone private-runtime project, cryptographic
completeness proof, or claim that historical Vizor binaries enforce the new
privacy policy.
