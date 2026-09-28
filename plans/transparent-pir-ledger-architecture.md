# Transparent PIR ledger architecture

Status: proposed design; production private authority is not enabled by the
[preparatory refactor](transparent-pir-preparatory-refactor.md).

## Objective and boundaries

Recover transparent receives and spends privately while retaining Vizor's
existing wallet database, transaction construction, shielding, hardware-wallet,
and account lifecycle. Wallet-libraries owns financial correctness; Vizor owns
policy, scheduling, transport, and presentation.

The central invariant is:

> A private transparent balance is authoritative only when validated ledger
> events, continuous per-script coverage, the locally accepted chain, and the
> LRZ projection agree in one durable database state.

Here LRZ denotes the existing `zcash_client_backend` and `zcash_client_sqlite`
APIs, published by this repository as `zakura-client-backend` and
`zakura-client-sqlite`.

```text
                  Vizor private queries setting
                              |
                   immutable operation policy
                              |
        +---------------------+---------------------+
        |                     |                     |
 transparent recovery   payload recovery      status observation
        |
 public filters downloaded whole and matched locally
        |
 private retrieval from matching shards
        |
 normalized events + coverage + source provenance
        |
 wallet-libraries ledger API and SQLite transaction
        |
        +-- candidate ledger: shadow / initial private recovery
        |
        +-- authoritative ledger + LRZ projection: activated account
```

The three recovery lanes share policy resolution and transport facilities but
retain independent queues, completion rules, evidence, and retries. Success in
one lane never completes another or authorizes a different source.

| Owner | Responsibilities |
| --- | --- |
| Protocol client | Filter encoding and matching, manifests, shard layout, PIR requests and response validation, publication revisions, protocol byte/query bounds. |
| Wallet-libraries ledger and SQLite adapter | Watched scripts, recovery bounds, accepted-chain binding, durable events and coverage, provenance, projection, financial eligibility, rewind and account lifecycle. |
| Vizor | Existing private-queries preference, operation lifecycle, endpoints and network route, deadlines and cancellation, bounded scheduling, recovery UI, rollout gates. |

No HTTP, filter layout, shard wire type, or PIR client type appears in the
wallet-facing API. The adapter accepts normalized events and bounded opaque
source/revision identifiers. Applications and protocol clients cannot write LRZ
projection tables directly.

## Trust and privacy model

PIR protects the requested table row; it does not hide all access patterns.
Filters are public data downloaded for every applicable range and matched
locally. Filter requests therefore do not select only matching ranges, but still
reveal the requested recovery range. The shard service can observe approximate
activity ranges, query counts and timing, response sizes, and network origin
unless an anonymity route hides it. Correlation between services can expose
these patterns. In `PrivateRequired`, transparent recovery and its follow-on
retrieval requests must not disclose a wallet address, script, outpoint, or
transaction identifier.

This is a **trusted-indexer profile for accuracy and completeness**. Digests,
manifests, record checks, and accepted-chain anchors detect inconsistent bytes,
mixed revisions, and disagreement with local anchors. They do not prove that
the published events match the chain. A publisher can supply the correct block
hash alongside a self-consistent fabricated or incomplete event set. Trust
includes negative filter results, amounts, spend records, and coinbase
classification.

Production publication requires an independent verifier that reads blocks from
an independently operated node, reconstructs receives and spends, and checks
filters, directory/page contents, counts, and deterministic digests. Verification
must bind to the exact publication digest and block anchors being promoted;
mismatches prevent promotion. This is an operational qualification control,
not a cryptographic inclusion or completeness proof. Designing those proofs is
outside this work.

The privacy boundary includes follow-on requests. A ledger transaction stub,
unknown fee, unresolved spend, or missing raw transaction does not authorize a
public txid or parent-output lookup. Locally constructed transactions, already
authorized payloads, and identifier-independent chain/mempool observations retain
their own provenance; they never establish ledger coverage. Broadcasting a
user-authorized transaction remains a separate operation.

## Authority and the shared Vizor setting

The library makes source authorization explicit:

