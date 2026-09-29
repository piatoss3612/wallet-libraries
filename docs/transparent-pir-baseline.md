# Transparent PIR ledger baseline

Status: Phase 0 record for the
[preparatory refactor](transparent-pir-preparatory-refactor.md), limited to what
Phase 1 needs. Phase 1 adds the ledger contract, the `tpir_*` schema, and
configured handles on top of this baseline. Later phases extend the inventory
where they change behavior.

## Revisions

| Repository | Revision | Notes |
| --- | --- | --- |
| wallet-libraries | `cf70bf3f5` on `main` | Includes the ZIP 318 drop migration (#55). |
| Vizor | `a3a2683ef` on `roman/ironwood-memo-pir` | Pins wallet-libraries `2e206f7894` and the Common v1.2 crates. |

Vizor enables `orchard`, `transparent-inputs`, `unstable`, and `serde` on
`zakura-client-sqlite`, and `orchard`, `transparent-inputs`, `pczt`, `sync`,
`tor`, and the lightwalletd features on `zakura-client-backend`. A separate
Vizor change moves the pin to the latest wallet-libraries tag, which excludes
the ZIP 318 drop. The Phase 1 handoff starts from that change, so its pin
difference also carries #55. That migration is unrelated to this refactor.

## Library baseline tests

All lanes pass on `cf70bf3f5`, with no pre-existing failures:

| Lane | Result |
| --- | --- |
| `cargo test --workspace --exclude zakura-wallet-lib --exclude zakura-pir-enhance --locked` | pass |
| `cargo test -p zakura-client-backend -p zakura-client-sqlite --features orchard,test-dependencies --locked` | pass; 487 SQLite tests |
| `cargo test -p zakura-client-sqlite --features test-dependencies --locked` | pass; 275 SQLite tests |
| `cargo test -p zakura-client-backend -p zakura-client-sqlite --features orchard,transparent-inputs,test-dependencies,unstable --locked` | pass; 550 SQLite and 121 backend tests |

CI does not run the last lane; Phase 1 adds it. Without `test-dependencies`, the
backend's `cfg(test)` build fails to resolve `rand_08`, and a feature-less
`cargo doc` reports broken intra-doc links in existing modules. Both conditions
predate this work.

## Library call sites

Paths are relative to `librustzcash/`.

| Concern | Location |
| --- | --- |
| Handle state | `WalletDb` in `zcash_client_sqlite/src/lib.rs` already carries `status_mode` and `enhancement_mode` as unpersisted `Option`s that return `*ModeNotConfigured` when absent. Six struct literals construct handles: `for_path`, `from_connection`, `transactionally`, `transactionally_with_extension`, `truncate_to_height_internal` in `wallet.rs`, and `fix_broken_commitment_trees`. |
| Transparent selectors | The `InputSource` impl in `zcash_client_sqlite/src/lib.rs` provides `get_unspent_transparent_output`, `get_spendable_transparent_outputs`, `get_spendable_transparent_outputs_for_addresses`, and `select_spendable_transparent_outputs`. Input selection reaches them only when a spend policy opts into transparent inputs; proposal decoding re-reads the outputs it spends. |
| Proposal consumption | `create_proposed_transactions` and `extract_and_store_transaction_from_pczt` in `zcash_client_backend/src/data_api/wallet.rs` both end in `store_transactions_to_be_sent`. The SQLite body is `store_transaction_to_be_sent` in `wallet.rs`. |
| Transparent writes | `put_transparent_output` in `wallet/transparent.rs` is the single output upsert. Remote callers are `put_received_transparent_utxo` and the `LowLevelWalletWrite` impl used by `store_decrypted_tx`; local callers are the self-send branches of `store_transaction_to_be_sent`. `mark_transparent_utxo_spent` writes both spend links and `transparent_spend_map`. |
| Rewind entry points | `truncate_to_height`, `truncate_to_chain_state`, and `rewind_to_chain_state` in the `WalletWrite` impls; account removal through `delete_account`. |
| Enhancement and status routing | `wallet/enhance_pir.rs` routes `has_transparent` results through `require_lwd`; status routing is in the `TransactionStatusRead` impl. Phase 2 owns both. |
| Migration leaves | `drop_zip318_pool_migration`, `v_tx_outputs_transparent_addresses`, `ivk_item_cache`, `add_transparent_receiver_address_index`, and `add_transparent_value_index`. Only `ufvk_support` and `full_account_ids` can require a seed, and only for legacy account rows. |

A transaction row carries local creation evidence when `transactions.created`
or `transactions.target_height` is set. `store_transaction_to_be_sent` sets
both; `record_transaction_created` sets `target_height`. Remote ingestion leaves
both unset, and `put_tx_data` preserves existing values with `COALESCE`, so a
later observation never erases local evidence. A row's presence or observation
height is not local evidence.

Locks are the `lock_expiry_height` and `lock_owner` columns on each
received-output table. The library has no outbox table; Vizor keeps its outbox
outside the wallet schema and records creation evidence through
`record_transaction_created`.

## Vizor handle inventory

Paths are relative to Vizor's `rust/src/`.

| Handle source | Location | Role |
| --- | --- | --- |
| Base constructors | `wallet/db.rs`: `open_wallet_db_with_timeout`, `open_wallet_db_for_read_with_timeout`, `open_wallet_db_readonly_with_timeout` | Every production handle, including foreground sync, send and PCZT paths, account mutation, balance reads, and native read-only handles. |
| Borrowed transaction | `wallet/sync/migration.rs` in `record_creation_evidence` | Constructs `WalletDb::from_connection` over an open transaction to record creation evidence. |
| Tests and examples | `wallet/addresses/tests.rs` (`old_wallet`), `examples/ledger_zcash_speculos_poc.rs` | Direct construction outside the base constructors. |
| Raw connections | `wallet/db.rs`: `open_wallet_raw_conn_with_timeout` | No `WalletDb`; used for Vizor-owned tables and checkpoints. |

Transparent discovery runs on the sync handle opened once per sync:

- UTXO refresh stores outputs through `store_transparent_outputs` in
  `sync_engine/mod.rs`, inside `transactionally`.
- Ledger discovery (`sync_engine/ledger_discovery.rs`) generates gap addresses,
  fetches address history, and stores full transactions.
- Transparent history enhancement
  (`sync_engine/enhancement/auxiliary/transparent_history.rs`) follows
  `transaction_data_requests`.

Two pre-database previews issue public requests without a wallet database:
`discover_used_software_accounts` and
`preview_software_account_transparent_balance` in `api/wallet.rs`.

Migrations run without a seed through `ensure_db_migrated_once` in
`wallet/keys.rs`, at app startup, and on hardware and observer imports. Only the
creation of a first software account passes a seed. Startup migration runs
before the Enhance/Status policy is applied, so migration must not depend on any
handle mode.

The Vizor tests run on the catch-up commit, and their results will be recorded
here with the Phase 1 handoff. The existing
`scripts/test-db-upgrade-mobile-v0.0.18.sh` probe covers single-derived,
multi-seed, and imported-only databases. Phase 1 adds a hardware-first scenario.

## Fixtures

Phase 1 needs account-shape fixtures (derived, multi-seed, imported-only,
hardware-first) and local-history fixtures: a remote transparent receive, a
local send with transparent change and sent-note details, a locked output, an
unresolved spend-map entry, and a coinbase output. The synthetic receive, spend,
empty-range, and mixed-transaction fixtures for candidate recovery are deferred
to Phase 3, where their expected effects are derived independently of the ledger
implementation.

## Deferred review items

Review of the Phase 1 stack raised points that belong to later phases. They are
tracked here and must be addressed by the phase named.

| Item | Owning phase |
| --- | --- |
| A privileged source-verification operation that qualifies a revision, clears source quarantine, and advances the trust epoch; commits cannot assert trust. | Phase 6 (real-source qualification), with the contract added when source verification is implemented. |
| Qualification enforced for active commits, so an unqualified revision's events are rejected or isolated rather than projected into authoritative state. | Phase 4 (projection and promotion gates). |
| Per-script coverage gaps or a durable next-work cursor in the watched-script snapshot, so bounded recovery resumes uncovered scripts after restart. | Phase 3 (candidate recovery and resumable commits). |
| Withholding or privately routing queued transparent follow-on enhancement work (parent-transaction retrieval) under `PrivateRequired`. | Phase 2 (Enhance/Status routing and dispatch guards). |

