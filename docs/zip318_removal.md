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
| `data_api::zip318`, `put_zip318_classification` | Computed and stored a ZIP 318 label for each decrypted transaction during `store_decrypted_tx`. Vizor derives its own history labels and never read it. |
| `propose_transfer`'s crossing attempt | Steered a send of a canonical denomination toward the migration shape: bucketed anchor, single-note funding. Vizor never called `propose_transfer`, so this never ran for it. |
| `WalletRead::pool_migration_params`, `WalletRead::anchor_retention_interval` | Read the grid for that attempt and for the builder. |
| `PreferSingle`, `select_single_spendable_note`, `anchor_computable` | Selection and anchor checks used only by that policy. |

Without that policy, every Ironwood bundle is padded to the default two-action
floor. The fee model still records its dummy-output counts, and
`Step::ironwood_bundle_padding` still reads them, so the computed fee and the
built bundle cannot disagree.

## What stays

### Ordinary sends that already have the crossing shape

A send is a *canonical crossing* when its whole shape matches a migration
transfer: one Orchard input with at most one Orchard change output, no
Ironwood input or change, no other change, one Ironwood payment of a canonical
ZIP 318 denomination, an anchor on the wallet's grid, and the standard fee for
that shape. Such a send is still built as a migration transfer is: a single
unpadded Ironwood action and the ZIP 318 rolling expiry.

This rule runs inside `propose_transaction` and the builder, not in
`propose_transfer`, so it reaches Vizor's ordinary sends. #55 first removed it
too, which made those sends pay one more action and stand out from Vizor's own
migration transfers. It was restored with the following adjustments:

- The fee model reads the grid from `InputSource::anchor_retention_interval`,
  so `propose_transaction` and `propose_shielding` take no ZIP 318 argument.
- The builder no longer re-reads the grid. It applies the rolling expiry
  exactly when the fee model recorded an unpadded Ironwood bundle and the fee is
  canonical, so the padding and the expiry cannot disagree.

Vizor proposes against the ordinary anchor, so its sends hit the grid only when
the anchor happens to fall on a boundary.

### Anchor retention

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
and drops the eight `orchard_ironwood_migration*` tables and their two indexes.
Published rc5/rc7 reference them only through `ON DELETE CASCADE` from
`accounts`, which is inert once they are gone.

`transactions.zip318_kind` and `v_transactions.zip318_kind` stay, as unused
legacy schema. Published zakura-client-sqlite 0.1.0-rc5 and 0.1.0-rc7 write the
column on every transaction store and read the view field, so a wallet this
library upgraded keeps working when a user reinstalls a build that uses them.
This library never reads either; new rows hold the default, `0`.

- The unreleased `drop_zip318_pool_migration` retains the column and view field
  in place. Supported inputs are fresh databases and upgrades from published
  schemas. Databases that applied the earlier development revision which dropped
  the column are outside the supported upgrade path; no repair migration is provided.
- `legacy_writer_marker` (the current leaf) installs `tpir_legacy_writes` and a
  trigger on classification updates after the transparent ledger schema exists.
  Older transaction stores update the classification; UTXO-only writes do not,
  so initialization also validates private projections without a marker. See
  `wallet::init::legacy_writers`. This replaced the explicit
  `prepare_legacy_rollback` handover.
- The column, its view field, the marker table and its trigger go together
  once no supported build writes the column
  ([#85](https://github.com/zakura-core/wallet-libraries/issues/85)).

A fresh wallet and an upgraded wallet end with the same schema, and
`verify_schema` checks that.

## Upstream syncs

These files diverge from upstream now. When an upstream release touches the
removed code, keep the deletion when resolving the merge. If upstream adds a
migration that depends on the dropped tables, it has to be adapted before it
can be registered here.
