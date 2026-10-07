//! SQLite regression coverage for cached witnesses that truncate a newer frontier.

use std::{collections::BTreeMap, path::PathBuf};

use incrementalmerkletree::{
    Address, Hashable, Level, Position,
    frontier::{Frontier, NonEmptyFrontier},
};
use orchard::tree::MerkleHashOrchard;
use shardtree::{LocatedPrunableTree, PrunableTree, RetentionFlags, error::ShardTreeError};
use tempfile::TempDir;
use zcash_client_backend::data_api::{
    ORCHARD_SHARD_HEIGHT, WalletCommitmentTrees, chain::CommitmentTreeRoot, testing::TestRng,
};
use zcash_protocol::consensus::{BlockHeight, Network};

use super::Error;
use crate::{
    OrchardCommitmentTree, WalletDb,
    testing::db::{test_clock, test_rng},
    util::testing::FixedClock,
    wallet::init::WalletMigrator,
};

type Db = WalletDb<rusqlite::Connection, Network, FixedClock, TestRng>;
type TreeError = ShardTreeError<Error>;
const SHARD_SIZE: u64 = 1 << ORCHARD_SHARD_HEIGHT;
const OLD_HEIGHT: BlockHeight = BlockHeight::from_u32(2_000_001);
const NEW_HEIGHT: BlockHeight = BlockHeight::from_u32(2_000_101);

#[derive(Clone, Copy)]
enum Pool {
    Orchard,
    Ironwood,
}

impl Pool {
    fn label(self) -> &'static str {
        match self {
            Self::Orchard => "Orchard",
            Self::Ironwood => "Ironwood",
        }
    }
}

// Canonical synthetic commitments: this tests tree storage, not note decryption or proof creation.
fn leaf(value: u8) -> MerkleHashOrchard {
    let mut bytes = [0; 32];
    bytes[0] = value;
    Option::from(MerkleHashOrchard::from_bytes(&bytes)).unwrap()
}

fn uniform_roots(value: MerkleHashOrchard) -> Vec<MerkleHashOrchard> {
    let mut roots = vec![value];
    for level in 0..ORCHARD_SHARD_HEIGHT {
        let child = roots[usize::from(level)];
        roots.push(MerkleHashOrchard::combine(
            Level::from(level),
            &child,
            &child,
        ));
    }
    roots
}

// A complete, hash-consistent first shard, compacted exactly as a prunable tree may be stored.
// Keep the two spend positions and the old checkpoint leaf; collapse all other full subtrees.
fn retained_shard(
    roots: &[MerkleHashOrchard],
    level: u8,
    start: u64,
) -> PrunableTree<MerkleHashOrchard> {
    let end = start + (1u64 << level);
    let contains_retained = [0, 1, SHARD_SIZE - 1]
        .iter()
        .any(|p| start <= *p && *p < end);
    if !contains_retained {
        return PrunableTree::leaf((roots[usize::from(level)], RetentionFlags::EPHEMERAL));
    }
    if level == 0 {
        let flags = if start == SHARD_SIZE - 1 {
            RetentionFlags::CHECKPOINT
        } else {
            RetentionFlags::MARKED
        };
        return PrunableTree::leaf((roots[0], flags));
    }
    let half = 1u64 << (level - 1);
    PrunableTree::parent(
        None,
        retained_shard(roots, level - 1, start),
        retained_shard(roots, level - 1, start + half),
    )
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    pool: Pool,
    old_root: MerkleHashOrchard,
    new_root: MerkleHashOrchard,
    first_shard_root: MerkleHashOrchard,
    second_shard_root: MerkleHashOrchard,
}

impl Fixture {
    fn open(&self) -> Db {
        WalletDb::for_path(&self.path, Network::TestNetwork, test_clock(), test_rng()).unwrap()
    }

