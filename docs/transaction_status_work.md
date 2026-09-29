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
`MIN(target_height, observed_height)`; other transactions have unknown evidence.
Local creation upserts preserve this provenance even when discovery inserted the
row first. Generic payload ingestion cannot supply a construction target.

A data-only migration lowers the observation-height column (historically
`min_observed_height`, now `observed_height`) to zero for legacy local
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