```rust
pub enum TransparentLedgerMode {
    Public,
    PrivateShadow,
    PrivateRequired,
}
```

`Public` retains current public discovery. `PrivateShadow` also retains public
financial authority and is a qualification mode, not a privacy claim.
`PrivateRequired` forbids public transparent discovery even while private
recovery is unavailable or incomplete.

Vizor derives this authorization from the same install-scoped **private queries**
preference used by Enhance and Status; there is no transparent-specific user
toggle. The policy applies to the whole wallet database on the selected network,
while each account has its own recovery state. Shadow selection is an internal
qualification control.

Preparation preserves existing production behavior and makes no new
transparent-privacy claim. In the first release that enables private transparent
authority, the saved private-queries preference is applied before discovery
starts. That release needs working private integration and the qualification
gates below; the presence of the new schema is insufficient. Once private
authority is selected, a disabled service, missing configuration, or release
kill switch pauses transparent operations rather than selecting public mode.

Every handle performing transparent discovery or financial authorization must
be explicitly configured. An unconfigured handle returns `ModeNotConfigured`,
including for an empty wallet. Transactional handles inherit configuration;
reopened foreground, read-only, and native/background handles resolve it again.

The database durably records applied policy, a policy generation, and the
reader compatibility requirement. The install preference remains the
user-facing choice; the database record prevents a missing startup value or stale
handle from silently weakening it. Operation policy is immutable, but a policy
transition revokes older operations. Dispatch and commit check the generation.

Setting changes extend Vizor's existing pause-and-resume flow:

1. Block new discovery and drain or cancel accepted public work before reporting
   private mode enabled.
2. Persist the shared preference and database policy transition, with native and
   background work no less restrictive during the transition.
3. Invalidate old work and cached financial summaries, then resume under a new
   operation policy.

Preference and database writes are not one transaction. Interrupted transitions
resume conservatively under the stricter state; startup must reconcile them
before dispatch. Public behavior can resume only after an explicit setting
change is durably reconciled. A read-time default is not that authorization.
An import or preview without a wallet database must resolve the same shared
policy before its first network request.

## Migration and activation

### Additive migration and legacy evidence

Schema migration requires no seed and does not rewrite public balances. It must
support long-lived wallets, multiple seeds/accounts, imported-only databases,
and hardware-first wallets.

Existing remotely observed rows receive legacy provenance, not private
coverage. Preserve independently established local-send evidence, pending
transactions, outboxes, proposal locks, address reservations, and payload
provenance. Do not infer local creation from a row's presence or a later
observation height. One projected row can have several origins.

The existing transparent receive sidecar remains a public-path cache during
preparation. Its checkpoints, legacy address checks, and legacy balances cannot
certify private recovery.

### Isolated candidate recovery

Shadow commits update candidate events, coverage, pending work, and candidate
address-window progress only. They cannot change production LRZ balances,
spend links, locks, address-use flags, or receive-address selection. Derivation
logic can be shared, but shadow-only discoveries stay in the candidate watch
set until activation.

The candidate ledger never ingests the LRZ projection as discovery evidence.
Compare exact receive/spend and UTXO sets at a common accepted chain point,
not balances alone. Legacy parity is a useful discrepancy check, not an
independent correctness oracle or proof of complete history.

### Private activation and account promotion

Enabling private mode immediately stops public transparent discovery. Accounts
without qualified coverage enter initial private recovery: transparent input
selection and shielding are unavailable; independently eligible shielded-only
operations continue. Keep the previous amount explicitly marked last-known,
never current or spendable.

Existing shadow state can be reused only after revalidating its production
source, chain anchors, publication lineage, script bounds, and current watch
set. Test fixtures cannot qualify a production account.

An account is promotable when the complete current watch set has continuous
coverage through the accepted decision point, address-window expansion is
stable, no relevant pages or unresolved spends remain, supported history is
available, and legacy discrepancies have been explained against accepted-chain
evidence. Neither agreement with an incomplete legacy snapshot nor deleting
mismatching rows resolves a discrepancy.

