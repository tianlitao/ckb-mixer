//! Nullifier set: a sparse Merkle tree over the 256-bit nullifier hash.
//!
//! The pool cell stores only the 32-byte root. A withdrawal brings the siblings
//! along its nullifier's path. With an empty leaf they must hash to the current
//! root, which shows the nullifier is unspent; with the spent marker in the leaf
//! the same siblings give the new root.
//!
//! The tree is deliberately plain: depth 256, the key's bit `h` picks the side
//! at height `h`, one hash per level, and the only shortcut is that a subtree
//! with nothing in it hashes to zero. Soundness then follows directly from
//! collision resistance: a non-zero node has exactly one pair of children that
//! hashes to it, so siblings that reproduce the root are the real siblings, and
//! the leaf the verifier started from is the real leaf.
//!
//! The `sparse-merkle-tree` crate (0.6.1, the newest release on crates.io) is
//! not used here on purpose. Its compiled proofs can carry a "merged with
//! zeros" sibling whose fields are taken from the proof unchecked, and that
//! lets a prover show a key that is in the tree as absent. For a nullifier set
//! that is a double spend. Upstream's 2026-11-13 fix closed a related gap in
//! the two-side merge opcode, but the folded-node path is still open on master;
//! the forgery is reproduced in `tests/tests/smt_crate_forgery.rs`.
//!
//! Proof encoding: a 32-byte bitmap (bit `h` set means the sibling at height
//! `h` is not empty), followed by those siblings, 32 bytes each, lowest height
//! first.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use blake2b_ref::Blake2bBuilder;

use crate::error::Error;
use crate::field::Bytes32;

const DEPTH: usize = 256;
const ZERO: Bytes32 = [0u8; 32];
const PERSONALIZATION: &[u8; 16] = b"ckb-mixer-nf-smt";

/// Root of the nullifier set before any withdrawal.
pub const EMPTY_ROOT: Bytes32 = ZERO;

/// Leaf of a spent nullifier. An unspent one has the empty (all-zero) leaf.
pub const SPENT: Bytes32 = {
    let mut leaf = [0u8; 32];
    leaf[0] = 1;
    leaf
};

/// Bit `index` of a key or bitmap, least significant bit of byte 0 first.
fn bit(bytes: &Bytes32, index: u8) -> bool {
    bytes[usize::from(index / 8)] >> (index % 8) & 1 == 1
}

