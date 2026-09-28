# Transparent PIR ledger architecture

Status: proposed design

## Objective

Build a private transparent-ledger subsystem that:

- recovers transparent receives and spends without revealing wallet addresses,
  scripts, outpoints, or transaction identifiers;
- maintains an authoritative transparent UTXO ledger;
- integrates safely with LRZ balances, transaction history, sending, shielding,
  account lifecycle, and chain rewinds;
- remains architecturally separate from LRZ except at a small, explicit
  projection boundary;
- never silently weakens privacy when a private service is unavailable; and
- supports bounded recovery, crash-safe resumption, and staged production
  qualification.

The central invariant is:

> A transparent balance is authoritative only when validated ledger events,
> continuous per-script coverage, the locally accepted chain, and the LRZ
> projection agree in one durable database state.

## System boundary

```text
                         wallet application
                policy, scheduling, UI, networking
                               |
                               v
                 transparent PIR sync session
                               |
               +---------------+---------------+
               |                               |
               v                               v
        public filter source             private shard source
        all filters downloaded           PIR retrieval for
        and matched locally              matched ranges only
               |                               |
               +---------------+---------------+
                               |
                               v
                  transparent ledger engine
                 events, coverage, pending work
                               |
                               v
                    durable ledger storage
                               |
                               v
                       LRZ read projection
              balances, history, sends, shielding
```

The transparent ledger is a financial-discovery subsystem. Transaction payload
recovery and transaction status observation remain separate concerns with their
own work, completion, and failure semantics. They may share transport and
cancellation infrastructure, but they do not share queues or source authority.

## Trust and privacy model

The protocol protects the requested table row. It does not hide all access
patterns.

The filter source receives requests for public data that is identical for every
wallet covering the same chain range. The wallet downloads every applicable
filter and matches its scripts locally. The filter source therefore does not
learn which filters matched.

The private shard source may observe:

- approximate chain ranges containing probable wallet activity;
- query counts and timing;
- response sizes; and
- the client's network origin unless an anonymity route is used.

The private request must not contain a wallet address, script, outpoint, or
transaction identifier.

The publication service is trusted for completeness. Content digests, manifests,
record validation, and accepted-chain checks can detect corruption, stale data,
mixed revisions, and events bound to the wrong chain. They cannot prove that an
otherwise self-consistent publication contains every chain event.

Production qualification should therefore include an independent publication
verifier that:

1. reads blocks from an independently operated node;
2. reconstructs the expected receive and spend event sets;
3. compares event counts and deterministic digests for every shard;
4. refuses publication promotion on any mismatch; and
5. records an auditable result tied to the publication digest.

## Component ownership

### Protocol layer

The protocol layer owns:

- filter encoding and matching;
- shard maps and manifests;
- PIR request generation and response verification;
- receive and spend event encodings;
- publication revisions;
- schema and layout identifiers; and
- query and byte limits.

It is independent of wallet accounts, SQLite schemas, UI state, and product
preferences.

### Transparent ledger layer

The ledger layer owns:

- watched scripts and their individual recovery start heights;
- receive and spend events;
- continuous settled and provisional coverage;
- resumable page retrieval;
- unresolved spends;
- unsupported-script accounting;
- publication identity; and
- rollback of state no longer supported by the accepted chain.

This layer is the authoritative source for remotely discovered transparent
activity.

### LRZ adapter

The LRZ adapter has a deliberately narrow role:

- enumerate wallet-owned scripts;
- supply locally accepted block hashes;
- atomically persist ledger commits;
- project ledger events into wallet-readable transaction, output, and spend
  records;
- roll the ledger back with chain rewinds; and
- enforce coverage when selecting transparent inputs.

It does not own PIR cryptography, HTTP behavior, endpoint policy, or retry
scheduling.

### Application integration

The application owns:

- selection of public, shadow, or private-required authority;
- filter and shard endpoints;
- direct or anonymity-network routing;
- cancellation and deadlines;
- synchronization scheduling;
- progress and user-visible recovery state; and
- production release gates.

## Wallet-facing contract

The product-neutral wallet API should expose balance and coverage together.

```rust
pub struct TransparentLedgerSnapshot {
    pub target_height: BlockHeight,
    pub covered_through: Option<BlockHeight>,
    pub settled_through: Option<BlockHeight>,
    pub balance: AccountBalance,
    pub completion: TransparentCompletion,
    pub unresolved_spends: usize,
    pub unsupported_scripts: usize,
}
```

A plain balance is insufficient. A covered zero, an uncovered zero, and a
partially recovered nonzero balance have different financial meaning.

Source authority is explicit:

```rust
pub enum TransparentLedgerMode {
    Public,
    PrivateShadow,
    PrivateRequired,
}
```

An unconfigured wallet handle returns an error rather than choosing a default.
The selected mode is captured once for a synchronization session.

`PrivateShadow` is a qualification mode, not a privacy claim. Its ledger does
not control displayed balances or input selection. `PrivateRequired` authorizes
only the private transparent path; a private failure never changes that decision.

## Scripts and recovery bounds

Coverage is tracked per script. Each script has a `required_from` height:

- a derived script normally starts at its account recovery birthday;
- an imported script uses an authenticated supplied birthday when available;
- an imported script with no known start is explicitly marked unknown and is
  recovered from genesis or the earliest published height.

`required_from` may move earlier but never later. Deriving a script today must
not grant it coverage over earlier blocks that were never checked.

If publication begins after a script's required history start, the wallet
remains incomplete even if every available shard was processed successfully.

## Event model

A receive event contains at least:

```rust
pub struct TransparentReceiveEvent {
    pub txid: TxId,
    pub output_index: u32,
    pub script: Vec<u8>,
    pub value: Zatoshis,
    pub mined_height: BlockHeight,
    pub is_coinbase: bool,
}
```

A spend event contains at least:

```rust
pub struct TransparentSpendEvent {
    pub spending_txid: TxId,
    pub input_index: u32,
    pub spent_outpoint: OutPoint,
    pub mined_height: BlockHeight,
}
```

Every event also retains its shard, revision, accepted-chain anchor, and encoded
record or record digest.

Stable identities are:

```text
receive = transaction identifier + output index
spend   = spending transaction identifier + input index + spent outpoint
```

Reapplying an identical event is a no-op. Different contents for an existing
identity are an integrity failure.

A spend whose receive is not yet known is retained as unresolved. It attaches
when the receive is later recovered. Unresolved spends prevent the affected
account from being considered financially complete.

## Durable state and atomic projection

Ledger state is logically isolated under a dedicated table namespace, for
example:

```text
tpir_meta
tpir_scripts
tpir_coverage
tpir_receive_events
tpir_spend_events
tpir_pending_pages
tpir_projection_origins
tpir_shadow_runs
```

Durable ledger state and the LRZ read projection live in the same physical
SQLite wallet database. One shard commit atomically writes:

- validated receive and spend events;
- per-script coverage;
- pending page progress;
- publication and anchor metadata;
- address-use and gap-window changes; and
- the corresponding LRZ projection.

Either all of these changes commit or none do. A separate attached database is
not used for authoritative state because financial correctness must not depend
on cross-database crash atomicity.

Large re-downloadable objects may live in a disposable cache, including public
filters, manifests, and PIR setup material. Losing that cache may cost bandwidth
but cannot change balance or coverage.

Projection provenance distinguishes events recovered by the ledger from state
recorded by local transaction construction or an independently authorized full
transaction payload. The ledger never ingests its own projection as new
evidence.

The projection supports transaction summaries without requiring raw transaction
bytes. Missing raw bytes do not authorize a public transaction lookup.

## Coverage and spendability

Coverage binds:

- one script;
- a start and end height;
- a shard and publication revision;
- the terminal locally accepted block hash;
- whether the range is sealed or provisional; and
- the original publication anchor.

`settled_through` includes only continuous sealed coverage. `covered_through`
may additionally include a validated provisional tail.

An output is eligible for spending or shielding only when:

- its receive is on the locally accepted chain;
- its script has continuous coverage through the decision target;
- the target height and hash are locally accepted;
- no relevant pending pages remain;
- no unresolved spend can affect the account;
- no unsupported script can affect the account;
- coinbase status and maturity are explicit; and
- the output is not already locked, reserved, or spent.

There is no freshness tolerance for financial authorization. One uncovered
block can contain a spend of an otherwise attractive input.

## Synchronization algorithm

Each run uses one fixed target: the highest contiguous locally scanned block,
including both height and hash.

1. Capture the immutable session policy.
2. Validate network and endpoint configuration before dispatch.
3. Capture the locally accepted target.
4. Fetch the publication map.
5. Validate network, genesis, schema, layout, and set lineage.
6. Enumerate watched scripts and their immutable recovery bounds.
7. Validate stored coverage against local accepted block hashes.
8. Roll back state no longer supported by the accepted chain.
9. Download every applicable public filter.
10. Match wallet scripts locally.
11. Query matching shards through private retrieval.
12. Commit each validated shard atomically.
13. Expand derived-address windows when activity reaches an edge.
14. Repeat script enumeration until stable or a bound is reached.
15. Persist an explicit completion state.

The target does not move during the run. A publication refresh can replace
publication data but cannot change the wallet's decision height.

No wallet write lock is held across network I/O.

## Rewinds

Ledger rollback participates in the same database transaction as the wallet's
chain rewind. It removes or invalidates:

- events above the retained height;
- coverage crossing the rewind;
- provisional coverage tied to a replaced tail;
- pending work tied to invalid anchors; and
- corresponding projection records.

State with independent local provenance follows its own wallet rules and is not
deleted merely because a PIR-derived projection is rolled back.

If the exact retained ancestor cannot be established, unverifiable coverage is
dropped rather than assigned an assumed hash.

## Failure semantics

### Inconclusive recovery

Network timeouts, overload, publication lag, or budget exhaustion preserve
committed progress and leave the ledger incomplete. They are retried later and
never authorize a source change.

### Integrity failure

Wrong network identity, unsupported schema, digest mismatch, mixed revision,
contradictory event, or accepted-chain mismatch rejects the affected commit and
ends trust in that session.

### Local wallet failure

Database, migration, or projection failure aborts the wallet operation. The
application must not report successful synchronization while durable financial
state may be inconsistent.

## Production qualification

Qualification requires more than balance equality. A fully independent block
oracle must match the ledger's exact receive set, spend set, UTXO set, balances,
coverage, and unresolved-spend state.

Required testing includes:

- known-answer and malformed protocol records;
- idempotent and contradictory commits;
- failpoints at every database statement boundary;
- process termination and WAL restart;
- receive-before-spend and spend-before-receive;
- coinbase maturity;
- imported, derived, internal, and ephemeral scripts;
- address-window expansion;
- sealed-shard and provisional-tail reorgs;
- cancellation at every network boundary;
- request capture proving that private mode discloses no wallet identifiers;
- service outage, overload, and stale-publication exercises; and
- mobile bandwidth, memory, and anonymity-route testing.

Production authority should be enabled in stages: protocol freeze, ledger and
projection implementation, shadow qualification, independent publication
verification, controlled private-required rollout, and finally general
availability.
