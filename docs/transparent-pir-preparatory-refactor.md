# Preparatory refactor for a transparent PIR ledger

Status: Phase 0 is done. Phase 1's wallet-libraries half is merged (#60–#62);
its Vizor half is pending. Phase 2's wallet-libraries half is merged (#64), and
Phase 3's is in review. The Vizor halves of Phases 2–3 and all of Phases 4–6
remain to be implemented and qualified. Production transparent
authority stays public during preparation.

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
| 0. Baseline (done) | Contract/call-site inventory and reference fixtures. | Consumer baseline and discovery/handle inventory. | Existing public behavior recorded. |
| 1. Contract and migration (library merged) | Read contract, policy/provenance schema, configured handles. | Dependency upgrade and explicit handle configuration. | Schema upgrades; private transparent input use remains unavailable. |
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

## Phase 0 — Establish the implementation baseline (done)

Phase 0 produced the call-site inventory on both sides. The library inventory
shaped Phase 1 below. The Vizor inventory, which later phases depend on, is:

- **Handles.** Every production `WalletDb` comes from the three constructors in
  `rust/src/wallet/db.rs`: `open_wallet_db_with_timeout`,
  `open_wallet_db_for_read_with_timeout`, and
  `open_wallet_db_readonly_with_timeout`. The one exception is the
  borrowed-transaction handle in `record_creation_evidence`
  (`rust/src/wallet/sync/migration.rs`). `open_wallet_raw_conn_with_timeout`
  returns a raw connection, not a `WalletDb`. Tests and examples construct
  their own handles (`rust/src/wallet/addresses/tests.rs`,
  `rust/examples/ledger_zcash_speculos_poc.rs`).
- **Transparent discovery** runs on the per-sync handle: UTXO refresh
  (`store_transparent_outputs` in `rust/src/wallet/sync_engine/mod.rs`),
  Ledger discovery (`rust/src/wallet/sync_engine/ledger_discovery.rs`), and
  transparent history
  (`rust/src/wallet/sync_engine/enhancement/auxiliary/transparent_history.rs`).
- **Pre-DB public requests**, with no `WalletDb`:
  `discover_used_software_accounts` and
  `preview_software_account_transparent_balance` in `rust/src/api/wallet.rs`.
  Phase 2 guards them.
- **Migrations** run seedless through `ensure_db_migrated_once`
  (`rust/src/wallet/keys.rs`) at startup, before the Enhance/Status policy is
  applied. Only creating the first software account passes a seed; hardware and
  observer imports are seedless.
- **Sync.** Vizor runs its own sync engine over `scan_cached_blocks` and does
  not call the backend's `sync::run`.

## Phase 1 — Add the contract, schema, and configured handles

The wallet-libraries half shipped in #60 (contract), #61 (schema and
provenance), and #62 (configured handles). It is narrower than originally
planned in some places and stricter in others; see the deviations below.

**What shipped in wallet-libraries**

1. **Contract.** `backend/data_api/transparent_ledger.rs` provides
   `ChainPoint`, `TransparentLedgerMode` (`Public`, `PrivateShadow`,
   `PrivateRequired`), `TransparentLedgerSnapshot<AccountId>`, and
   `TransparentLedgerRead` (`transparent_ledger_mode`,
   `transparent_ledger_snapshot`). The snapshot carries the authority, the
   authorized balance split into regular and coinbase, the last-known amount
   with `LegacyPublic` or `LegacyPublicAndLocal` provenance, and blockers.
   There is no write trait.
2. **Schema.** The `transparent_ledger_schema` migration is seedless and
   additive. It creates three tables:
   - `tpir_meta`: the durable policy (`applied_mode`, `policy_generation`,
     `min_reader_version`), seeded as public, generation 0, reader version 1;
   - `tpir_output_origins` and `tpir_spend_origins`: provenance, with codes
     0 = legacy public and 1 = local construction; 2 and 3 are reserved.

   It backfills legacy and local origins. From then on every transparent
   output or spend write records its origin in the same transaction, and
   outbox creation evidence adds local origins atomically. Existing remote rows
   are legacy evidence, never private coverage.
3. **Handles.** `WalletDb::set_transparent_ledger_mode` and
   `with_transparent_ledger_mode` set a per-handle mode. It is not persisted;
   `transactionally` inherits it.
