# Routed transaction status work

Status and payload retrieval are independent durable obligations. Status work is
returned only by `TransactionStatusRead::transaction_status_work()`. The batch
contains each actionable txid once, routed to `Public` or `Private` according to
the explicitly configured `TransactionStatusMode`, except that a required-private
transparent ledger policy forces private status work.

Configure every SQLite handle with `set_status_mode` or `with_status_mode` before
obtaining status work, including individual lookups. SQLite also requires
`set_transparent_ledger_mode` or `with_transparent_ledger_mode` on the same handle.
An unconfigured status mode returns `StatusModeNotConfigured`; an unconfigured
ledger mode returns `TransparentLedgerModeNotConfigured`, even for an empty or
shielded-only wallet. Under a durably applied or handle-configured
`PrivateRequired` policy every status obligation is `TransactionStatusWork::Private`.
Transactional handles inherit both settings; reopened handles do not. The
application must authorize its selected disclosure policy. Discard snapshots when
changing that policy.

Payload routing has no effect on the configured `TransactionStatusMode` itself.
The ledger policy can only withhold public status transport (by forcing private
work), not invent a public route.

`transaction_status_work_for(txid)` uses the same evidence and routing rules,
without requiring a queued obligation, enqueueing one, or changing wallet state.
It supports migration recovery and native status observers. An unknown txid can
still be queried privately, but has unknown inclusion evidence.

## Local reconfirmation after a rewind

A rewind queues a durable status obligation for a transaction that compact scanning cannot
rediscover through this wallet's shielded notes or spends. Before un-mining it, SQLite retains
an inclusion receipt when its old height has an available wallet block hash. The receipt stores
the height, hash and optional transaction index independently of the block rows being deleted.
It is historical evidence, not current mining, unspentness, ledger coverage or spending authority.

Automatic batch status work waits while an inclusion receipt awaits a rescan of its height.
Only a successfully accepted `put_blocks` batch can validate it: the same block hash restores
mined metadata locally and settles reconfirmation; a different hash exposes status fallback.
Neither a high scan frontier, a cached block row, nor a rejected batch authorizes restoration.
Explicit `transaction_status_work_for(txid)` remains available while automatic work waits.

Capture, un-mining and queue creation are atomic. Scan acceptance, restoration and receipt
completion are atomic. Receipts survive reopen and repeated rewinds. A completed status
observation or newer authoritative inclusion supersedes the receipt; payload work is independent.
Errors and inconclusive observations never complete it.

Account import retains the existing pruning behavior: it rescans from the birthday but preserves
mined state below the pruning window. Older-writer rewinds and already-stranded databases may
lack receipts. The additive receipt migration starts empty rather than inventing erased hashes;
already-stranded rows use the status fallback with its one-observation expiry exemption.

PR #86 removed the explicit legacy rollback preparation/resume API. The schema retains the
columns used by published older writers, but writes by those writers after this upgrade are
not reconciled or qualified. This change does not recreate that removed handover mechanism,
claim that an older writer captured receipts, or reconstruct missing inclusion evidence.

### Status PIR coverage expectations

| Situation | Expected behavior |
| --- | --- |
| Original block remains accepted, even outside Status PIR retention | Restore from the receipt without an automatic status query |
| Recent block changed and private status has a matching record | Apply the positive status observation |
| No record, with validated coverage of the required interval and a trustworthy local inclusion bound | Apply the supported negative observation |
| Missing record outside coverage, or unknown inclusion bound | Keep recovery unresolved; do not infer failure or absence |
| Recovery is delayed until the relevant history ages out | Completion requires another historical private recovery source and is not guaranteed here |

Recent changed-block recovery is expected to succeed while the relevant Status PIR coverage
remains available, not simply because the transaction was once recent. Imported transactions
can lack the inclusion bound required for a conclusive negative result even within the window.
A receipt does not establish `earliest_possible_inclusion`; it records one observed inclusion,
not when these bytes first became available on any possible branch. Existing creation bounds
continue to be widened on rewind.

`PrivateRequired` still forces private work. `CoverageIncomplete`, stale sessions, transport
errors, cancellation, malformed responses and unsupported capability preserve the obligation,
receipt and reconfirmation flag. Missing private coverage never permits a public fallback.
An empty PIR row is not sufficient evidence of absence without the existing coverage checks.

If a block changed and historical coverage is unavailable, confirmation can remain unresolved
indefinitely; applications must not infer "Send failed" or expiration solely from this uncertainty.
This library change does not add UI handling, historical retention, snapshot authentication or
an inclusion proof. Matching the block hash reuses the original observation under the wallet's
existing chain trust model; it does not independently prove the original transaction membership.

## Expiry dormancy

