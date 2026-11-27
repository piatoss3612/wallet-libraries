#![cfg(feature = "orchard")]

use incrementalmerkletree::{Address, Hashable, Level, Position};
use orchard::tree::MerkleHashOrchard;
use shardtree::{PrunableTree, RetentionFlags};

#[test]
fn prune_preserves_orchard_root() {
    let leaves = [1, 2, 3, 4].map(|n| {
        let mut bytes = [0; 32];
        bytes[0] = n;
        MerkleHashOrchard::from_bytes(&bytes).unwrap()
    });
    let leaf_level = Level::from(0);
    let expected = MerkleHashOrchard::combine(
        Level::from(1),
        &MerkleHashOrchard::combine(leaf_level, &leaves[0], &leaves[1]),
        &MerkleHashOrchard::combine(leaf_level, &leaves[2], &leaves[3]),
    );
    let root_addr = Address::from_parts(Level::from(2), 0);
    let end = Position::from(4);

    for retention in [
        RetentionFlags::MARKED,
        RetentionFlags::CHECKPOINT,
        RetentionFlags::REFERENCE,
        RetentionFlags::EPHEMERAL,
    ] {
        let pair = |left, right, flags| {
            PrunableTree::parent(
                None,
                PrunableTree::leaf((left, flags)),
                PrunableTree::leaf((right, RetentionFlags::EPHEMERAL)),
            )
        };
        let tree = PrunableTree::parent(
            None,
            pair(leaves[0], leaves[1], retention),
            pair(leaves[2], leaves[3], RetentionFlags::EPHEMERAL),
        );
        assert_eq!(tree.root_hash(root_addr, end).unwrap(), expected);
        let pruned = tree.prune(root_addr.level());
        assert_eq!(
            pruned.root_hash(root_addr, end).unwrap(),
            expected,
            "{retention:?}"
        );
    }
}