    fn with_tree<A>(
        &self,
        callback: impl for<'a> FnMut(
            &'a mut OrchardCommitmentTree<&'a rusqlite::Transaction<'a>>,
        ) -> Result<A, TreeError>,
    ) -> Result<A, TreeError> {
        let mut db = self.open();
        // Both pools have the same tree/hash geometry; the SDK callbacks select
        // the distinct SQLite stores and commit only successful operations.
        match self.pool {
            Pool::Orchard => db.with_orchard_tree_mut(callback),
            Pool::Ironwood => db
                .with_ironwood_tree_mut(callback)
                .map(|value| value.expect("WalletDb tracks an Ironwood tree")),
        }
    }

    fn new(pool: Pool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.sqlite");
        let mut db =
            WalletDb::for_path(&path, Network::TestNetwork, test_clock(), test_rng()).unwrap();
        WalletMigrator::new().init_or_migrate(&mut db).unwrap();
        drop(db);
        let first = uniform_roots(leaf(1));
        let second = uniform_roots(leaf(2));
        let first_shard_root = first[usize::from(ORCHARD_SHARD_HEIGHT)];
        let second_shard_root = second[usize::from(ORCHARD_SHARD_HEIGHT)];
        let ommer = MerkleHashOrchard::combine(
            Level::from(ORCHARD_SHARD_HEIGHT),
            &first_shard_root,
            &second_shard_root,
        );
        let frontier =
            NonEmptyFrontier::from_parts(Position::from(2 * SHARD_SIZE), leaf(3), vec![ommer])
                .unwrap();
        let new_root = frontier.root(Some(Level::from(32)));
        let mut old_root = first_shard_root;
        for level in ORCHARD_SHARD_HEIGHT..32 {
            old_root = MerkleHashOrchard::combine(
                Level::from(level),
                &old_root,
                &MerkleHashOrchard::empty_root(Level::from(level)),
            );
        }

        let fixture = Self {
            _dir: dir,
            path,
            pool,
            old_root,
            new_root,
            first_shard_root,
            second_shard_root,
        };
        fixture
            .with_tree(|tree| {
                use zcash_client_backend::data_api::ll::wallet::update_tree;
                // Same tree-update routine that put_blocks/scan_cached_blocks uses.
                // A newer scan range supplies its preceding frontier and one new commitment.
                let recent_frontier: Frontier<MerkleHashOrchard, 32> =
                    frontier.clone().try_into().unwrap();
                update_tree(
                    pool.label(),
                    &recent_frontier,
                    NEW_HEIGHT,
                    tree,
                    None,
                    std::iter::once((
                        LocatedPrunableTree::from_parts(
                            Address::from_parts(Level::from(0), 2 * SHARD_SIZE + 1),
                            PrunableTree::leaf((leaf(4), RetentionFlags::CHECKPOINT)),
                        )
                        .unwrap(),
                        BTreeMap::from([(NEW_HEIGHT + 1, Position::from(2 * SHARD_SIZE + 1))]),
                    )),
                    std::iter::empty(),
                )?;
                // An earlier, incomplete scan range in shard 1 has a real frontier:
                // its ommer gives the first shard's root but not the complete second shard.
                let middle_frontier = Frontier::<MerkleHashOrchard, 32>::from_parts(
                    Position::from(SHARD_SIZE),
                    leaf(2),
                    vec![first_shard_root],
                )
                .unwrap();
                update_tree(
                    pool.label(),
                    &middle_frontier,
                    OLD_HEIGHT + 1,
                    tree,
                    None,
                    std::iter::once((
                        LocatedPrunableTree::from_parts(
                            Address::from_parts(Level::from(0), SHARD_SIZE + 1),
                            PrunableTree::leaf((leaf(2), RetentionFlags::CHECKPOINT)),
                        )
                        .unwrap(),
                        BTreeMap::from([(OLD_HEIGHT + 2, Position::from(SHARD_SIZE + 1))]),
                    )),
                    std::iter::empty(),
                )?;
                // Backfill the older complete first shard, using the same SDK routine.
                update_tree(
                    pool.label(),
                    &Frontier::<MerkleHashOrchard, 32>::empty(),
                    OLD_HEIGHT - 100,
                    tree,
                    None,
                    std::iter::once((
                        LocatedPrunableTree::from_parts(
                            Address::from_parts(Level::from(ORCHARD_SHARD_HEIGHT), 0),
                            retained_shard(&first, ORCHARD_SHARD_HEIGHT, 0),
                        )
                        .unwrap(),
                        BTreeMap::from([(OLD_HEIGHT, Position::from(SHARD_SIZE - 1))]),
                    )),
                    std::iter::empty(),
                )?;
                assert_eq!(tree.root_at_checkpoint_id(&OLD_HEIGHT)?, Some(old_root));
                assert_eq!(tree.root_at_checkpoint_id(&NEW_HEIGHT)?, Some(new_root));
                Ok(())
            })
            .unwrap();
        fixture
    }

    fn read_root(&self, height: BlockHeight) -> Result<Option<MerkleHashOrchard>, TreeError> {
        self.with_tree(|tree| tree.root_at_checkpoint_id(&height))
    }

    fn witnesses(&self, positions: &[u64], caching: bool) {
        // The transaction builder reads its anchor before querying each input.
        self.with_tree(|tree| {
            assert_eq!(
                tree.root_at_checkpoint_id(&OLD_HEIGHT)?,
                Some(self.old_root)
            );
            for &position in positions {
                let position = Position::from(position);
                let witness = if caching {
                    tree.witness_at_checkpoint_id_caching(position, &OLD_HEIGHT)?
                } else {
                    tree.witness_at_checkpoint_id(position, &OLD_HEIGHT)?
                }
                .expect("marked position has a witness");
                assert_eq!(witness.root(leaf(1)), self.old_root);
            }
            Ok(())
        })
        .unwrap();
        // with_tree commits and drops the connection before subsequent reads.
    }

    fn download_complete_roots(&self) {
        let roots = [
            CommitmentTreeRoot::from_parts(OLD_HEIGHT, self.first_shard_root),
            CommitmentTreeRoot::from_parts(NEW_HEIGHT, self.second_shard_root),
        ];
        let mut db = self.open();
        match self.pool {
            Pool::Orchard => db.put_orchard_subtree_roots(0, &roots),
            Pool::Ironwood => db.put_ironwood_subtree_roots(0, &roots),
        }
        .unwrap();
    }

    fn assert_roots_after_reopen(&self) {
        assert_eq!(self.read_root(OLD_HEIGHT).unwrap(), Some(self.old_root));
        assert_eq!(self.read_root(NEW_HEIGHT).unwrap(), Some(self.new_root));
    }
}

