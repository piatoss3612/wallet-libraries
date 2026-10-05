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
the shard limit and names Zcash mainnet's network and genesis block; and the
service's init names `SCHEMA`. A refused check makes no further requests. A
watch set with no addresses needs nothing retrieved: once its target is
accepted, the pass makes no request and completes at the target.

`WalletChain` is the chain view for a wallet: it answers from the blocks the
wallet scanned, only through the watch set's target, and never from a
publication. A pass needs it only from the watch set's floor, the lowest
required height (the account birthday). Shards ending below the floor are
neither exported nor cataloged, and the reference client may roll its own
coverage back to just below a shard that starts under the floor without a
wallet hash; that is the only use it makes of blocks below the floor. When the
publication ends below the target, the pass syncs to the map's end if that end
is at or above the floor, the chain view accepts its terminal block, and the
companion holds nothing above it; completing there still reports `Behind`.
Otherwise it passes the target and reports `Behind` at once. Commits keep the
watch set's context either way.

Every pass has script, publication, query, byte and export bounds. Its
`Progress` reports `covered_through` and an `Outcome`:

| `Outcome` | Meaning |
| --- | --- |
| `Complete` | Every watched script is covered through the target. |
| `Behind` | The publication ends below the target; `covered_through` may reach its end. |
| `More` | A query, byte or pending-page budget stopped the pass; the next pass resumes. |
| `Overloaded` | The service refused for capacity throughout its retry budget. |
| `Stalled` | An unknown chain block, unresolved spends or unbounded script discovery. |

The companion store owns reference page continuation and revision-bound caches;
the wallet store owns candidate evidence, qualification and activation. The
adapter never qualifies a revision, promotes an account or authorizes a spend.

Every commit's `RecoveryRevision` is derived from the publication, never
counted, so a recreated companion reproduces the triples the wallet holds:

- `source` hashes the companion binding with the set-identity fields that never
  change while the publication continues (shard schema, network, genesis block,
  profile, envelope version and start height), the shard's geometry, that
  geometry's seal parameters, and the shard id. A set growing into a new
  geometry tier changes no existing source.
- `revision` hashes the shard's manifest digest and whether it is sealed.
- `lineage` is the published revision number plus one.

The companion catalogs each source's published revisions and records which
ones a batch exported. Each pass classifies the map its sync finished with,
without fetching another, and returns a `BatchState`. Commits are returned
only when it is `Ready`:

| `BatchState` | Meaning |
| --- | --- |
| `Ready` | The map agrees with the catalog, and every exported revision is still published or has a successor at a higher lineage in this batch. |
| `Pending` | A lagging replica (an unsealed revision below one already seen, sealed or not), a map missing a shard, stored facts naming a revision the map no longer names, a shard ending on a block the chain view does not hold, or an exported tail whose successor is not retrieved yet. Nothing to apply; a later pass can be ready. |
| `Withdrawn(cause)` | The publication contradicts the catalog. Nothing to apply. Keep the companion and retry later. |

| `WithdrawnCause` | Meaning |
| --- | --- |
| `Regression` | A sealed shard is published below a revision already seen. |
| `Equivocation` | One revision number is published with other content, seal state or endpoint. |
| `ChangedSealed` | An exported sealed revision is no longer published, and the map does not merely name an older unsealed revision of its shard. |
| `Retired` | An exported shard is published under another source, as after a geometry change. |

`Ready` assumes the trusted operation. A successor withdraws its predecessor's
provisional evidence from the wallet only when the wallet qualifies it in the
same transaction (`qualify_and_apply_transparent_ledger_commit`), and
acknowledging the batch makes the companion forget the predecessor. Apply every
commit, then call `acknowledge_applied`; only the latest `Ready` batch can be
acknowledged. Export intent is persisted before a batch is returned, so a crash
before acknowledgment replays the same batch, and a pass that would forget a
possibly applied revision stays `Pending` until its successor is retrieved.

A publication whose set identity no longer continues the companion's (another
profile, start height, envelope or seal for a geometry in use), whether at the
start of a pass or in a map refreshed mid-pass, fails with
`RecoveryError::PublicationChanged`. Recreating the companion is then safe:
the changed fields are part of every affected source, so the new companion's
revisions for those sources cannot collide with the old ones, and every other
source reproduces the triples the wallet holds. Any other divergence the
reference client finds, such as a refreshed map that changes a shard the pass
already read, is a `RecoveryError::Failure`: retry with the same companion,
whose catalog classifies the publication it then finds.

Never recreate a companion because a batch is `Withdrawn`. A publisher that
re-cuts a set without changing its identity restarts revision numbers; the
catalog reports that as `Regression` or `Pending`, but a recreated companion
cannot detect it, and the wallet refuses its colliding revisions as an
integrity failure.

The messages of `RecoveryError::Invalid` and `RecoveryError::Failure` may quote
the companion's transparent history or the caller's transport errors. Log the
variant only.

Companions are format `transparent-reference-companion-v2`. An earlier
companion, whose lineage was a local counter, is refused with
`companion format v1; recreate`. Each pass prunes catalog rows that are neither
published nor exported, keeping each source's newest row; the store's filter
and setup caches down to revisions the map names; and the store's commit log
down to its last entry. Acknowledgment prunes the catalog again.

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
