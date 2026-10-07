# Cached witnesses must preserve newer checkpoint roots

When a newer frontier supplies a cap hash above an incompletely stored subtree,
an older-anchor cached witness query must preserve that hash. The witness can be
correct while its cache write removes the only stored hash that makes a newer
checkpoint readable. A successful wallet-tree callback commits that change to
SQLite; reopening the database does not restore the newer root.

This applies to both Ironwood and Orchard. Their SQLite tables are separate,
but both use the same `shardtree` cache implementation. The transaction builder
queries `witness_at_checkpoint_id_caching` for each selected input in both pools.

## SQLite regression fixture

`librustzcash/zcash_client_sqlite/src/wallet/commitment_tree/cached_witness_tests.rs`
uses the production wallet-tree callbacks and the same SDK `update_tree` routine
used by block scanning. It has no dependency on a wallet application.

The fixture uses the production 32-level tree and 16-level shards:

1. A newer scan inserts a frontier containing the combined root of completed
   shards 0 and 1, then inserts one further commitment.
2. An earlier incomplete scan inserts two commitments in shard 1. The full
   shard-1 root and remaining commitments are not independently stored.
3. An older scan backfills complete shard 0, retaining two witness positions
   and its checkpoint. Full unmarked subtrees are represented by coherent
   pruned hashes, so the fixture does not need to append 65,536 leaves.
4. Both the older and newer roots are checked against independently constructed
   expected hashes before querying witnesses.
5. A successful wallet callback queries one or two older-anchor witnesses,
   verifies their roots, and commits. Subsequent root checks open new database
   connections.

For each pool, the suite checks one cached witness, repeated cached witnesses,
read-only witnesses, and cached witnesses with both complete subtree roots
independently available. The latter two are controls.

## Comparing the upstream correction

The upstream cap-preservation correction is
[`zcash/incrementalmerkletree@5fdad27450a1`](https://github.com/zcash/incrementalmerkletree/commit/5fdad27450a1).
It retains a Parent's existing annotation as well as a Leaf's cached hash during
truncated `root_caching` queries. It is included in `shardtree 0.8.0`.

This workspace currently requires `shardtree 0.7`, and resolves `0.7.1`.
The regression-only draft deliberately keeps that dependency and all wallet
behavior unchanged. The root-preservation assertions are expected to fail with
the existing dependency until a compatible correction is adopted.

Run the focused checks with the repository development helper:

```sh
python3 scripts/dev.py test cached_witness_tests --config orchard -p zakura-client-sqlite
python3 scripts/dev.py test cached_witness_tests --config transparent -p zakura-client-sqlite
```

Expected results with `shardtree 0.7.1`: four root-preservation failures and four
passing controls. Applying only the upstream cap-preservation change to a local
copy of `shardtree 0.7.1` should make all eight pass with the same fixture.
Such a local comparison is validation evidence; the temporary dependency
override does not belong in the submitted regression-only change.

A subsequent dependency change can adopt a compatible 0.7.x backport. Moving
to 0.8 also moves the public `incrementalmerkletree` dependency to 0.9 and needs
separate compatibility review. Dependency requirements are maintained in
`manifests/sources.toml`, not the generated root Cargo.toml.

## Evidence boundary

These tests isolate SQLite tree updates, cached witnesses, and persistence.
They do not claim a production lightwalletd incident, full synchronization-loop
execution, note decryption, proving, signing, or broadcasting. Synthetic
commitments and their frontier hashes are mathematically consistent; no scan
queue, cap, or checkpoint is inserted by direct SQL.
