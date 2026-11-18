//! The pool cell's data and the two state transitions the pool allows.
//!
//! Layout (1,640 bytes, all integers little-endian, all hashes 32 bytes):
//!
//! | offset | size    | field                                                  |
//! |-------:|--------:|--------------------------------------------------------|
//! |      0 |       4 | `next_index`: number of deposits so far                |
//! |      4 |       4 | `root_pos`: slot of the newest root in `roots`         |
//! |      8 | 30 × 32 | `roots`: ring buffer of recent commitment-tree roots   |
//! |    968 | 20 × 32 | `frontier`: see [`crate::merkle`]                      |
//! |   1608 |      32 | `nullifier_root`: root of the nullifier set            |

use alloc::vec::Vec;

use crate::error::Error;
use crate::field::{fr_from_le_bytes, fr_to_le_bytes, Bytes32};
use crate::merkle::{self, TREE_CAPACITY, TREE_DEPTH};
use crate::smt;

/// Number of recent roots a withdrawal proof may refer to. One root is recorded
/// per deposit transaction, so a proof stays valid for this many of them.
pub const ROOT_HISTORY: usize = 30;

pub const STATE_LEN: usize = 8 + 32 * ROOT_HISTORY + 32 * TREE_DEPTH + 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub next_index: u32,
    pub root_pos: u32,
    pub roots: [Bytes32; ROOT_HISTORY],
    pub frontier: [Bytes32; TREE_DEPTH],
    pub nullifier_root: Bytes32,
}

