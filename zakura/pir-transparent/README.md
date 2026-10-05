# Transparent PIR candidate recovery

The `wallet` feature bridges the reference transparent PIR client and a durable
companion SQLite store into normalized wallet-libraries recovery commits.
Applications supply a stable account binding, an origin label, a current watch
set, an independently accepted chain view, and, for every pass, their own
`FilterSource` and `ShardTransport`. No address, txid, parent or outpoint lookup
fallback exists.

The adapter makes no network requests of its own and has no HTTP client in its
normal dependency graph. The caller's transports decide routing, timeouts,
retries and response limits, and must reach the origin bound into the
companion. `RecoveryConfig::origin` is an identity label, never dialed: the
companion's binding covers the source, account binding, origin and `SCHEMA`
(`transparent-shard-v11`), so a companion opened under another account, origin
or schema is refused. The re-exported `Table`, `ShardRequest`, `refusal`,
`StaleRevision`, `Overloaded` and `BoxError` are what a transport implementation
needs to map service refusals.

`recover` checks, in order and before any retrieval: the watch set's target is
accepted by the chain view; the watch set and retained scripts are within the
script limit; the filter source does not use the parent-filter experiment,
whose selective child requests leak coarse activity; the shard map is within
the shard limit; and the service's init names `SCHEMA`. A refused check makes
no further requests.

Every pass has script, publication, query, byte and export bounds. Its
`Progress` reports `covered_through` and an `Outcome`:

| `Outcome` | Meaning |
| --- | --- |
| `Complete` | Every watched script is covered through the target. |
| `Behind` | The publication ends below the target. |
| `More` | A query, byte or pending-page budget stopped the pass; the next pass resumes. |
| `Overloaded` | The service refused for capacity throughout its retry budget. |
| `Stalled` | An unknown chain block, unresolved spends or unbounded script discovery. |

The companion store owns reference page continuation and revision-bound caches;
the wallet store owns candidate evidence, qualification and activation.
Replaying a pass after a crash is idempotent. Export intent is persisted before
returning a batch, and revisions a later map no longer names stay recorded in
the companion until trusted reconciliation is acknowledged. Inspect
`batch.retired_revisions()` before applying commits. Resolve any notifications
through independently trusted wallet qualification or rewind controls, apply
returned commits with the existing wallet writer, then call
`acknowledge_reconciled`. Retain failures and leave the batch unacknowledged when
either step fails. For a batch without retirements, apply its commits and call
`acknowledge_applied`; that method refuses any batch with retirements, including
an empty replacement batch. The adapter never qualifies a revision, promotes an
account or authorizes a spend.
A server's revision counter cannot authorize withdrawal. Reader-schema and
publication lineage changes fail closed and require a compatible companion
store.

This is recovery plumbing; sending, Vizor and public transaction-details fetching
are outside its scope. The headless real-source harness and final qualification
are tracked with the activity metadata implementation plan.

`recover-activity` is a bounded headless harness for real HTTP retrieval into
library SQLite candidate evidence, followed by a durable reopen comparison. It
builds the reference HTTP transports itself (development dependency on
`transparent-wallet` with `reqwest`): a 30 s timeout, a 64 MiB response limit,
one request at a time and up to three attempts for transient failures.

```sh
cargo run --locked -p zakura-pir-transparent --features wallet \
  --example recover-activity -- independent-snapshot.json new-evidence-directory \
  http://127.0.0.1:18192
```

The snapshot supplies `birthday`, `through`, up to 64 public locking `scripts`,
and consecutive independently collected RPC `headers` from birthday minus one
through the target. Each header has `height`, display-order `hash`, `time`, and
`previousblockhash`. Keep the raw RPC responses beside this snapshot. Never derive
accepted headers from a publisher manifest. The origin should be a controlled
capture proxy so every HTTP attempt can be checked for public lookup fallback.

This harness inserts public scripts as controlled fixture watches and uses empty
shielded scan fixtures. It proves transparent metadata delivery and SQLite
persistence, not ownership of those public funds or shielded scan correctness.
It requires nonempty metadata recovery and reader version 7, reopens both stores,
and refuses qualification or account activation. Its output directory must be
new; failed runs and partial stores remain available for diagnosis.
