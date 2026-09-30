# Transparent PIR candidate recovery

The `wallet` feature bridges the reference HTTP client and durable companion
SQLite store into normalized wallet-libraries recovery commits. Applications
supply explicit public/private origins, a stable account binding, a current
watch set and an independently accepted chain view. No address, txid, parent or
outpoint lookup fallback exists.

Every pass has script, publication, response, query, byte and export bounds.
The companion store owns reference page continuation and revision-bound caches;
the wallet store owns candidate evidence, qualification and activation. Replaying
a pass after a crash is idempotent. Export intent is persisted before returning
a batch, so withdrawal remains reconcilable even if the wallet committed and the
process died before acknowledgment. Apply returned commits with the existing
wallet writer and retain its failures; the adapter never qualifies a revision,
promotes an account or authorizes a spend.

A reference reorg or withdrawn revision is reported durably until the application
has reconciled the wallet's evidence through its existing trusted controls. A
server's revision counter cannot authorize withdrawal. The caller acknowledges
that reconciliation only after the wallet transaction commits. Reader-schema and
publication lineage changes fail closed and require a compatible companion store.

This is recovery plumbing; sending, Vizor and public transaction-details fetching
are outside its scope. The headless real-source harness and final qualification
are tracked with the activity metadata implementation plan.

`recover-activity` is a bounded headless harness for real HTTP retrieval into
library SQLite candidate evidence, followed by a durable reopen comparison:

```sh
cargo run --locked -p zakura-pir-transparent --features wallet \
  --example recover-activity -- independent-snapshot.json new-evidence-directory \
  http://127.0.0.1:18192 http://127.0.0.1:18193
```

The snapshot supplies `birthday`, `through`, up to 64 public locking `scripts`,
and consecutive independently collected RPC `headers` from birthday minus one
through the target. Each header has `height`, display-order `hash`, `time`, and
`previousblockhash`. Keep the raw RPC responses beside this snapshot. Never derive
accepted headers from a publisher manifest. The endpoints should be controlled
capture proxies so every HTTP attempt can be checked for public lookup fallback.

This harness inserts public scripts as controlled fixture watches and uses empty
shielded scan fixtures. It proves transparent metadata delivery and SQLite
persistence, not ownership of those public funds or shielded scan correctness.
It requires nonempty metadata recovery and reader version 7, reopens both stores,
and refuses qualification or account activation. Its output directory must be
new; failed runs and partial stores remain available for diagnosis.
