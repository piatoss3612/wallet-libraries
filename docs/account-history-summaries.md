# Account-scoped history summaries

`WalletDb::transaction_history_summaries(account)` is the library-owned read for an
activity list. It returns `wallet::history::TransactionSummary` for each transaction
with recorded effects for the account, without loading `transactions.raw`.

The accounting SQL is shared with the canonical `v_transactions` schema definition.
The parameterized query filters received notes, spent notes, and sent-note counts by
account before aggregation. The existing view's columns and behavior are unchanged;
historical migrations remain frozen. The schema regression checks the expanded view
against the persisted result of those migrations.

## Replacing Vizor's copied aggregation

After pinning this library change, Vizor can replace `HISTORY_BASES_CTE` and
`read_history_bases` with a call on its existing configured read handle:

```rust,ignore
let read_tx = conn.unchecked_transaction()?;
let db = wallet_db_on(&read_tx, db_path, network);
let summaries = db.transaction_history_summaries(account)?;
let txids = summaries.iter().map(|s| s.txid).collect::<Vec<_>>();
let details = db.transaction_history_details(account, &txids)?;
// Read the relevant outputs through read_tx, then assemble activity rows.
```

Use the same transaction for summaries, output details, and completeness reads.
A summary call starts its own snapshot if the connection is in autocommit mode;
separate autocommit calls are not one consistent snapshot. The API works on an
existing read-only connection and never initializes the schema or schedules network
work. An unknown account returns `AccountUnknown`; a known account without recorded
transactions returns an empty vector.

The summary carries every fact in Vizor's existing history base: consensus txid,
database-local transaction order, mined height and index, expiry and expiration,
net movement, input and output totals, fee, mined and construction timestamps,
shielding, and whether a version-2 Orchard note is spent. Counts, memo presence,
change, pool-crossing value and trust also preserve the corrected library view's
semantics. Database-local transaction identifiers only support local ordering and
pairing; they are not portable identifiers.

Mapping to Vizor's current `TxBase` retains its presentation defaults: absent block
or construction times become zero and an absent transaction index becomes -1. The
library preserves those unknowns as `Option`. A missing expiry cannot establish
expiration. Expiry uses the library view's private-history fallback; expiration
retains the view's existing stored-transaction expiry predicate. Completeness and
financial authority remain separate reads; a summary does not establish either.

Vizor still owns output attribution for its display, TEX step pairing, self-transfer,
shielding, migration, swap and gift-card labels, sorting, and display limits. Apply
limits after pairing and classification so a TEX funding leg is not lost. Results
have unspecified order and cover all stored transactions with account effects.

This library PR prepares that replacement. It does not modify Vizor or remove its
other output SQL. Consumer integration should pin this exact head (and repin after
merge) and retain Vizor's classification regressions.

## Validation and reproducible timing

The `history_summaries` regressions cover independent expected movements for equal
outputs, multiple inputs and cross-account transfers, full equivalence to the
corrected view, consumed-field equivalence to a frozen Vizor query, unknown metadata,
raw-column access denial, early account filtering in the SQLite query plan, and a
caller snapshot held across a concurrent WAL writer and completeness reads.

The ignored `history_summaries_multi_account_benchmark` uses real migrated schema
and deterministic history: eight accounts with 1,000 transactions each and 32 KiB
raw payloads per transaction. It compares five warm reads of the typed API, the
frozen current Vizor query, and the raw-projected view. It deliberately carries the
old query only as a test oracle, outside production code.

Run the focused regressions through the build-owner wrapper:

```sh
python3 scripts/dev.py test --config transparent -p zakura-client-sqlite history_summaries
```

For the ignored timing experiment, use the same wrapper's built test binary with
`--ignored --nocapture` (see the build-owner record), retaining exclusive build
ownership while compiling. Timings are observations on synthetic data, not a
production latency guarantee. Raw-read denial and query-plan assertions are the
structural performance checks.