A wallet-libraries transaction rechecks these conditions, materializes the
candidate projection, merges independent local evidence, records active
authority, and invalidates the candidate work generation. Preserve shared
transaction rows, pending-spend overlays, and locks. Legacy-only observations
must not contribute to the new authoritative UTXO set. Failure rolls back the
entire promotion; restart sees either the prior state or the activated account.

After promotion, ledger commits update the authoritative projection atomically.
If the chain advances or recovery becomes incomplete, transparent authorization
pauses until coverage catches up. The prior covered balance can remain visible
with its chain point. One account's recovery failure does not block an otherwise
ready account or unrelated shielded operations.

### Rollback and repair

Supported rollback uses a privacy-aware release or forward repair. Retain
durable evidence for rebuilding the projection; do not require a user to discard
the wallet database or pending local transactions. A restored database must
reconcile the install preference before networking and revalidate coverage.
An outage or disabled rollout keeps private policy and pauses transparent
operations. Returning to public discovery requires the existing setting change.

Test the designated supported rollback release against the upgraded database.
Do not claim that arbitrary older binaries will honor new privacy metadata;
a new marker cannot retrofit enforcement into an old application.

## Wallet-facing contract

Use `zcash_client_backend::data_api::transparent_ledger` for the product-neutral
contract. `TransparentLedgerRead` extends `WalletRead`;
`TransparentLedgerWrite` extends the read trait and `WalletWrite`. Reuse the
existing account and error types.

The API provides:

- a watched-script snapshot containing ownership, recovery bounds, and watch-set
  generation;
- one atomic `TransparentLedgerSnapshot<AccountId>`;
- one `apply_transparent_ledger_commit` operation for normalized events,
  coverage, resumable progress, and expected operation context; and
- a guarded account-promotion operation that performs the activation transaction
  described above.

Configuration and policy transitions are explicit. A recovery source cannot
choose its own destination or bypass promotion by marking a commit authoritative;
storage checks the account lifecycle and current policy.

```rust
pub struct ChainPoint {
    pub height: BlockHeight,
    pub hash: BlockHash,
}
```

`TransparentLedgerSnapshot` is the single balance-and-recovery result. Its
canonical fields and meanings are:

| Field | Meaning |
| --- | --- |
| Account, mode, authority | Account identity, configured mode, and whether current financial authority is public, private, or unavailable during recovery. |
| Target | Optional locally accepted `ChainPoint` captured for recovery; absent when the chain is unknown. |
| Covered / settled through | Optional accepted chain points for continuous coverage of the current account watch set; settled includes sealed ranges only. |
| Authorized balance | Optional transparent balance split into regular and coinbase `Balance` values, with existing confirmation and lock categories; absent when current financial authority cannot be established. |
| Last-known balance | Optional prior balance with its source and available chain point; legacy observations without a verified anchor retain that uncertainty. |
| Recovered net | Optional candidate amount from currently recovered events, explicitly unverified until coverage is complete. |
| Completion and blockers | Recovery state plus reasons such as publication lag, pending pages, unsupported history, unresolved spends, or integrity failure. |
| Diagnostic counts | Remaining work, unresolved spends, and unsupported scripts relevant to this account. |

A partially recovered net amount can overstate or understate the true balance.
It is neither an authoritative balance nor a reliable lower bound. Unavailable
is not zero. All fields come from one database read snapshot. Do not embed the
whole `AccountBalance`, which would conflate this result with shielded pools.

Existing `InputSource` methods remain the input-selection interface, with
ledger eligibility enforced internally. There is no separate selector callers
can bypass, and no standalone public ledger-rewind operation.

## Scripts and event evidence

Each watched script carries its account/scope and `required_from` bound.
Derivation today does not establish that the script had no earlier history.
A shielded account birthday is usable only when it is also a justified
transparent-history lower bound. Trusted supplied recovery information and
earlier known activity can move the bound earlier; it never moves later.

Unknown starts remain explicit and require recovery from genesis. Starting at
the earliest publication can make partial progress but cannot close an older
gap. Missing locally accepted historical anchors also remain incomplete; the
publisher cannot fill them with its own asserted hashes.

