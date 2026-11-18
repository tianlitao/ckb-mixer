//! Append-only Poseidon Merkle tree of deposit commitments (depth 20).
//!
//! The chain never stores the tree. It stores a *frontier*: for every level,
//! the hash of the most recent node that is a left child and still waits for
//! its right sibling. That is enough to append leaves and recompute the root,
//! and it is the same trick as Tornado Cash's `filledSubtrees`.

use alloc::vec::Vec;

use ark_bn254::Fr;
use ark_ff::Zero;

use crate::error::Error;
use crate::field::{fr_from_le_bytes, fr_to_le_bytes, Bytes32};
use crate::poseidon::hash2;
use crate::poseidon_constants::ZEROS;

pub const TREE_DEPTH: usize = 20;
pub const TREE_CAPACITY: u32 = 1 << TREE_DEPTH;

/// Root of the subtree of height `level` that contains only empty leaves.
pub fn zero_hash(level: usize) -> Fr {
    ZEROS[level]
}

/// Root of the tree before any deposit.
pub fn empty_root() -> Fr {
    ZEROS[TREE_DEPTH]
}

/// Append `leaves` at positions `next_index..`, update `frontier` in place and
/// return the new root.
///
/// Only the last leaf of the batch is hashed all the way up. An earlier leaf
/// stops at the first level where its node is a left child: it records that
/// node in the frontier and leaves the levels above to a later leaf of the same
/// batch, which will pass through them anyway. A batch of n leaves therefore
/// costs about n + 20 hashes instead of 20n.
pub fn append(
    next_index: u32,
    frontier: &mut [Bytes32; TREE_DEPTH],
    leaves: &[Fr],
) -> Result<Fr, Error> {
    let count = u32::try_from(leaves.len()).map_err(|_| Error::TreeFull)?;
    if count == 0 {
        return Err(Error::ActionEncoding);
    }
    if count > TREE_CAPACITY - next_index.min(TREE_CAPACITY) {
        return Err(Error::TreeFull);
    }

    let mut root = Fr::zero();
    for (offset, leaf) in leaves.iter().enumerate() {
        let is_last = offset + 1 == leaves.len();
        let mut index = next_index + offset as u32;
        let mut node = *leaf;

        for level in 0..TREE_DEPTH {
            if index & 1 == 0 {
                // Left child: remember it, its right sibling is still empty.
                frontier[level] = fr_to_le_bytes(&node);
                if !is_last {
                    break;
                }
                node = hash2(&node, &ZEROS[level]);
            } else {
                // Right child: its left sibling is a complete subtree in the frontier.
                let left = fr_from_le_bytes(&frontier[level]).ok_or(Error::StateEncoding)?;
                node = hash2(&left, &node);
            }
            index >>= 1;
        }
        root = node;
    }
    Ok(root)
}