/// Hash of the node whose children sit at `height`.
fn node(height: u8, left: &Bytes32, right: &Bytes32) -> Bytes32 {
    if *left == ZERO && *right == ZERO {
        return ZERO;
    }
    let mut hasher = Blake2bBuilder::new(32).personal(PERSONALIZATION).build();
    hasher.update(&[height]);
    hasher.update(left);
    hasher.update(right);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// Check that `nullifier_hash` is unspent under `root` and return the root
/// after marking it spent.
pub fn mark_spent(root: &Bytes32, nullifier_hash: &Bytes32, proof: &[u8]) -> Result<Bytes32, Error> {
    if proof.len() < 32 {
        return Err(Error::NullifierProof);
    }
    let (bitmap, mut siblings) = proof.split_at(32);
    let bitmap: Bytes32 = bitmap.try_into().map_err(|_| Error::NullifierProof)?;

    let mut before = ZERO;
    let mut after = SPENT;
    for height in 0..=u8::MAX {
        let sibling = if bit(&bitmap, height) {
            if siblings.len() < 32 {
                return Err(Error::NullifierProof);
            }
            let (head, tail) = siblings.split_at(32);
            siblings = tail;
            let sibling: Bytes32 = head.try_into().map_err(|_| Error::NullifierProof)?;
            // Empty siblings are implied by the bitmap; one encoding per proof.
            if sibling == ZERO {
                return Err(Error::NullifierProof);
            }
            sibling
        } else {
            ZERO
        };

        if bit(nullifier_hash, height) {
            before = node(height, &sibling, &before);
            after = node(height, &sibling, &after);
        } else {
            before = node(height, &before, &sibling);
            after = node(height, &after, &sibling);
        }
    }
    if !siblings.is_empty() {
        return Err(Error::NullifierProof);
    }
    if before != *root {
        return Err(Error::NullifierSpent);
    }
    Ok(after)
}

/// Off-chain view of the nullifier set, used to build proofs.
///
/// A reference implementation: it keeps only the spent keys and rehashes the
/// tree on every call, which is fine for tests and small sets. An indexer would
/// cache the nodes.
#[derive(Debug, Clone, Default)]
pub struct NullifierSet {
    spent: BTreeSet<Bytes32>,
}

impl NullifierSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.spent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spent.is_empty()
    }

    pub fn contains(&self, nullifier_hash: &Bytes32) -> bool {
        self.spent.contains(nullifier_hash)
    }

    /// Mark a nullifier spent. Returns `false` if it already was.
    pub fn insert(&mut self, nullifier_hash: Bytes32) -> bool {
        self.spent.insert(nullifier_hash)
    }

    pub fn root(&self) -> Bytes32 {
        let keys: Vec<&Bytes32> = self.spent.iter().collect();
        subtree_root(&keys, DEPTH)
    }

    /// Siblings along the path of `nullifier_hash`, in the encoding that
    /// [`mark_spent`] expects.
    pub fn proof(&self, nullifier_hash: &Bytes32) -> Vec<u8> {
        let mut bitmap = ZERO;
        let mut siblings = [ZERO; DEPTH];

        // Walk down from the root. `on_path` holds the keys that share every bit
        // above the current height with `nullifier_hash`.
        let mut on_path: Vec<&Bytes32> = self.spent.iter().collect();
        for height in (0..=u8::MAX).rev() {
            let side = bit(nullifier_hash, height);
            let (same, other): (Vec<&Bytes32>, Vec<&Bytes32>) =
                on_path.into_iter().partition(|key| bit(key, height) == side);
            let sibling = subtree_root(&other, usize::from(height));
            if sibling != ZERO {
                bitmap[usize::from(height / 8)] |= 1 << (height % 8);
                siblings[usize::from(height)] = sibling;
            }
            on_path = same;
        }

        let mut proof = Vec::with_capacity(32 + 32 * 24);
        proof.extend_from_slice(&bitmap);
        for sibling in siblings.iter().filter(|sibling| **sibling != ZERO) {
            proof.extend_from_slice(sibling);
        }
        proof
    }
}

