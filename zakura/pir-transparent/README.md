# Transparent PIR candidate recovery

The `wallet` feature bridges the reference HTTP client and durable companion
SQLite store into normalized wallet-libraries recovery commits. Applications
supply explicit public/private origins, a stable account binding, a current
watch set and an independently accepted chain view. No address, txid, parent or
outpoint lookup fallback exists.

Every pass has script, publication, response, query, byte and export bounds.
The companion store owns reference page continuation and revision-bound caches;
the wallet store owns candidate evidence, qualification and activation. Replaying
a pass after a crash is idempotent. Apply returned commits with the existing
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