SQLite omits finite-expiry obligations from batch status work once the contiguous
fully scanned height reaches `expiry_height + PRUNING_DEPTH` (100 blocks of reorg
safety). The queue row remains intact. Rewinding below that boundary makes the
obligation eligible again, subject to the existing mined and conclusive-status
rules. A reported chain tip or a scanned range beyond a gap does not establish
this boundary. With a known chain tip, unknown scan progress does not trigger
expiry dormancy; the existing empty-batch behavior for an unknown tip is unchanged.

Zero expiry means no expiry and never triggers this filter. Unknown expiry also
retains its existing scheduling rules. Dormancy changes neither inclusion evidence
nor transaction status, and does not complete payload retrieval or prove absence
for migration retirement. Explicit `transaction_status_work_for(txid)` lookups
remain available for dormant obligations.

Reconfirmation fallback is exempt from expiry dormancy until one completed status observation.
A matching accepted block settles it locally instead. Errors and incomplete coverage consume
neither the exemption nor the retained receipt; ordinary scheduling resumes after completion.

## Inclusion evidence

A private work item carries `earliest_possible_inclusion: Option<BlockHeight>`.
The bound is inclusive and conservative. `None` means unknown, not genesis,
first observation, or permission to use public transport. A private positive
observation remains useful without a bound. A missing record is inconclusive
unless a validated snapshot covers the entire required inclusion interval.

SQLite records original local creation evidence through sent-transaction storage.
Durable outboxes that hold signed bytes outside the wallet use
`TransactionStatusWrite::record_transaction_created` in the same database
transaction as their outbox entry, before broadcasting. The supplied height must
come from original construction context; later scheduling, retries, received
payloads, and first observation do not establish it. This API is a trusted local
write, not an import-metadata ingestion API.

No new column is needed. `target_height` has an existing contract: it is present
only for transactions created by this wallet. For those transactions the bound is
`MIN(target_height, min_observed_height)`; other transactions have unknown evidence.
Local creation upserts preserve this provenance even when discovery inserted the
row first. Generic payload ingestion cannot supply a construction target.

A data-only migration lowers `min_observed_height` to zero for legacy local
transactions, because historical rewinds did not preserve the necessary bound.
Imported rows remain unchanged. A recent snapshot may therefore be unable to prove
absence for a legacy local transaction. This loss of precision is deliberate.
New local transactions get useful bounds from their original creation context.
Unknown-expiry scheduling uses the original local target when available, so widening
coverage evidence does not prematurely expire these transactions.

The threat assumption is that the local creator knows when these exact bytes
first became available for inclusion; network observations and imported metadata
do not have that authority. A construction target can precede signing, but must
not be an arbitrary future schedule. SQLite widens the supplied bound to at most
the current chain tip, refuses evidence writes and sent-transaction storage without
a known tip, and never raises existing bounds. Rewinding below a stored bound lowers it to the
actual rescan floor atomically with the rewind, even if the retained checkpoint
is higher or no scanned blocks need truncation. The common retained chain prefix
cannot contain a transaction created later; the replacement suffix is included
in future coverage checks. If that premise cannot be established, use unknown
evidence rather than calling the creation writer.

The derived evidence survives ordinary ingestion, queue dormancy, reactivation and reopening.
Evidence writes do not complete enhancement or enqueue status work. Failed outer
transactions roll back both outbox and evidence writes.

## Executing and completing work

Dispatch the work variant to its selected source. A private error never authorizes
a public transaction-ID lookup. The caller supplies `required_through` separately:
it is the chain height against which this particular negative decision will be
made, not transaction provenance. Both bounds remain local to the PIR verifier.

Persist only conclusive observations through `set_transaction_status`. In
particular, do not turn `CoverageIncomplete` into `TxidNotRecognized`, a payload
not-found notification, or successful migration retirement. Keep the work pending
and continue unrelated sync work. Attempt each txid once per checkpoint, including
when rereading the queue; retry inconclusive work on a later checkpoint.

For migration retirement every candidate must be conclusively absent through the
recovery decision height. Incomplete coverage leaves the run unchanged.

## Breaking API migration

Removed: `TransactionDataRequest::GetStatus`, `TransactionStatusRequest`,
`WalletRead::transaction_status_requests`, and `into_status_request`. There is no
compatibility adapter that drops evidence or reintroduces implicit routing.

Implement `TransactionStatusRead` in custom stores and replace status enumeration
with the dedicated work API. `transaction_data_requests()` now contains only
transparent history/spentness work. `transaction_enhancement_work()` remains the
only payload entrypoint. `set_transaction_status()` remains the conclusive status
write API, independent of payload completion.

This change does not alter PIR wire encoding or snapshot authentication.