impl PoolState {
    /// State of a freshly created pool: no deposits, no spent nullifiers.
    pub fn empty() -> Self {
        let mut roots = [[0u8; 32]; ROOT_HISTORY];
        roots[0] = fr_to_le_bytes(&merkle::empty_root());
        Self {
            next_index: 0,
            root_pos: 0,
            roots,
            frontier: [[0u8; 32]; TREE_DEPTH],
            nullifier_root: smt::EMPTY_ROOT,
        }
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        if data.len() != STATE_LEN {
            return Err(Error::StateEncoding);
        }
        let (header, rest) = data.split_at(8);
        let (roots_bytes, rest) = rest.split_at(32 * ROOT_HISTORY);
        let (frontier_bytes, nullifier_root) = rest.split_at(32 * TREE_DEPTH);

        let next_index = u32::from_le_bytes(header[..4].try_into().map_err(|_| Error::StateEncoding)?);
        let root_pos = u32::from_le_bytes(header[4..].try_into().map_err(|_| Error::StateEncoding)?);
        if next_index > TREE_CAPACITY || root_pos as usize >= ROOT_HISTORY {
            return Err(Error::StateEncoding);
        }

        let mut state = Self {
            next_index,
            root_pos,
            roots: [[0u8; 32]; ROOT_HISTORY],
            frontier: [[0u8; 32]; TREE_DEPTH],
            nullifier_root: [0u8; 32],
        };
        for (slot, chunk) in state.roots.iter_mut().zip(roots_bytes.chunks_exact(32)) {
            slot.copy_from_slice(chunk);
        }
        for (slot, chunk) in state.frontier.iter_mut().zip(frontier_bytes.chunks_exact(32)) {
            slot.copy_from_slice(chunk);
        }
        state.nullifier_root.copy_from_slice(nullifier_root);
        Ok(state)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATE_LEN);
        out.extend_from_slice(&self.next_index.to_le_bytes());
        out.extend_from_slice(&self.root_pos.to_le_bytes());
        for root in &self.roots {
            out.extend_from_slice(root);
        }
        for node in &self.frontier {
            out.extend_from_slice(node);
        }
        out.extend_from_slice(&self.nullifier_root);
        out
    }

    /// The newest commitment-tree root.
    pub fn current_root(&self) -> &Bytes32 {
        &self.roots[self.root_pos as usize]
    }

    /// Whether `root` is one of the recent commitment-tree roots.
    ///
    /// The all-zero value marks unused history slots and is never a root.
    pub fn is_known_root(&self, root: &Bytes32) -> bool {
        *root != [0u8; 32] && self.roots.contains(root)
    }

    /// Append the deposits' commitments (`n × 32` bytes) to the commitment tree
    /// and record the new root. Returns `n`.
    pub fn deposit(&mut self, commitments: &[u8]) -> Result<u32, Error> {
        if commitments.is_empty() || commitments.len() % 32 != 0 {
            return Err(Error::ActionEncoding);
        }
        let mut leaves = Vec::with_capacity(commitments.len() / 32);
        for chunk in commitments.chunks_exact(32) {
            let bytes: Bytes32 = chunk.try_into().map_err(|_| Error::ActionEncoding)?;
            leaves.push(fr_from_le_bytes(&bytes).ok_or(Error::NonCanonicalCommitment)?);
        }

        let root = merkle::append(self.next_index, &mut self.frontier, &leaves)?;
        let count = leaves.len() as u32;
        self.next_index += count;
        self.root_pos = (self.root_pos + 1) % ROOT_HISTORY as u32;
        self.roots[self.root_pos as usize] = fr_to_le_bytes(&root);
        Ok(count)
    }

    /// Mark a note as withdrawn.
    ///
    /// Checks everything about a withdrawal that concerns the pool state: the
    /// proof's root is a recent root, the nullifier hash is canonical and it is
    /// not spent yet. Whether the zero-knowledge proof itself is valid, and
    /// where the money goes, is checked by the caller.
    pub fn withdraw(
        &mut self,
        root: &Bytes32,
        nullifier_hash: &Bytes32,
        nullifier_proof: &[u8],
    ) -> Result<(), Error> {
        if fr_from_le_bytes(root).is_none() {
            return Err(Error::NonCanonicalRoot);
        }
        // Without this check a note could be withdrawn once per alias of its
        // nullifier hash, see `crate::field`.
        if fr_from_le_bytes(nullifier_hash).is_none() {
            return Err(Error::NonCanonicalNullifier);
        }
        if !self.is_known_root(root) {
            return Err(Error::UnknownRoot);
        }
        self.nullifier_root = smt::mark_spent(&self.nullifier_root, nullifier_hash, nullifier_proof)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_bn254::Fr;

    fn commitment(n: u64) -> Bytes32 {
        fr_to_le_bytes(&Fr::from(n))
    }

    #[test]
    fn layout() {
        assert_eq!(STATE_LEN, 1640);
        let state = PoolState::empty();
        let bytes = state.to_bytes();
        assert_eq!(bytes.len(), STATE_LEN);
        assert_eq!(PoolState::from_bytes(&bytes), Ok(state.clone()));
        assert_eq!(&bytes[8..40], &fr_to_le_bytes(&merkle::empty_root()));
        assert!(bytes[40..].iter().all(|b| *b == 0));
        assert!(state.is_known_root(&fr_to_le_bytes(&merkle::empty_root())));
        assert!(!state.is_known_root(&[0u8; 32]));
    }

    #[test]
    fn rejects_malformed_state() {
        let good = PoolState::empty().to_bytes();
        assert_eq!(PoolState::from_bytes(&good[..STATE_LEN - 1]), Err(Error::StateEncoding));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(PoolState::from_bytes(&long), Err(Error::StateEncoding));

        let mut bad_pos = good.clone();
        bad_pos[4] = ROOT_HISTORY as u8;
        assert_eq!(PoolState::from_bytes(&bad_pos), Err(Error::StateEncoding));

        let mut bad_index = good;
        bad_index[..4].copy_from_slice(&(TREE_CAPACITY + 1).to_le_bytes());
        assert_eq!(PoolState::from_bytes(&bad_index), Err(Error::StateEncoding));
    }

    #[test]
    fn deposits_roll_the_root_history() {
        let mut state = PoolState::empty();
        let initial_root = *state.current_root();
        let mut seen = alloc::vec![initial_root];

        for n in 0..ROOT_HISTORY as u64 - 1 {
            assert_eq!(state.deposit(&commitment(n + 1)), Ok(1));
            assert!(!seen.contains(state.current_root()));
            seen.push(*state.current_root());
        }
        // 30 roots so far (the empty root plus 29 deposits): all still known.
        assert!(seen.iter().all(|root| state.is_known_root(root)));
        assert_eq!(state.next_index, ROOT_HISTORY as u32 - 1);

        // The next deposit overwrites the oldest slot.
        state.deposit(&commitment(1000)).unwrap();
        assert_eq!(state.root_pos, 0);
        assert!(!state.is_known_root(&initial_root));
        assert!(seen[1..].iter().all(|root| state.is_known_root(root)));

        let bytes = state.to_bytes();
        assert_eq!(PoolState::from_bytes(&bytes), Ok(state));
    }

    #[test]
    fn batch_deposit_records_one_root() {
        let mut batch = PoolState::empty();
        let mut bytes = Vec::new();
        for n in 1..=5u64 {
            bytes.extend_from_slice(&commitment(n));
        }
        assert_eq!(batch.deposit(&bytes), Ok(5));
        assert_eq!(batch.next_index, 5);
        assert_eq!(batch.root_pos, 1);

        let mut single = PoolState::empty();
        for n in 1..=5u64 {
            single.deposit(&commitment(n)).unwrap();
        }
        assert_eq!(batch.current_root(), single.current_root());
    }

    #[test]
    fn deposit_rejects_bad_input() {
        let mut state = PoolState::empty();
        assert_eq!(state.deposit(&[]), Err(Error::ActionEncoding));
        assert_eq!(state.deposit(&[0u8; 31]), Err(Error::ActionEncoding));
        assert_eq!(state.deposit(&[0xff; 32]), Err(Error::NonCanonicalCommitment));
        assert_eq!(state, PoolState::empty());

        state.next_index = TREE_CAPACITY;
        assert_eq!(state.deposit(&commitment(1)), Err(Error::TreeFull));
    }

    #[test]
    fn withdraw_checks_root_and_nullifier() {
        let mut state = PoolState::empty();
        state.deposit(&commitment(1)).unwrap();
        let root = *state.current_root();
        let nullifier = commitment(77);
        let empty_set_proof = [0u8; 32];

        assert_eq!(state.withdraw(&[0xff; 32], &nullifier, &empty_set_proof), Err(Error::NonCanonicalRoot));
        assert_eq!(state.withdraw(&root, &[0xff; 32], &empty_set_proof), Err(Error::NonCanonicalNullifier));
        assert_eq!(state.withdraw(&commitment(5), &nullifier, &empty_set_proof), Err(Error::UnknownRoot));
        assert_eq!(state.withdraw(&[0u8; 32], &nullifier, &empty_set_proof), Err(Error::UnknownRoot));
        assert_eq!(state.nullifier_root, smt::EMPTY_ROOT);

        assert_eq!(state.withdraw(&root, &nullifier, &empty_set_proof), Ok(()));
        assert_ne!(state.nullifier_root, smt::EMPTY_ROOT);
        assert_eq!(state.withdraw(&root, &nullifier, &empty_set_proof), Err(Error::NullifierSpent));
    }
}
