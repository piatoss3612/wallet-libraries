//! Pruning for a note commitment tree that [`update_tree`](super::update_tree) inserts nothing
//! into, such as Ironwood's before its activation.

use incrementalmerkletree::Hashable;
use shardtree::{ShardTree, error::ShardTreeError, store::ShardStore};
use zcash_protocol::consensus::BlockHeight;

/// Removes the oldest checkpoints not retained for anchors until at most `max_checkpoints` of
/// them remain, as `ShardTree` does after an insertion, but only checkpoints of an empty tree
/// state (no position), which mark no leaf and so need no retention flags cleared. It stops at
/// the first checkpoint with a position and leaves that one, and any after it, to `ShardTree`.
pub(super) fn prune_positionless_checkpoints<S, const DEPTH: u8, const SHARD_HEIGHT: u8>(
    tree: &mut ShardTree<S, DEPTH, SHARD_HEIGHT>,
) -> Result<(), ShardTreeError<S::Error>>
where
    S: ShardStore<CheckpointId = BlockHeight>,
    S::H: Clone + PartialEq + Hashable,
{
    let count = tree
        .store()
        .checkpoint_count()
        .map_err(ShardTreeError::Storage)?;
    let retained = tree
        .store()
        .retained_checkpoints()
        .map_err(ShardTreeError::Storage)?;
    let mut unretained = vec![];
    tree.store()
        .for_each_checkpoint(count, |id, checkpoint| {
            if !retained.contains(id) {
                unretained.push((*id, checkpoint.position().is_none()));
            }
            Ok(())
        })
        .map_err(ShardTreeError::Storage)?;
    let excess = unretained.len().saturating_sub(tree.max_checkpoints());
    for (id, _) in unretained
        .into_iter()
        .take(excess)
        .take_while(|(_, positionless)| *positionless)
    {
        tree.store_mut()
            .remove_checkpoint(&id)
            .map_err(ShardTreeError::Storage)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::BlockHeight;

    /// A tree a call inserts nothing into, such as Ironwood's before its activation, still gets a
    /// checkpoint at the frontier and at each height another pool checkpoints. Only `insert_tree`
    /// pruned, so those piled up; they are now kept to `max_checkpoints`.
    #[test]
    fn checkpoints_of_a_tree_without_inserts_stay_bounded() {
        use incrementalmerkletree::frontier::Frontier;
        use shardtree::{
            ShardTree,
            store::{Checkpoint, ShardStore, memory::MemoryShardStore},
        };

        let max_checkpoints = 3;
        let mut tree: ShardTree<MemoryShardStore<sapling::Node, BlockHeight>, 32, 16> =
            ShardTree::new(MemoryShardStore::empty(), max_checkpoints);
        for batch in 0..10u32 {
            let start = 1_000 + 100 * batch;
            let missing =
                (1..=5).map(move |i| (BlockHeight::from(start + i), Checkpoint::tree_empty()));
            super::super::update_tree(
                "test",
                &Frontier::empty(),
                BlockHeight::from(start),
                &mut tree,
                None,
                core::iter::empty(),
                missing,
            )
            .unwrap();
        }
        assert_eq!(tree.store().checkpoint_count().unwrap(), max_checkpoints);
        assert_eq!(
            tree.store().min_checkpoint_id().unwrap(),
            Some(BlockHeight::from(1_903)),
            "the newest checkpoints are the ones kept"
        );
    }
}