fn cached_witness_preserves_newer_root(pool: Pool, positions: &[u64]) {
    let fixture = Fixture::new(pool);
    fixture.witnesses(positions, true);
    fixture.assert_roots_after_reopen();
}

fn read_only_witnesses_preserve_newer_root(pool: Pool) {
    let fixture = Fixture::new(pool);
    fixture.witnesses(&[0, 1], false);
    fixture.assert_roots_after_reopen();
}

fn complete_roots_preserve_newer_root(pool: Pool) {
    let fixture = Fixture::new(pool);
    fixture.download_complete_roots();
    fixture.witnesses(&[0, 1], true);
    fixture.assert_roots_after_reopen();
}

#[test]
fn ironwood_cached_witness_preserves_newer_root() {
    cached_witness_preserves_newer_root(Pool::Ironwood, &[0]);
}

#[test]
fn ironwood_repeated_cached_witnesses_preserve_newer_root() {
    cached_witness_preserves_newer_root(Pool::Ironwood, &[0, 1]);
}

#[test]
fn ironwood_read_only_witnesses_preserve_newer_root() {
    read_only_witnesses_preserve_newer_root(Pool::Ironwood);
}

#[test]
fn ironwood_complete_roots_preserve_newer_root() {
    complete_roots_preserve_newer_root(Pool::Ironwood);
}

#[test]
fn orchard_cached_witness_preserves_newer_root() {
    cached_witness_preserves_newer_root(Pool::Orchard, &[0]);
}

#[test]
fn orchard_repeated_cached_witnesses_preserve_newer_root() {
    cached_witness_preserves_newer_root(Pool::Orchard, &[0, 1]);
}

#[test]
fn orchard_read_only_witnesses_preserve_newer_root() {
    read_only_witnesses_preserve_newer_root(Pool::Orchard);
}

#[test]
fn orchard_complete_roots_preserve_newer_root() {
    complete_roots_preserve_newer_root(Pool::Orchard);
}