/// Host-side helper: rebuild the tree from all of its leaves and return the
/// sibling path of leaf `index` (level 0 first) together with the root.
///
/// This is an independent, straightforward implementation; the tests use it as
/// the reference for [`append`], and a wallet uses it to build withdrawal proofs.
pub fn merkle_path(leaves: &[Fr], index: usize) -> Option<([Fr; TREE_DEPTH], Fr)> {
    if index >= leaves.len() || leaves.len() > TREE_CAPACITY as usize {
        return None;
    }
    let mut nodes: Vec<Fr> = leaves.to_vec();
    let mut position = index;
    let mut path = [Fr::zero(); TREE_DEPTH];

    for (level, sibling) in path.iter_mut().enumerate() {
        *sibling = nodes.get(position ^ 1).copied().unwrap_or(ZEROS[level]);
        nodes = nodes
            .chunks(2)
            .map(|pair| hash2(&pair[0], pair.get(1).unwrap_or(&ZEROS[level])))
            .collect();
        position >>= 1;
    }
    Some((path, nodes[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic generator, enough to vary leaves and batch sizes.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn leaf(&mut self) -> Fr {
            Fr::from(self.next()) * Fr::from(self.next()) + Fr::from(self.next())
        }
    }

    fn reference_root(leaves: &[Fr]) -> Fr {
        if leaves.is_empty() {
            return empty_root();
        }
        merkle_path(leaves, 0).unwrap().1
    }

    /// Recompute the root from a leaf and its sibling path, as the circuit does.
    fn root_from_path(leaf: Fr, index: usize, path: &[Fr; TREE_DEPTH]) -> Fr {
        let mut node = leaf;
        for (level, sibling) in path.iter().enumerate() {
            node = if (index >> level) & 1 == 0 {
                hash2(&node, sibling)
            } else {
                hash2(sibling, &node)
            };
        }
        node
    }

    #[test]
    fn single_appends_match_reference() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut frontier = [[0u8; 32]; TREE_DEPTH];
        let mut leaves = Vec::new();
        for i in 0..70u32 {
            let leaf = rng.leaf();
            leaves.push(leaf);
            let root = append(i, &mut frontier, &[leaf]).unwrap();
            assert_eq!(root, reference_root(&leaves), "after leaf {i}");
        }
    }

    #[test]
    fn batches_match_reference() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for round in 0..40 {
            let mut frontier = [[0u8; 32]; TREE_DEPTH];
            let mut leaves: Vec<Fr> = Vec::new();
            // Several batches of varying size on top of each other.
            for _ in 0..6 {
                let size = 1 + (rng.next() % 13) as usize;
                let batch: Vec<Fr> = (0..size).map(|_| rng.leaf()).collect();
                let root = append(leaves.len() as u32, &mut frontier, &batch).unwrap();
                leaves.extend_from_slice(&batch);
                assert_eq!(root, reference_root(&leaves), "round {round}, {} leaves", leaves.len());
            }
        }
    }

    #[test]
    fn batch_and_single_appends_agree_on_later_roots() {
        // The frontier after a batch differs from the one after single appends in
        // entries that are never read again; every later root must still agree.
        let mut rng = Rng(42);
        let leaves: Vec<Fr> = (0..37).map(|_| rng.leaf()).collect();

        let mut batched = [[0u8; 32]; TREE_DEPTH];
        append(0, &mut batched, &leaves[..23]).unwrap();
        let mut single = [[0u8; 32]; TREE_DEPTH];
        for (i, leaf) in leaves[..23].iter().enumerate() {
            append(i as u32, &mut single, &[*leaf]).unwrap();
        }
        for i in 23..leaves.len() {
            let a = append(i as u32, &mut batched, &[leaves[i]]).unwrap();
            let b = append(i as u32, &mut single, &[leaves[i]]).unwrap();
            assert_eq!(a, b);
            assert_eq!(a, reference_root(&leaves[..=i]));
        }
    }

    #[test]
    fn paths_open_to_the_root() {
        let mut rng = Rng(7);
        let leaves: Vec<Fr> = (0..11).map(|_| rng.leaf()).collect();
        let root = reference_root(&leaves);
        for (index, leaf) in leaves.iter().enumerate() {
            let (path, path_root) = merkle_path(&leaves, index).unwrap();
            assert_eq!(path_root, root);
            assert_eq!(root_from_path(*leaf, index, &path), root);
        }
        assert!(merkle_path(&leaves, leaves.len()).is_none());
    }

    #[test]
    fn empty_tree_root_is_all_zero_leaves() {
        // One real leaf at index 0, then compare against the same tree where the
        // leaf is the empty leaf: that is the empty root.
        let (path, _) = merkle_path(&[Fr::from(5u64)], 0).unwrap();
        assert_eq!(root_from_path(Fr::zero(), 0, &path), empty_root());
    }

    #[test]
    fn capacity_is_enforced() {
        let mut frontier = [[0u8; 32]; TREE_DEPTH];
        let leaf = Fr::from(1u64);
        assert_eq!(append(0, &mut frontier, &[]), Err(Error::ActionEncoding));
        assert_eq!(append(TREE_CAPACITY, &mut frontier, &[leaf]), Err(Error::TreeFull));
        assert_eq!(append(TREE_CAPACITY - 1, &mut frontier, &[leaf, leaf]), Err(Error::TreeFull));
        // The very last slot is still usable (all levels are right children).
        assert!(append(TREE_CAPACITY - 1, &mut frontier, &[leaf]).is_ok());
        assert_eq!(append(u32::MAX, &mut frontier, &[leaf]), Err(Error::TreeFull));
    }
}
