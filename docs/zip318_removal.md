# ZIP 318 pool migration in the fork

Upstream `zcash_client_backend` and `zcash_client_sqlite` carry a ZIP 318
Orchard -> Ironwood pool-migration engine and the wallet-side policy around it.
Vizor does not use any of it. It schedules, signs and broadcasts its own
migration transfers, with its own ZIP 318 constants, and builds them directly
with the transaction builder. This fork therefore removes the upstream engine
and everything that existed only to serve it. The one exception is anchor
retention, which Vizor's migration relies on.

## What was removed

The engine itself (`zcash_pool_migration` and sqlite's `pool_migration`
module) went first, in `7b525e45e`. That change kept the schema. This change
removes the rest:

| Piece | What it did |
| --- | --- |
| `orchard_ironwood_migration*` tables and indexes | Stored the engine's plans. Nothing read or wrote them once the engine was gone. |
| `transactions.zip318_kind`, `v_transactions.zip318_kind` | Labelled each decrypted transaction against ZIP 318. Vizor derives its own history labels and never read the column. |
| `data_api::zip318`, `put_zip318_classification` | Computed and stored that label during `store_decrypted_tx`. |
| Canonical-crossing send policy | Made an ordinary send shaped like a migration transfer indistinguishable from one: bucketed anchor, single-note funding, unpadded Ironwood bundle, rolling expiry. Vizor never called `propose_transfer`, so the policy never ran for it. |
| `PoolMigrationParams`, `pool_migration_params`, `anchor_retention_interval` | Carried the grid into the fee model and input selection for that policy. |
| `PreferSingle`, `select_single_spendable_note`, `anchor_computable` | Selection and anchor checks used only by that policy. |

Without that policy, every Ironwood bundle is padded to the default two-action
floor. The fee model still records its dummy-output counts, and
`Step::ironwood_bundle_padding` still reads them, so the computed fee and the
built bundle cannot disagree.

## What stays

`put_blocks` still retains a note commitment tree checkpoint at every boundary
of the 144-block ZIP 318 grid from NU6.3 onward. It also still creates
checkpoints at boundary blocks that carry no shielded output. Vizor's migration
draws its anchors from these boundaries and treats them as durable
(`is_wallet_durable_anchor` in `vizor-wallet`). Removing retention would let
ordinary checkpoint pruning evict an anchor that a signed migration transfer
still needs. `anchor_retention::AnchorRetention` and `AnchorRetentionInterval`
therefore stay, along with `WalletDb::with_anchor_retention_interval`.

`v_transactions.pool_crossing_value` also stays. It reports value moved between
the wallet's own shielded pools, and it is not specific to ZIP 318.

## Schema

The migrations that created the removed schema are published, and later
migrations depend on them, so they stay registered and unchanged. A new
migration, `drop_zip318_pool_migration`, runs after `status_inclusion_evidence`
and does three things:

- drops the eight `orchard_ironwood_migration*` tables and their two indexes;
- rebuilds `v_transactions` from its stored definition with only the
  `zip318_kind` column removed;
- drops `transactions.zip318_kind`.

A fresh wallet and an upgraded wallet end with the same schema, and
`verify_schema` checks that.

## Upstream syncs

These files diverge from upstream now. When an upstream release touches the
removed code, keep the deletion when resolving the merge. If upstream adds a
migration that depends on the dropped tables or on `zip318_kind`, it has to be
adapted before it can be registered here.
