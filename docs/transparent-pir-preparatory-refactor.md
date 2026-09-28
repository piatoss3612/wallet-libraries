# Preparatory refactor for a transparent PIR ledger

Status: proposed execution plan. All phases below remain to be implemented and
qualified; production transparent authority stays public during preparation.

## Objective and fixed boundaries

Prepare wallet-libraries and Vizor incrementally for the
[transparent-ledger architecture](transparent-pir-ledger-architecture.md).
That document owns the invariants and API semantics. This plan owns the order
of changes, repository handoffs, intermediate behavior, and acceptance gates.

Keep the existing LRZ wallet core, database, input selectors, transaction
builder, and hardware flows. Wallet-libraries owns durable evidence and financial
authorization; Vizor owns the shared private-queries setting, networking,
scheduling, and presentation. No new user toggle is introduced.

Shielded scanning and transparent recovery remain two logical discovery loops
joined by txid. Enhance and Status have separate work and completion rules.
Follow the architecture's [sync contract](transparent-pir-ledger-architecture.md#shielded-sync-and-shared-transactions)
and [history contract](transparent-pir-ledger-architecture.md#history-completeness-and-storage-contract).

Private activation is exercised only in tests/development during this plan.
Once production activation is introduced in a later stage, the saved shared
setting applies before discovery, public transparent queries stop immediately,
and incomplete accounts cannot consume transparent inputs. Independently
eligible shielded-funded operations, including unshielding, remain available.
Missing display-only payment details do not block otherwise eligible spending.

## Execution order and repository handoffs

Complete Phase 0, then Phases 1–6 in order. Within each phase, implement and test
the wallet-libraries steps first, then integrate that exact revision into Vizor.
A phase can contain several small changes; its acceptance gate must pass before
enabling behavior that depends on it. Tests belong with each change, not only
in the final qualification phase.

| Phase | Wallet-libraries deliverable | Vizor deliverable | Behavior available at phase exit |
| --- | --- | --- | --- |
| 0. Baseline | Contract/call-site inventory and reference fixtures. | Consumer baseline and discovery/handle inventory. | Existing public behavior recorded. |
| 1. Contract and migration | Types, additive schema, provenance, configured handles. | Dependency upgrade and explicit handle configuration. | Schema upgrades; private transparent input use remains unavailable. |
| 2. Privacy boundaries | Durable policy transitions and guarded follow-on work. | Shared policy, dispatch guards, native/preview coverage. | Required-private fixtures fail closed before any unsupported request. |
| 3. Candidate recovery | Watched scripts, candidate events, coverage, resumable commits. | Disabled/fixture source and bounded coordinator. | Isolated shadow recovery; no production projection changes. |
| 4. Safe activation | Atomic projection, rewind, promotion, and all financial gates. | Balance/operation integration and activation fixtures. | Per-account private activation and spending exercised with fixtures. |
| 5. History integration | Evidence-backed history reads and detail state. | Partial-history classification, FFI, and UI. | Mixed transactions and restored history represented accurately. |
| 6. Qualification | Lifecycle/failure evidence and repair compatibility. | Cross-repository regression and request-capture results. | Preparatory refactor complete; real PIR integration still gated. |

For each handoff:

1. Record the tested wallet-libraries commit and enabled features.
2. Update Vizor's related dependency pins and lockfile consistently; verify the
   resolved graph rather than assuming all packages use the same revision.
3. Run the phase's consumer checks against that revision. Keep dependency or
   consensus changes unrelated to this refactor visible as separate changes.
4. Record the gate result and remaining limitations. Do not enable production
   private authority merely because a pin, schema, or fixture test is present.

For library paths below, `backend/` means
`librustzcash/zcash_client_backend/src/` and `sqlite/` means
`librustzcash/zcash_client_sqlite/src/`. Vizor paths are relative to its repository
root. New modules are marked proposed. Reuse existing transport and database
boundaries; do not create a general sync plugin framework or a second
authoritative database.

## Phase 0 — Establish the implementation baseline

Planning snapshot: wallet-libraries `1709e4dd3d` on
`docs/transparent-pir-design`; Vizor `a3a2683ef` on
`roman/ironwood-memo-pir`, with wallet-libraries dependencies pinned to
`2e206f7894`. Recheck both checkouts, dirty work, dependency graphs, and feature
configuration before implementation. A newer library checkout is not evidence
that Vizor already runs its behavior.

**Wallet-libraries steps**

1. Map the contract onto `backend/data_api.rs` and `backend/data_api/`, plus
   `sqlite/lib.rs`, `sqlite/wallet.rs`, `sqlite/wallet/transparent.rs`, and
   `sqlite/wallet/init/`.
2. Inventory transparent selectors, transaction/proposal consumption, rewind and
   rescan entry points, local-send ingestion, and enhancement/status routing.
   Identify where shared transaction origins must survive projection rollback.
3. Define synthetic reference fixtures for receives, spends, empty ranges,
   coinbase, mixed transactions, local sends, and imported/hardware accounts.
   Derive expected effects independently of the candidate ledger implementation.

**Vizor steps**

1. Map DB constructors in `rust/src/wallet/db.rs` and their exceptions; include
   foreground, read-only, transaction, native/background, and pre-DB import or
   preview entry points.
2. Inventory UTXO refresh, `ledger_discovery.rs`, `address_history.rs`, and
   enhancement auxiliary lanes under `rust/src/wallet/sync_engine/`, plus
   `rust/src/wallet/transaction_data/`. Record current source authorization.
3. Record reference balances, known local payment details, pending sends, locks,
   and account/address behavior using fixtures. Run the relevant existing
   tests and distinguish pre-existing failures from refactor regressions.

**Exit gate:** both repositories have a concrete call-site and fixture inventory,
with an agreed dependency baseline. No source, schema, or authority change.

## Phase 1 — Add the contract, schema, and configured handles

**Wallet-libraries steps**

1. Add proposed `backend/data_api/transparent_ledger.rs` with `ChainPoint`,
   `TransparentLedgerMode`, `TransparentLedgerRead/Write`,
   `TransparentLedgerSnapshot<AccountId>`, normalized commit/context types,
   and guarded promotion signatures. Reuse wallet account/error types and keep
   protocol layouts, HTTP, and PIR-client types out of the API.
2. Define history completeness separately from the transparent balance snapshot:
   owned effects, provisional classification, recipients, per-output memos,
   optional fees/provenance, and mining/status evidence. Define contracts now;
   implement the complete read path in Phase 5.
3. Add seedless, additive migrations through `sqlite/wallet/init.rs` and
   `sqlite/wallet/init/migrations/`. Establish `tpir_*` policy/account state,
   scripts, event observations, coverage, pending work, and projection origins.
   Classify existing remote rows as legacy evidence, never private coverage.
4. Preserve local transaction bytes, sent outputs, known recipients/fees/grouping,
   outboxes, locks, reservations, and independent shielded/payload origins.
   Do not infer local creation from a row's presence.
5. Add explicit handle configuration and inheritance in `WalletDb`. New APIs
   reject unconfigured use, including an empty DB. Until Phase 4,
   `PrivateRequired` rejects promotion and transparent financial authorization
   as unavailable; incomplete implementations cannot fabricate successful
   coverage or a spendable private transparent balance.

**Vizor steps**

1. Upgrade the dependency pin/lockfile to this library revision and exercise the
   migration using existing wallet initialization paths, including imported-only
   and hardware-first databases without access to a seed.
2. Configure the new transparent mode on every relevant handle from the Phase 0
   inventory. Preparatory production builds explicitly retain public transparent
   authority and the existing Enhance/Status behavior; development fixtures can
   request stricter modes.
3. Keep the saved user setting intact. Never overwrite a durably applied
   `PrivateRequired` policy with `Public` because a build lacks support.
   Such a database must remain blocked or use a supported privacy-aware release.

**Exit gate:** representative databases upgrade without seeds, changed public
balances, or lost local history. Transactional/reopened handles retain the
required configuration; unconfigured/private-unavailable paths fail explicitly.
Migration interruption and storage failures fabricate neither coverage nor
history completeness.

## Phase 2 — Enforce privacy before adding recovery networking

**Wallet-libraries steps**

1. Implement durable applied policy, generations, and compatibility requirements.
   A transition revokes stale operation contexts; commit checks must reject
   obsolete policy generations even when an older handle remains open.
2. Audit work creation and routing in `backend/data_api/enhance_pir/`,
   `sqlite/wallet/enhance_pir.rs`, and status/transaction-retrieval APIs. Under
   `PrivateRequired`, mixed results such as `has_transparent` or
   `LwdRequired` must yield explicit pending/unsupported private details,
   without deleting financial facts or authorizing a public request.
3. Keep transaction/action/output detail work independent. A recovered memo,
   unsupported payload, or incomplete status response cannot complete another
   obligation or establish ledger coverage. Preserve existing trusted local
   evidence rules for status and expiry.

**Vizor steps**

1. Extend the existing policy in
   `rust/src/wallet/sync_engine/enhancement/policy.rs`,
   `lib/src/providers/enhance_pir_provider.dart`, and
   `lib/src/core/storage/enhance_pir_preference_store.dart`. Extend the paused
   setting-transition flow; reconcile preference and database policy
   conservatively before dispatch and capture one immutable policy per operation.
2. Guard UTXO refresh, Ledger/address-history discovery, software account
   discovery, and previews. Resolve the same policy before pre-DB requests;
   unsupported private previews report unavailable.
3. Guard payload, fee/parent, and status dispatch from either discovery loop,
   including already queued public work and retries. Shielded-first mixed
   discovery must be protected before the transparent ledger sees the txid;
   server shape flags cannot grant disclosure authority.
4. Apply the policy to `rust/src/ffi.rs`, mobile/native preparation adapters,
   and their reopened handles. Preserve foreground handoff where a background
   task cannot establish the required private anchor. Reuse existing route,
   cancellation, and deadline handling.
5. Add request-capture tests for private transitions and unsupported sources
   before the coordinator can issue requests. Production transparent authority
   remains public during preparation; the stricter path is exercised with
   test/development policy, not a new user setting.

**Exit gate:** private fixtures emit no unauthorized address, script, outpoint,
or txid lookup through any inventoried entry point. Test startup, interrupted
setting writes, cancellation, stale queues/handles, mixed shape flags, and
missing configuration. Explicit public behavior remains covered by regression
tests. No real transparent PIR client is needed to pass this gate.

## Phase 3 — Recover an isolated candidate ledger

**Wallet-libraries steps**

1. Implement proposed `sqlite/wallet/transparent_ledger.rs` storage and
   `apply_transparent_ledger_commit` for candidate state. Enumerate owned
   scripts with account/scope, conservative recovery bounds, and watch-set
   generations; unknown starts require recovery from genesis.
2. Persist immutable receive/spend content separately from mined placement and
   publication observations. Accept idempotent replay, retain spends received
   before their outputs, and reject contradictory content or canonical spends.
3. Commit source-bound coverage, negative filter results, pending pages, and
   candidate address-window progress atomically. Partial pages, missing anchors,
   unsupported scripts, or unresolved spends cannot certify completeness.
4. Validate policy/account/watch/chain context on every commit. Handle candidate
   rewind, account deletion/import, and earlier recovery bounds now; restart
   must resume durable work, not infer completion from absent rows.
5. Keep candidate writes isolated from LRZ balances, spend links, locks,
   production address-use flags, and receive-address selection. Read candidate
   diagnostics through the library, never by treating the projection as evidence.

**Vizor steps**

1. Add a proposed `rust/src/wallet/sync_engine/transparent_ledger.rs`
   coordinator and a narrow source boundary returning normalized results.
   Implement disabled and deterministic fixture sources only. Disabled returns
   unavailable; fixtures cannot qualify a production account.
2. Capture a fixed accepted contiguous chain point and operation context,
   enumerate scripts, call the source under query/byte/page/time bounds, and
   commit through the library. Repeat on window growth at the same target.
   Hold no write lock across network/source I/O.
3. Schedule alongside shielded scanning while keeping separate checkpoints,
   queues, retries, and completion. In shadow mode retain the public projection
   and `.receive.redb` cache as before; neither becomes private evidence.
4. Add local exact-set comparison against fixtures at a common chain point.
   Expose only development diagnostics at this stage, with candidate amounts
   explicitly unverified and potentially above or below the real balance.

**Exit gate:** fixture receives/spends, empty ranges, partial pages, cancellation,
window growth, and restart converge to expected candidate state. Reorg/import
races reject stale commits. Shadow changes no production balances, selection,
locks, address allocation, or history.

## Phase 4 — Activate safely and enforce every financial path

Projection, rewind, promotion, and financial gating form one release boundary.
They may be implemented in smaller changes. Keep successful promotion unavailable
outside focused tests until those library components pass together; only then
let Vizor's fixture coordinator exercise activation.

**Wallet-libraries steps**

1. Project candidate events into existing transaction/output/spend structures.
   Join mixed transactions by txid with pool-specific identities, preserving
   independent local, shielded, and payload contributions. Keep coinbase
   classification explicit even without raw bytes or transaction indices.
2. Implement authoritative commits and projection in one SQLite transaction.
   Add rollback to every applicable height/chain-state truncation and rescan
   path using its actual retained height. Invalidate evidence, coverage, work,
   and derived state without deleting valid independent origins or issued-address
   history.
3. Implement guarded per-account promotion: recheck complete watch-set coverage,
   accepted anchors, stable window expansion, resolved spends/pages, production
   source qualification, and explained legacy discrepancies. Atomically project,
   merge local overlays, switch authority, and revoke candidate work. Allow
   fixture promotion only through test/development paths.
4. Implement the atomic ledger snapshot, distinguishing authorized, last-known,
   and recovered-unverified amounts. Historical snapshots and incomplete net
   amounts cannot authorize a current spend.
5. Enforce eligibility inside existing individual, address, batched, and
   value-bounded transparent selectors, plus proposal consumption and hardware
   finalization. Coverage through accepted `H` supports target `H + 1`;
   recheck the current chain and retain confirmations, maturity, spend,
   reservation, and lock rules. No freshness tolerance or alternate selector.
6. Preserve legitimate same-proposal chained outputs through local evidence.
   Block incomplete transparent inputs without blocking independently eligible
   shielded-funded unshielding. Its resulting own transparent outputs must pass
   transparent eligibility before later spending.

**Vizor steps**

1. Consume the snapshot in balance/summary and operation availability paths,
   including `rust/src/wallet/wallet_summary_cache.rs`. Show unavailable or
   last-known amounts accurately; invalidate caches on commits, policy,
   promotion, rewind, and account lifecycle changes.
2. Integrate software send/shielding, `rust/src/wallet/sync/send.rs`,
   `rust/src/wallet/sync/pczt.rs`, and Ledger/Keystone completion paths with
   the existing library selectors and revalidation rules. Preserve the current
   signing, outbox, persistence, and broadcast lifecycle; a UI precheck never
   substitutes for authorization.
3. Exercise private activation with fixtures: immediately stop public
   transparent discovery, leave incomplete accounts unavailable, promote ready
   accounts independently, and handle lag or outage without source fallback.
   Reuse shadow state only after revalidating its full activation context.

**Exit gate:** commit/promotion/rewind failpoints and WAL restart leave a
consistent projection and authority. Both discovery orders and payload replay
preserve mixed effects. All transparent selectors and stale proposals reject
incomplete coverage; coinbase, locks, local chains, shielded-funded unshielding,
account isolation, and explicit public behavior have focused regression tests.

## Phase 5 — Integrate complete and partial transaction history

**Wallet-libraries steps**

1. Implement the Phase 1 history read contract using existing transaction/output
   views where possible. Read effects and completeness consistently, retaining
   account/scope, local intent, optional metadata, and source provenance.
2. Tie any stored detail-completion markers to evidence and supported
   capabilities. Keep missing recipients, per-output memos, fees, and status
   distinct from missing owned-effect coverage. Empty queues are not proof of
   complete history.
3. Invalidate derived classifications/detail state with affected evidence on
   rewind, promotion, account changes, and later enhancement. Preserve richer
   independently recorded local-send details when partial discoveries arrive.

**Vizor steps**

1. Update `rust/src/wallet/sync/transactions.rs`: make
   `HISTORY_BASES_CTE` classification completeness-aware, remove unknown-fee
   coalescing in `read_history_bases`, and fix `classify_history_tx` so
   missing external outputs cannot hide a known debit with change.
2. Carry recovery state and optional fields through flat Rust API/Flutter
   results. Regenerate bindings with `scripts/generate-rust-bridge.sh` when
   the API changes; adapt providers and activity/details views together.
3. Keep transaction identity stable while details arrive. Represent shielding,
   self-unshielding, mixed payments, and cross-account effects without duplicate
   accounting. A provisional net debit is not a final recipient amount.
4. Show known activity with incomplete details instead of invented zero fees
   or recipients. Preserve local TEX grouping; without sufficient restored
   grouping evidence, retain the individual transactions. Invalidate history
   and summary caches when discovery or enhancement changes these results.

**Exit gate:** Rust and Dart fixtures cover preserved local history and seed
restoration for shielding, self/external unshielding, mixed inputs, and
cross-account transfers. Test both arrival orders, missing external outputs
with change, unknown fees/memos, and later enrichment. Known debits remain
visible, self-transfers are not double-counted, and no history gap triggers
public enrichment.

## Phase 6 — Qualify migration, restart, and cross-repository behavior

**Wallet-libraries steps**

1. Run the independent block-derived oracle over exact receive/spend, UTXO,
   balance, coverage, and projected-effect results. Legacy parity alone does
   not qualify correctness or completeness.
2. Exercise fresh, long-lived, multi-seed, imported-only, and hardware-first
   migration fixtures, including pending sends, reservations, and prior rewinds.
3. Inject commit/promotion/rewind failures, disk-full/storage errors, and restart.
   Cover same-height and sealed/provisional reorgs, re-mining, account deletion,
   earlier bounds, and stale generations.
4. Validate forward repair and the designated privacy-aware rollback release.
   Preserve local evidence and applied private policy; arbitrary historical
   binaries are not supported rollback targets.

**Vizor steps**

1. Run the fixture coordinator through the integrated Rust/Flutter interfaces:
   public-to-shadow, initial private recovery, qualified shadow reuse, per-account
   promotion, interrupted activation, lag, outage, and restart.
2. Capture requests across sync, import, preview, startup, fee/payload/status
   work, and native boundaries. Include setting races, shielded-first mixed
   discovery, stale queued public work, and server shape flags.
3. Verify that balances, visible history, operation availability, and caches agree
   after both discovery orders, payload replay, account changes, and rewinds.
   Preserve local sends and hardware-flow state throughout.
4. Record exact library/application revisions, feature configuration, fixture
   provenance, gate outcomes, and remaining unsupported details. Keep sensitive
   comparisons local and export only redacted aggregate diagnostics.

**Exit gate:** no unexplained qualifying discrepancies, shadow financial side
effects, unauthorized public requests, duplicate accounting, or lost local
history. Restart/repair behavior and the supported rollback release have direct
evidence. This completes preparation, not remote-service or production-private
qualification.

## Validation and completion boundary

Run focused backend/SQLite tests with transparent support and applicable
existing balance, selection, migration, status, and enhancement regressions.
For each consumer handoff, validate the actual dependency graph and run the
relevant Vizor Rust and Dart tests. Use FVM for Flutter; run mobile-tagged tests
with the repository's mobile form-factor define when changing mobile UI.
Heavy regtest/device runs require a separately scheduled explicit request and
remain release gates; unit/fixture success does not imply they passed.

Preparation is complete when Phases 1–6 pass against an identified pair of
library/consumer revisions: deterministic recovery can resume, promote, project,
authorize or block inputs, reconstruct honest history, and rewind through the
intended APIs while shared-policy transitions cover all disclosure paths.

The next stage adds real filters, manifests, shards, PIR retrieval, publication
verification, protocol known-answer/malformed-input tests, real-source shadow
qualification, a controlled private cohort, and measured mobile/network-route
behavior. No phase here removes the public source or enables production private
authority.

Full private reconstruction of external transparent recipients requires an
additional payload/summary capability with identity binding, supported-pool
coverage, and explicit fee evidence. It has its own
[capability gate](transparent-pir-ledger-architecture.md#capability-and-rollout-boundary).
This plan requires honest partial history and blocks unauthorized fallback;
it does not claim full seed-restored recipient/memo/fee parity, cryptographic
completeness proofs, or support for privacy-unaware rollback binaries.