Receive content contains txid, output index, exact script, value, and explicit
coinbase classification. Spend content contains spending txid, input index,
and spent outpoint. Stable identities are:

```text
receive = transaction identifier + output index
spend   = spending transaction identifier + input index
```

The spent outpoint is checked content, not an extra identity component that
could hide contradictory spends for the same input. Retain unresolved spends
until their receives arrive; they block completeness for the affected account.
Reject incompatible canonical spends of the same outpoint; pending local
conflicts follow the existing wallet transaction rules.

Separate immutable content from mining placement and source observations.
Canonical placement records the mined height and locally accepted block hash.
Observations retain publication/revision identity, accepted-chain anchor, and
record bytes or digest. Identical content observed in another publication is
not a contradiction. Re-mining after an accepted rewind updates canonical
placement without changing identity. Conflicting immutable content, or
incompatible placements claimed on the same accepted chain, is an integrity
failure.

Local construction, reservations, and unmined/mempool observations form a
separate overlay. They can prevent input reuse or enrich history; they do not
advance coverage or become proof of absence.

## Durable state and atomic projection

Ledger state lives under the `tpir_*` namespace in the existing physical SQLite
wallet database. It stores policy/lifecycle metadata, scripts and generations,
events and source observations, coverage, resumable pending work, projection
origins, and local shadow-comparison summaries.

Candidate and authoritative writes use the same validation rules but different
projection behavior. Candidate writes stop at isolated ledger state.
Authoritative writes atomically persist:

- validated events, observations, and unresolved-spend resolution;
- complete coverage intervals and pending-page progress;
- publication anchors and expected operation generations;
- address-use/window updates and any newly required recovery; and
- the corresponding LRZ output, spend, and transaction projection.

Partial pages may persist resumable progress but cannot advance coverage past
unfinished work. A validated negative filter result needs its own durable
coverage commit even when there are no events. Coverage cannot span missing
pages, unsupported scripts, or unvalidated ranges.

Financial correctness must not depend on cross-database crash atomicity.
Filters, manifests, and setup material may use a disposable cache; losing it
costs bandwidth, not evidence. Projection rollback removes only the invalidated
source's contribution, preserving independent local origins. The ledger does
not read back its own projection as evidence.

Projection supports history without raw transaction bytes. Preserve explicit
coinbase classification in the existing balance and input-selection queries:
a missing transaction index must not make a PIR-created output non-coinbase.
Unknown fee, time, or other unavailable metadata stays unknown rather than
becoming a fabricated zero or triggering public enrichment.

## Coverage, synchronization, and financial authorization

Coverage binds a script and inclusive height interval to its source revision,
accepted terminal hash, sealed/provisional status, and original publication
anchor. Sealed publication is not chain finality: a reorg invalidates affected
sealed coverage too. A provisional revision replacement requires revalidation,
not extending the old revision's coverage by assumption.

A recovery run fixes the highest contiguous locally scanned `ChainPoint`.
This is distinct from LRZ's `TargetHeight`: a proposal intended for block
`T` needs coverage through accepted block `T - 1`. Live authorization also
requires local contiguous scanning through that decision point and no known
newer tip left uncovered. The future block `T` has no accepted hash to check.
Historical snapshots are informational and cannot authorize a current spend.

The bounded synchronization loop is:

1. Capture operation policy, accepted target, and chain/watch-set generations.
2. Validate configuration before dispatch, then publication network/genesis,
   schema, layout, and lineage.
3. Enumerate scripts and validate stored coverage against the accepted chain.
4. Download every applicable public filter and match scripts locally.
5. Retrieve matched shards privately, with query/byte/page budgets.
6. Commit validated progress after rechecking operation context in the database.
7. Expand the appropriate candidate or active address window and repeat at the
   same target until stable or a bound is reached.
8. Publish completion from durable state; recheck eligibility before promotion.

A shard extending beyond the fixed target may be retrieved and validated whole,
but only events through the accepted target enter canonical state or the
projection. Coverage retains both its accepted endpoint and original publication
anchor; publication refresh does not move the run's target.