/// Root of a subtree with `height` levels below it. All `keys` lie in that
/// subtree, i.e. they agree on every bit from `height` upwards.
fn subtree_root(keys: &[&Bytes32], height: usize) -> Bytes32 {
    if keys.is_empty() {
        return ZERO;
    }
    if height == 0 {
        return SPENT;
    }
    let level = (height - 1) as u8;
    let (right, left): (Vec<&Bytes32>, Vec<&Bytes32>) = keys.iter().partition(|key| bit(key, level));
    node(level, &subtree_root(&left, height - 1), &subtree_root(&right, height - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pseudo-random keys; nullifier hashes are uniformly distributed too.
    fn key(n: u32) -> Bytes32 {
        let mut hasher = Blake2bBuilder::new(32).personal(b"test-key").build();
        hasher.update(&n.to_le_bytes());
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        out
    }

    #[test]
    fn empty_set() {
        let set = NullifierSet::new();
        assert_eq!(set.root(), EMPTY_ROOT);
        assert_eq!(set.proof(&key(1)), [0u8; 32]);
    }

    #[test]
    fn follows_the_reference_tree() {
        // `mark_spent` walks one path bottom-up; `NullifierSet::root` rebuilds the
        // whole tree top-down. They must agree after every insertion.
        let mut set = NullifierSet::new();
        let mut root = EMPTY_ROOT;
        for n in 0..60 {
            let k = key(n);
            let proof = set.proof(&k);
            root = mark_spent(&root, &k, &proof).unwrap();
            assert!(set.insert(k));
            assert_eq!(root, set.root(), "after {} insertions", n + 1);
        }
        assert_eq!(set.len(), 60);
    }

    #[test]
    fn neighbouring_keys() {
        // Keys that differ only in the lowest bits share almost the whole path.
        let mut set = NullifierSet::new();
        let mut root = EMPTY_ROOT;
        for low in [0u8, 1, 2, 3, 0x80, 0xff] {
            let mut k = key(7);
            k[0] = low;
            root = mark_spent(&root, &k, &set.proof(&k)).unwrap();
            set.insert(k);
            assert_eq!(root, set.root());
        }
        // ... and keys that differ only in the highest bit split at the root.
        let mut k = key(7);
        k[31] ^= 0x80;
        root = mark_spent(&root, &k, &set.proof(&k)).unwrap();
        set.insert(k);
        assert_eq!(root, set.root());
    }

    #[test]
    fn spent_nullifier_cannot_be_spent_again() {
        let mut set = NullifierSet::new();
        for n in 0..8 {
            set.insert(key(n));
        }
        let k = key(100);
        let old_proof = set.proof(&k);
        let root = mark_spent(&set.root(), &k, &old_proof).unwrap();
        set.insert(k);
        assert_eq!(root, set.root());

        // Neither the proof used the first time nor the current siblings can
        // present the key as unspent again.
        assert_eq!(mark_spent(&root, &k, &old_proof), Err(Error::NullifierSpent));
        assert_eq!(mark_spent(&root, &k, &set.proof(&k)), Err(Error::NullifierSpent));
        for n in 0..8 {
            assert_eq!(mark_spent(&root, &key(n), &set.proof(&key(n))), Err(Error::NullifierSpent));
        }
    }

    #[test]
    fn proof_is_bound_to_key_root_and_siblings() {
        let mut set = NullifierSet::new();
        for n in 0..8 {
            set.insert(key(n));
        }
        let root = set.root();
        let k = key(50);
        let proof = set.proof(&k);
        assert!(proof.len() > 32, "a non-empty set gives at least one sibling");
        assert!(mark_spent(&root, &k, &proof).is_ok());

        assert_eq!(mark_spent(&root, &key(51), &proof), Err(Error::NullifierSpent));
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &proof), Err(Error::NullifierSpent));

        // Flip one bit of a sibling.
        let mut tampered = proof.clone();
        tampered[40] ^= 1;
        assert_eq!(mark_spent(&root, &k, &tampered), Err(Error::NullifierSpent));

        // Move a sibling to another height.
        let mut moved = proof.clone();
        let height = (0..=u8::MAX).rev().find(|h| bit(&proof[..32].try_into().unwrap(), *h)).unwrap();
        moved[usize::from(height / 8)] &= !(1 << (height % 8));
        moved[0] |= 1;
        assert_eq!(mark_spent(&root, &k, &moved), Err(Error::NullifierSpent));
    }

    #[test]
    fn stale_proof_is_rejected() {
        let mut set = NullifierSet::new();
        set.insert(key(1));
        let k = key(2);
        let proof = set.proof(&k);
        set.insert(key(3));
        assert_eq!(mark_spent(&set.root(), &k, &proof), Err(Error::NullifierSpent));
        assert!(mark_spent(&set.root(), &k, &set.proof(&k)).is_ok());
    }

    #[test]
    fn malformed_proofs_are_rejected() {
        let k = key(1);
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &[]), Err(Error::NullifierProof));
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &[0u8; 31]), Err(Error::NullifierProof));
        // Trailing bytes after the siblings the bitmap announces.
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &[0u8; 33]), Err(Error::NullifierProof));
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &[0u8; 64]), Err(Error::NullifierProof));

        // Bitmap announces a sibling that is missing, or that is empty.
        let mut bitmap = [0u8; 32];
        bitmap[3] = 0x10;
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &bitmap), Err(Error::NullifierProof));
        let mut with_zero = bitmap.to_vec();
        with_zero.extend_from_slice(&[0u8; 32]);
        assert_eq!(mark_spent(&EMPTY_ROOT, &k, &with_zero), Err(Error::NullifierProof));

        assert!(mark_spent(&EMPTY_ROOT, &k, &[0u8; 32]).is_ok());
    }
}
