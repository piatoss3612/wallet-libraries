# Preparatory refactor for a transparent PIR ledger

Status: proposed implementation sequence

## Purpose

Prepare wallet-libraries and the wallet application for transparent PIR without
introducing protocol, service, and database changes in one step.

The refactor is intentionally goal-oriented. It establishes only the boundaries
needed for a private transparent ledger:

- a product-neutral ledger contract;
- atomic event and coverage storage;
- an LRZ projection boundary;
- coverage-aware financial reads;
- centralized rewind integration;
- explicit source authority; and
- protocol-neutral application scheduling and transport.

Before adding real PIR networking, a deterministic fixture ledger should be able
to drive transparent balances, history, spendability, restart, and rewind end to
end.

## Intermediate architecture

```text
application sync session
        |
        +-- immutable privacy policy
        |
        +-- private retrieval runtime
        |
        +-- transparent ledger coordinator
                        |
                        v
               TransparentLedger API
                        |
                        v
             LRZ SQLite implementation
                 events + coverage
                        |
                        v
                 LRZ read projection
```

The preparatory work does not select private authority or depend on a deployed
PIR service. It makes the eventual protocol integration a new validated event
source rather than a wallet-wide restructuring.

## Refactor principles

1. Preserve observable production behavior until an explicit authority change.
2. Make schema changes additive and migration-safe.
3. Keep protocol types out of wallet-facing LRZ APIs.
4. Put financial invariants in wallet-libraries rather than application code.
5. Put policy, networking, cancellation, and UI state in the application.
6. Exercise every boundary with fixture events before using a remote service.
7. Keep balance, coverage, and spendability inseparable at the financial API.
8. Require explicit source configuration; missing configuration is an error.

## 1. Introduce the wallet contract

Add a product-neutral module such as:

```text
zcash_client_backend::data_api::transparent_ledger
```

It defines:

```rust
pub trait TransparentLedgerRead {
    type Error;

    fn transparent_ledger_status(
        &self,
        account: AccountId,
    ) -> Result<TransparentLedgerStatus, Self::Error>;

    fn transparent_balance_snapshot(
        &self,
        account: AccountId,
        target: BlockHeight,
    ) -> Result<TransparentBalanceSnapshot, Self::Error>;

    fn transparent_spendable_outputs(
        &self,
        account: AccountId,
        target: BlockHeight,
        request: SpendRequest,
    ) -> Result<Vec<TransparentInput>, Self::Error>;
}

pub trait TransparentLedgerWrite {
    type Error;

    fn apply_transparent_ledger_commit(
        &mut self,
        commit: TransparentLedgerCommit,
    ) -> Result<(), Self::Error>;

    fn rewind_transparent_ledger(
        &mut self,
        height: BlockHeight,
    ) -> Result<(), Self::Error>;
}
```

The types represent wallet concepts rather than a particular wire format:

- `TransparentLedgerMode`
- `TransparentLedgerStatus`
- `TransparentBalanceSnapshot`
- `TransparentLedgerCommit`
- `TransparentReceiveEvent`
- `TransparentSpendEvent`
- `TransparentCoverage`
- `TransparentCompletion`

No HTTP, endpoint, filter, shard, or PIR client type appears in this contract.

## 2. Add the durable schema

Add the transparent ledger tables through normal SQLite migrations. Initial
migrations are additive and do not change source authority.

The migration introduces:

- metadata and publication identity;
- watched scripts and recovery bounds;
- receive and spend events;
- settled and provisional coverage;
- resumable pending work;
- projection provenance; and
- shadow comparison summaries.

Migration tests use copies or representative fixtures for:

- fresh databases;
- long-lived databases;
- multiple accounts;
- transparent outputs and spends;
- pending transactions;
- imported accounts; and
- databases previously rewound.

Interrupted migration, storage exhaustion, and restart behavior are tested.

## 3. Implement one event-projection entry point

LRZ gains one transaction-level operation that applies a complete ledger commit.
It is responsible for:

- validating stable event identities;
- detecting contradictory replays;
- storing ledger events and coverage;
- retaining unresolved spends;
- creating transaction summaries;
- projecting receives and spends;
- updating address-use metadata;
- expanding address windows;
- preserving explicit coinbase status; and
- recording projection provenance.

Protocol and application code do not write LRZ transparent tables directly.

The first caller is a deterministic fixture source. This permits database and
wallet behavior to be proven before protocol behavior is involved.

## 4. Make coverage part of financial reads

Introduce a balance result that always carries its authority state:

```rust
pub struct TransparentBalanceSnapshot {
    pub balance: AccountBalance,
    pub target_height: BlockHeight,
    pub covered_through: Option<BlockHeight>,
    pub settled_through: Option<BlockHeight>,
    pub completion: TransparentCompletion,
    pub unresolved_spends: usize,
    pub unsupported_scripts: usize,
}
```

Add a centralized spendability result:

```rust
pub enum TransparentSpendability {
    Ready,
    CoverageIncomplete,
    PublicationBehind,
    RecoveryInProgress,
    UnresolvedSpends,
    UnsupportedScripts,
    ChainUnknown,
    IntegrityFailure,
}
```

Transparent input selection and shielding consult this gate. They do not derive
authority from a balance alone.

During the preparatory stage, compatibility configuration preserves current
production behavior. Private financial authority is not enabled by this step.

## 5. Centralize chain rewinds

All wallet rewinds pass through one transactional path that updates:

- accepted chain state;
- transparent ledger events;
- transparent coverage;
- pending transparent work;
- LRZ projection rows; and
- address-use state.

After a successful rewind, no event or coverage row may depend on a block above
the retained height.

Tests cover:

- events entirely above the rewind;
- coverage crossing the rewind;
- provisional tail replacement;
- an unresolved spend crossing the rewind;
- locally originated projection state;
- failure in the middle of rollback; and
- restart after a committed rollback.

The application does not manually invalidate transparent state around individual
rewind call sites.

## 6. Add explicit source authority

Every wallet handle that can enumerate or apply transparent discovery work is
configured with:

```rust
pub enum TransparentLedgerMode {
    Public,
    PrivateShadow,
    PrivateRequired,
}
```

An unconfigured handle returns `ModeNotConfigured`, including for an empty
wallet. A transactional handle inherits its parent's mode; a reopened handle is
configured again by its caller.

The mode expresses authorization, not service availability. Service failure does
not mutate it.

The preparatory sequence introduces the type and configuration checks while
retaining the production authority selected by the application.

## 7. Create a protocol-neutral private runtime

The application should provide common private-retrieval infrastructure:

```text
sync_engine/private_retrieval/
    policy.rs
    route.rs
    transport.rs
    cancellation.rs
    observability.rs
    errors.rs
```

It supplies:

- one immutable policy per synchronization session;
- direct or anonymity-network routing;
- HTTPS enforcement;
- request and response size bounds;
- deadlines;
- cancellation before, during, and after dispatch;
- redacted logging and metrics;
- revision-conflict handling; and
- distinct service, integrity, and local-storage errors.

Protocol-specific components remain siblings above the shared runtime. The
transparent ledger does not share its work queue or completion state with
payload or status recovery.

## 8. Introduce the transparent ledger coordinator

The application adds a dedicated synchronization phase behind a source trait:

```rust
pub trait TransparentLedgerSource {
    async fn synchronize(
        &mut self,
        request: TransparentSyncRequest,
        sink: &mut dyn TransparentLedgerSink,
    ) -> Result<TransparentSyncReport, TransparentSyncError>;
}
```

Initial sources are:

- `DisabledTransparentSource`; and
- `FixtureTransparentSource`.

The coordinator:

1. captures the session policy;
2. captures a fixed locally accepted target;
3. obtains watched scripts and recovery bounds;
4. invokes the selected source;
5. commits results through the LRZ ledger API;
6. repeats when address-window growth creates new scripts;
7. publishes an explicit completion state; and
8. updates financial-operation availability.

No real private network service is necessary to validate this lifecycle.

## 9. Add user-visible recovery state

The application represents transparent recovery independently of general chain
synchronization:

```text
Not configured
Recovering
Partially covered
Current
Publication behind
Service unavailable
Unsupported history
Integrity failure
```

A partially recovered amount may be displayed, but the UI communicates its
coverage and whether spending or shielding is available.

The UI model is added before private authority so protocol integration does not
have to fit into a binary success/failure interface.

## 10. Add fixture and contract testing

Before real PIR integration, the fixture source must demonstrate:

1. a receive increases the projected balance;
2. a spend decreases it;
3. a spend received before its output is retained and later attaches;
4. incomplete coverage prevents transparent input selection;
5. complete coverage permits otherwise eligible selection;
6. state survives process restart;
7. replay is idempotent;
8. contradictory replay is rejected;
9. a rewind removes invalid events, coverage, and projection atomically;
10. address-window growth creates new script work; and
11. no source-specific code writes LRZ projection tables directly.

These tests become the storage contract used by the eventual PIR source.

## Proposed pull-request sequence

### PR 1: Ledger contract

- Add product-neutral types and traits.
- Document authority and coverage semantics.
- Make no behavioral change.

### PR 2: Additive schema

- Add ledger tables and projection provenance.
- Add migration and restart tests.
- Make no authority change.

### PR 3: Atomic event projection

- Implement receive and spend projection.
- Preserve explicit coinbase status.
- Add idempotency, contradiction, and spend-before-receive tests.

### PR 4: Coverage-aware reads

- Add balance-with-coverage.
- Add spendability status.
- Route transparent input selection and shielding eligibility through the
  centralized gate while preserving configured production behavior.

### PR 5: Transactional rewind integration

- Integrate ledger rollback into the central rewind transaction.
- Add reorg, failpoint, restart, and provenance tests.

### PR 6: Private runtime boundary

- Introduce immutable session policy.
- Extract protocol-neutral transport, cancellation, and observability.
- Preserve behavior.

### PR 7: Transparent ledger coordinator

- Add the dedicated synchronization phase.
- Integrate disabled and fixture sources.
- Add bounded repetition for address-window growth.
- Expose recovery state to the application.

### PR 8: Shadow comparison framework

- Compare exact events, outputs, spends, UTXOs, and balances locally.
- Record only redacted aggregate mismatch information.
- Add release-readiness reporting without changing financial authority.

Real filter, shard, PIR, and publication integration follows after these seams
and invariants are demonstrated.

## Out of scope

The preparatory refactor does not:

- replace every LRZ balance or history query;
- move authoritative state into another database;
- introduce a general synchronization plugin framework;
- expose protocol-specific types through LRZ wallet APIs;
- change transparent source authority;
- combine payload, status, and transparent work queues; or
- remove a production source before private qualification.

## Completion criteria

The stage is ready for a real PIR source when an end-to-end test can:

1. create a wallet with a locally accepted chain;
2. feed a deterministic source a receive and spend;
3. commit them through the transparent-ledger API;
4. observe the correct LRZ balance and history;
5. prevent spending while coverage is incomplete;
6. permit spending when coverage becomes complete;
7. restart and preserve the same result;
8. rewind and atomically remove invalid ledger and projection state; and
9. complete the flow without source-specific code touching LRZ tables.

At that point, transparent PIR is primarily a new authenticated event source.
Protocol integration can focus on filters, private retrieval, publication trust,
network privacy, and production rollout instead of restructuring wallet state at
the same time.