No write lock is held across network I/O. Under the commit transaction,
revalidate policy generation, account existence, watch-set generation, and chain
anchors. Rewinds, deletion, imports, or bound changes invalidate affected work.
Expected window growth obtains a fresh watch snapshot for the next pass.
Stale work is retried from new context, not committed under obsolete authority.
Ordinary chain advance can leave an earlier valid commit useful but cannot make
it sufficient for a newer financial decision.

For private spending or shielding, the affected account must have complete
coverage for its current watch set through the decision point, no relevant
pending pages or unresolved spends, no unsupported history/scripts, and a valid
accepted receive. Apply existing confirmations, coinbase maturity, spend,
reservation, and lock rules as well. There is no freshness tolerance.

Enforce this in individual-outpoint, address, batched, and value-bounded input
queries, plus proposal consumption and hardware finalization. Revalidate and
reserve inputs in the wallet transaction; a prior UI eligibility check is not
authorization. Existing legitimate same-proposal chained outputs use their
local construction evidence and reservations, not fictitious mined coverage.
A selector may continue with eligible shielded pools; a transparent-only
operation reports recovery unavailable rather than misleading insufficient
funds.

## Rewinds and account lifecycle

Integrate ledger rollback into LRZ's height truncation, chain-state truncation,
and rescan/rewind paths. Use the actual retained height and rescan floor
selected by each operation. Remove or invalidate unsupported canonical
placements, coverage, provisional revisions, pending work, and projection
contributions in the same database transaction as the chain change.

Never leave active evidence anchored above the retained height. If an ancestor
hash cannot be established, drop unverifiable coverage rather than invent an
anchor. Preserve independently valid local transactions, reservations, and
issued-address history; rewind must not make an exposed address unused again.
Recompute chain-derived address-use state separately.

Account import, deletion, and birthday lowering update script bounds, invalidate
affected generations, and integrate with Vizor's existing operation drain.
Delete account-owned ledger state and coverage atomically without deleting shared
transactions or another account's evidence. Adding earlier history cannot leave
a previously complete account marked current. Summary caches must invalidate
on these changes and on policy, promotion, and ledger writes.

## Failure semantics and qualification

Timeout, overload, publication lag, cancellation, and budget exhaustion preserve
committed progress and leave recovery incomplete. Stale operation context
requires a new snapshot. Neither condition authorizes a source change.

Wrong network/schema, inconsistent digests, mixed revisions, or contradictory
content rejects the affected commit and ends trust in that session. Financial
eligibility remains blocked for affected state until revalidated. Database,
migration, or projection failure aborts the wallet operation; it cannot be
reported as successful synchronization.

Qualification requires an independent block-derived oracle for exact receive,
spend, UTXO, balance, and coverage results. Include:

- seedless upgrades of fresh, long-lived, multi-seed, imported-only, and
  hardware-first databases, with pending sends and prior rewinds;
- commit, promotion, and rewind failpoints; disk-full failures; process
  termination and WAL restart;
- idempotency, contradictions, spend-before-receive, negative filters, partial
  pages, unknown bounds/anchors, and address-window growth;
- coinbase maturity, all input selectors, software shielding, Ledger rounds,
  Keystone PCZTs, stale proposals, and local chained outputs;
- same-height and sealed/provisional reorgs, re-mining, actual rewind heights,
  and concurrent import/deletion/policy changes;
- proof that shadow cannot alter public balances, selection, locks, address-use,
  or receive-address choice;
- request capture across sync, import, preview, fee/payload recovery, startup,
  setting transitions, cancellation, and native/background entry points; and
- outage/lag exercises, supported rollback, and mobile resource and network-route
  measurements.

Production gates are fixture-backed wallet integration, real protocol validation
(including known-answer and malformed records), real-source shadow qualification,
independent publication verification, a controlled private cohort, then general
availability. Keep detailed comparisons local and export only redacted aggregate
diagnostics. Record the exact library pin, protocol/publication version, and
tested application release for each gate. Preparation alone qualifies none of
the remote protocol, service, or production privacy claims.