4. **Fail-closed rules.**
   - An explicit mode is required for transparent input selection, storing any
     transaction with transparent inputs, `put_received_transparent_utxo`,
     transparent history requests, and the ledger APIs. Unconfigured handles
     fail. An unconfigured handle on a wallet without a private policy can
     still read balances for display.
   - A durable `PrivateRequired` is never weakened: weaker handles fail with
     `TransparentLedgerPolicyConflict`. A missing policy row or table, or a
     newer reader requirement, fails closed.
   - Transparent authority is unavailable under `PrivateRequired`, while the
     chain tip is unknown, and in builds without `transparent-inputs`. Then
     selectors and stores with transparent inputs fail,
     `get_wallet_summary` omits transparent funds,
     `get_transparent_balances` fails, and `get_received_outputs` reports
     `u32::MAX` confirmations until spendable.
   - Under `PrivateRequired`, public transparent discovery is refused, the
     backend `sync::run` skips UTXO refresh before any request (it now requires
     `TransparentLedgerRead`), and transparent history requests are withheld.
5. **CI** runs an `orchard,transparent-inputs,test-dependencies,unstable` lane.

**Deviations from the original plan**

- **Schema.** Only the policy and provenance tables exist. The recovery tables
  (scripts, event observations, coverage, pending work) move to the Phase 3
  migration.
- **Contract.** Write, commit, promotion, and history-completeness types were
  left out. Phases 3–5 add them with the code that uses them.
- **Existing APIs require configuration.** The plan had only new APIs reject
  unconfigured handles; the existing transparent APIs above do too.
- **Phase 2 gates pulled forward.** Refusing public discovery and withholding
  transparent summary and balances under `PrivateRequired` are already in the
  library. Phase 2 keeps the rest.
- **Release.** No release contains the migration yet, so it can still be
  corrected in place. Once a release includes it, it is frozen and schema
  changes need forward migrations; record it in `PUBLIC_MIGRATION_STATES`
  then.

**Vizor steps**

1. Consume wallet-libraries `main` (at least `0e0d1b128`) through
   `[patch.crates-io]` git entries for `zakura-client-backend` and
   `zakura-client-sqlite`. Other Vizor dependencies such as `zakura-pir-enhance`
   reach the backend through crates.io, so a direct git dependency would
   duplicate it. The pin also brings in the ZIP 318 schema drop (#55).
2. Configure `TransparentLedgerMode::Public` on every handle in the Phase 0
   inventory, from one mode source in
   `rust/src/wallet/sync_engine/enhancement/policy.rs`. It always returns
   `Public` in Phase 1; Phase 2 derives it from the private-queries setting.
   Development fixtures can request stricter modes.
3. Keep the saved user setting intact. Never overwrite a durably applied
   `PrivateRequired` policy with `Public` because a build lacks support; such a
   database stays blocked, and the user sees that it needs a newer build.
4. Check callers that shield or propose before the first chain-tip update,
   which now fail.
5. Extend the upgrade probes: fixtures from a pre-Phase-1 base (and the
   existing `mobile/v0.0.18` base), including a hardware-first scenario
   without a seed and raw-SQL transparent rows (a remote UTXO and a local send
   with a lock).

**Exit gate:** representative databases, including imported-only and
hardware-first ones, upgrade without seeds, changed public balances, or lost
local history. Verified with raw SQL: the `tpir_*` tables exist, `tpir_meta` is
`(0, 0, 1)`, and every transparent output and spend has an origin.
Transactional/reopened handles retain the required configuration;
unconfigured/private-unavailable paths fail explicitly, and a durable
`PrivateRequired` survives startup migration unchanged. Migration interruption
and storage failures fabricate neither coverage nor history completeness.

## Phase 2 — Enforce privacy before adding recovery networking

Phase 1 already refuses public transparent discovery and withholds transparent
history requests, summary funds, and balances under `PrivateRequired`. This
phase owns the rest: queued follow-on work such as parent-transaction retrieval
from `tx_retrieval_queue`, Status routing, durable policy transitions with
generations checked at dispatch, and the Vizor paths.

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

Phase 3 adds candidate recovery that the library owns and keeps apart from the
wallet's balances. It stores what a source reports about each watched address
and checks every commit against the current policy, account, addresses, and
chain. It never writes the LRZ tables that balances, input selection,
receiving-address allocation, and history read.

**What the wallet-libraries half adds**

1. **Watch set.** `TransparentLedgerRead::transparent_watch_set(account)`
   returns, from one read:
   - every watched address:
     - the account's rows in `addresses` (external, internal, ephemeral, and
       imported standalone keys and scripts);
     - the legacy external address, which may have no row;
     - the candidate window (see step 5);
   - the capture context: the policy generation, and the highest contiguously
     scanned local block as the target;
   - the pages earlier runs left open.

   Every address must be covered from the account birthday.
