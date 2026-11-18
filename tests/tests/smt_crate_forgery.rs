//! Why the nullifier set does not use the `sparse-merkle-tree` crate.
//!
//! With version 0.6.1 of that crate, a compiled proof can show a key that IS
//! in the tree as absent. A pool that kept its nullifiers in such a tree and
//! checked withdrawals with `CompiledMerkleProof` would accept the same note
//! again and again.
//!
//! How it works: the crate does not hash a node that has an empty sibling. It
//! folds the empty sibling into the node as `MergeWithZero { base_node,
//! zero_bits, zero_count }`, where `zero_bits` records on which side the empty
//! sibling was. Proof opcode `0x51` lets the proof supply such a node as a
//! sibling, and its three fields are taken as they come.
//!
//! Take a spent key whose lowest set bit is at height `h`. In the real tree its
//! leaf was folded `h + 1` times, so the node above height `h` is
//! `MergeWithZero { base_node: leaf, zero_bits: {h}, zero_count: h + 1 }`.
//! The forged proof claims the key's leaf is empty and offers, as the sibling
//! at height `h`, `MergeWithZero { base_node: leaf, zero_bits: {h},
//! zero_count: h }`. Merging an empty node with that sibling only increments
//! `zero_count`, which gives exactly the real node. From there the honest
//! proof's remaining siblings lead to the real root.
//!
//! `mixer_core::smt` has no folded nodes and no node kinds to confuse: every
//! level is one hash of (height, left, right).

use mixer_core::smt::{self, NullifierSet};
use mixer_core::Error;
use sparse_merkle_tree::{
    blake2b::Blake2bHasher, default_store::DefaultStore, traits::Hasher, CompiledMerkleProof,
    SparseMerkleTree, H256,
};

type Tree = SparseMerkleTree<Blake2bHasher, H256, DefaultStore<H256>>;

const KEYS: u8 = 20;

fn key(n: u8) -> H256 {
    let mut hasher = Blake2bHasher::default();
    hasher.write_byte(n);
    hasher.finish()
}

fn spent() -> H256 {
    H256::from(smt::SPENT)
}

/// A compiled proof that `key`, which is in `tree`, is absent from it.
fn forge_absence(tree: &Tree, key: H256) -> Vec<u8> {
    let height = (0..=u8::MAX).find(|bit| key.get_bit(*bit)).expect("key is not zero");
    let honest: Vec<u8> = tree
        .merkle_proof(vec![key])
        .expect("proof")
        .compile(vec![key])
        .expect("compiled proof")
        .into();

    // The base node of the real leaf.
    let mut hasher = Blake2bHasher::default();
    hasher.write_byte(0);
    hasher.write_h256(&key.parent_path(0));
    hasher.write_h256(&spent());
    let base_node = hasher.finish();
    let mut zero_bits = H256::zero();
    zero_bits.set_bit(height);

    // L: push the leaf. O n: merge with n empty siblings. Q: merge with a
    // MergeWithZero sibling read from the proof.
    let mut proof = vec![0x4c];
    if height > 0 {
        proof.extend_from_slice(&[0x4f, height]);
    }
    proof.push(0x51);
    proof.push(height);
    proof.extend_from_slice(base_node.as_slice());
    proof.extend_from_slice(zero_bits.as_slice());

    // The honest proof starts with "L, O n": the key's n lowest siblings are
    // empty. The forged part covered `height + 1` of them; keep the rest.
    assert_eq!(honest[..2], [0x4c, 0x4f], "the key's lowest siblings are empty");
    let empty_levels = if honest[2] == 0 { 256 } else { u16::from(honest[2]) };
    let covered = u16::from(height) + 1;
    assert!(empty_levels >= covered, "no real sibling below the forged one");
    if empty_levels > covered {
        proof.extend_from_slice(&[0x4f, (empty_levels - covered) as u8]);
    }
    proof.extend_from_slice(&honest[3..]);
    proof
}

#[test]
fn sparse_merkle_tree_crate_accepts_forged_absence_proofs() {
    let mut tree = Tree::default();
    let keys: Vec<H256> = (0..KEYS).map(key).collect();
    for key in &keys {
        tree.update(*key, spent()).expect("update");
    }
    let root = *tree.root();

    for key in &keys {
        assert_eq!(tree.get(key).expect("get"), spent());

        // The honest proof behaves: the key is there, and it is not absent.
        let honest = tree.merkle_proof(vec![*key]).expect("proof").compile(vec![*key]).expect("compiled");
        assert!(honest.verify::<Blake2bHasher>(&root, vec![(*key, spent())]).expect("verify"));
        assert!(!honest.verify::<Blake2bHasher>(&root, vec![(*key, H256::zero())]).expect("verify"));

        // The forged proof says the same key is absent, and verifies.
        let forged = CompiledMerkleProof(forge_absence(&tree, *key));
        assert_eq!(
            forged.verify::<Blake2bHasher>(&root, vec![(*key, H256::zero())]),
            Ok(true),
            "the forged absence proof was expected to verify"
        );
    }
}

#[test]
fn mixer_nullifier_set_has_no_absence_proof_for_a_spent_key() {
    let mut set = NullifierSet::new();
    let keys: Vec<[u8; 32]> = (0..KEYS).map(|n| key(n).into()).collect();
    for key in &keys {
        set.insert(*key);
    }
    let root = set.root();

    for key in &keys {
        // The only siblings that lead to the root are the real ones, and with
        // them the leaf is the spent marker, not empty.
        assert_eq!(smt::mark_spent(&root, key, &set.proof(key)), Err(Error::NullifierSpent));
    }
    // A key that is not in the set is absent, and can be marked.
    let fresh: [u8; 32] = key(KEYS).into();
    assert!(smt::mark_spent(&root, &fresh, &set.proof(&fresh)).is_ok());
}