2. **Commit.** `TransparentLedgerWrite::apply_transparent_ledger_commit`
   applies one pass for one account atomically. A commit carries:
   - receive and spend events;
   - checked ranges, including ranges with no events;
   - ranges the source cannot check;
   - opened and completed pages;
   - the source revision;
   - an anchor: the highest local block the source verified the revision agrees
     with.

   Nothing in a commit may extend past the anchor, and the anchor may not
   extend past the target.
3. **Context checks.** A commit is refused, applying nothing, unless:
   - the policy is still at the captured generation;
   - the policy permits private recovery (`PrivateShadow` or
     `PrivateRequired`) both on the handle and durably;
   - the account still exists;
   - the target is still a contiguously scanned local block, and the anchor is
     still a local block;
   - every address the commit names is still watched by the account.

   Rejections are `Stale` (retry from a fresh watch set), `Integrity` (stop
   trusting the session), or `Invalid` (malformed).
4. **Evidence rules.**
   - A receive is identified by its outpoint, and a spend by its txid and input
     index. Content is immutable; a later commit can set only a placement that
     is missing.
   - A spend names the address of the output it consumes. It stays unresolved
     until that output arrives, and is checked against it.
   - Refused as integrity failures: contradictory content, a different
     placement on the local chain, and two mined spends of one output.
   - Each revision that reports an event is recorded as an observation.
   - Within a source, a higher lineage replaces a lower one. Accepting it
     removes the coverage and pages of the source's older provisional
     revisions; sealed revisions are never superseded.
   - Within one revision, supported coverage and an open page cannot overlap
     for the same address, in either order, and no range can be both checked
     and unsupported. Other revisions' pages and coverage
     are independent evidence.
   - All events of one transaction share one placement and one coinbase
     classification, and a coinbase transaction spends nothing. No commit
     anchor may lie above its revision's asserted publication height.
     Unsupported ranges block completeness until another source covers them.
5. **Candidate window.** Mined activity within a gap limit of a derived scope's
   window end extends the window in `tpir_candidate_windows`. The commit then
   reports `window_grew`. Window addresses are derived on read. They are never
   written to `addresses`, so they are never marked used or offered for
   receiving.
6. **Lifecycle.**
   - *Truncation* runs in every build, at the rescan floor: a rewind can keep a
     higher checkpoint but requeues the blocks above the floor. It clears event
     placements above the floor and removes pages opened for a later target.
     Coverage anchored above the floor is clipped to it and re-anchored there:
     the revision agreed with the old chain at its anchor, and that chain equals
     the surviving one up to the floor. Coverage is deleted when the floor block
     is unknown.
   - *Policy transitions* remove open pages.
   - *Re-attributing an imported receiver* to another account forgets the
     previous account's evidence for it.
   - *Deleting an account* removes its candidate state.
   - *Reader version.* The first candidate commit raises
     `tpir_meta.min_reader_version` to 3, so earlier builds, which would leave
     candidate state stale across rewinds, fail closed.
   - *Lowering a birthday* needs no hook: completeness is computed from the
     current birthday, so coverage from the old birthday no longer suffices.
7. **Diagnostics.** `TransparentLedgerRead::transparent_candidate_recovery`
   returns, from one read:
   - continuous coverage from the birthday (vacuous while the target is below
     it), and blockers;
   - counts;
   - the mined receives and spends, and the unspent outputs;
   - their sum, which is unverified: it can be above or below the real balance,
     and is absent when it exceeds `MAX_MONEY`.
8. **Schema.** The seedless, additive `transparent_recovery_schema` migration
   adds nine empty `tpir_*` tables:
   - `tpir_candidate_windows`, `tpir_revisions`, `tpir_coverage`;
   - `tpir_receive_events` and `tpir_spend_events`, each with its observations
     table;
   - `tpir_pending_pages` and `tpir_pending_page_scripts`;
   - indexes for the per-event lookups: coverage by script, events by account,
     and spends by prevout.

**Deviations from the original plan**

- **The bound is the birthday.** The architecture accepts a birthday only when
  it is also a justified transparent-history bound. Phase 3 uses the account
  birthday for every script, trusting a restored wallet's user-supplied
  birthday as shielded scanning does.
- **Addresses are checked one by one; there is no watch-set generation.** Each
  named address must still be watched. Addresses added mid-run are simply
  uncovered, so in-flight work survives. The coordinator repeats while
  `window_grew` is set or the watch set changes.
- **Deferred to Phase 4:**
  - durable integrity quarantine and trust epochs;
  - source qualification;
  - recovered-unverified amounts in `TransparentLedgerSnapshot`;
  - projection, promotion, and the history-completeness contract.

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
