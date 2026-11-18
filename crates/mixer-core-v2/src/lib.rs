//! Pool v2: deposits arrive as intent cells and one transaction settles a
//! batch of deposits and withdrawals. See `docs/intent-aggregation.md`.
//!
//! `mixer-core` is left as deployed with v1; this crate reuses its tree,
//! nullifier set and proof encodings.
//!
//! Pool type script args (140 bytes):
//!
//! | offset | size | field                                                     |
//! |-------:|-----:|-----------------------------------------------------------|
//! |      0 |   32 | `type_id`, see [`type_id`]                                |
//! |     32 |    8 | `denomination`, shannons, little-endian                   |
//! |     40 |   32 | `vk_hash`: data hash of the verifying key cell            |
//! |     72 |   33 | pool lock: code hash, hash type                           |
//! |    105 |   33 | intent lock: code hash, hash type                         |
//! |    138 |    2 | `root_history`: number of recent roots kept, little-endian |
//!
//! Intent lock script args (104 bytes):
//!
//! | offset | size | field                                                     |
//! |-------:|-----:|-----------------------------------------------------------|
//! |      0 |   32 | `pool_type_hash`: script hash of the pool's type script   |
//! |     32 |   32 | `commitment` of the note being deposited                  |
//! |     64 |   32 | `owner_lock_hash`: who may take the intent back           |
//! |     96 |    8 | `reclaim_since`: blocks to wait before that, little-endian |
//!
//! Pool cell data (`680 + 32 × root_history` bytes): the v1 layout with a ring
//! buffer of `root_history` roots instead of 30.
//!
//! Batch action, in the `input_type` field of the pool input's `WitnessArgs`:
//!
//! ```text
//! 0x02 | w (2, LE) | withdrawal × w
//! withdrawal: fee (8, LE) | root (32) | nullifier_hash (32) | groth16 proof (128)
//!             | len (2, LE) | nullifier-set proof (len)
//! ```
//!
//! Deposits are not in the witness: they are the transaction's intent inputs.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use blake2b_ref::Blake2bBuilder;
use mixer_core::action::{Withdraw, PROOF_LEN};
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes, Bytes32};
use mixer_core::merkle::{self, TREE_CAPACITY, TREE_DEPTH};
use mixer_core::{smt, Error};

pub const POOL_ARGS_LEN: usize = 32 + 8 + 32 + 33 + 33 + 2;
pub const INTENT_ARGS_LEN: usize = 32 + 32 + 32 + 8;
pub const TAG_BATCH: u8 = 2;

const WITHDRAWAL_FIXED_LEN: usize = 8 + 32 + 32 + PROOF_LEN + 2;

fn bytes32(bytes: &[u8]) -> Option<Bytes32> {
    bytes.try_into().ok()
}

/// A script's code, as a script names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptId {
    pub code_hash: Bytes32,
    pub hash_type: u8,
}

/// The pool type script's args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    pub type_id: Bytes32,
    pub denomination: u64,
    pub vk_hash: Bytes32,
    pub pool_lock: ScriptId,
    pub intent_lock: ScriptId,
    pub root_history: u16,
}

impl PoolConfig {
    pub fn parse(args: &[u8]) -> Result<Self, Error> {
        Self::decode(args).ok_or(Error::ScriptArgs)
    }

    fn decode(args: &[u8]) -> Option<Self> {
        if args.len() != POOL_ARGS_LEN {
            return None;
        }
        let config = Self {
            type_id: bytes32(&args[..32])?,
            denomination: u64::from_le_bytes(args[32..40].try_into().ok()?),
            vk_hash: bytes32(&args[40..72])?,
            pool_lock: ScriptId { code_hash: bytes32(&args[72..104])?, hash_type: args[104] },
            intent_lock: ScriptId { code_hash: bytes32(&args[105..137])?, hash_type: args[137] },
            root_history: u16::from_le_bytes(args[138..140].try_into().ok()?),
        };
        if config.denomination == 0 || config.root_history == 0 {
            return None;
        }
        Some(config)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(POOL_ARGS_LEN);
        out.extend_from_slice(&self.type_id);
        out.extend_from_slice(&self.denomination.to_le_bytes());
        out.extend_from_slice(&self.vk_hash);
        out.extend_from_slice(&self.pool_lock.code_hash);
        out.push(self.pool_lock.hash_type);
        out.extend_from_slice(&self.intent_lock.code_hash);
        out.push(self.intent_lock.hash_type);
        out.extend_from_slice(&self.root_history.to_le_bytes());
        out
    }
}

/// CKB's Type ID: the hash of the creating transaction's first input and the
/// index of the output being created. An input is spent once, so no second cell
/// can ever be created with the same value.
///
/// `first_input` is the serialized `CellInput` (44 bytes).
pub fn type_id(first_input: &[u8], output_index: u64) -> Bytes32 {
    let mut hasher = Blake2bBuilder::new(32).personal(b"ckb-default-hash").build();
    hasher.update(first_input);
    hasher.update(&output_index.to_le_bytes());
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// The intent lock script's args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentArgs {
    pub pool_type_hash: Bytes32,
    pub commitment: Bytes32,
    pub owner_lock_hash: Bytes32,
    pub reclaim_since: u64,
}

impl IntentArgs {
    pub fn parse(args: &[u8]) -> Option<Self> {
        if args.len() != INTENT_ARGS_LEN {
            return None;
        }
        Some(Self {
            pool_type_hash: bytes32(&args[..32])?,
            commitment: bytes32(&args[32..64])?,
            owner_lock_hash: bytes32(&args[64..96])?,
            reclaim_since: u64::from_le_bytes(args[96..].try_into().ok()?),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(INTENT_ARGS_LEN);
        out.extend_from_slice(&self.pool_type_hash);
        out.extend_from_slice(&self.commitment);
        out.extend_from_slice(&self.owner_lock_hash);
        out.extend_from_slice(&self.reclaim_since.to_le_bytes());
        out
    }
}

/// The `since` an input needs for its owner to take an intent back after
/// `blocks` blocks: relative, counted in blocks.
pub const fn reclaim_since(blocks: u64) -> u64 {
    SINCE_RELATIVE_BLOCKS | blocks
}

/// Whether an input's `since` makes the chain enforce that the intent cell has
/// been live for at least `blocks` blocks.
///
/// Only the relative block-number form counts. An absolute `since`, or one in
/// epochs or seconds, says nothing about the cell's age in blocks.
pub const fn since_allows_reclaim(since: u64, blocks: u64) -> bool {
    since & !SINCE_VALUE_MASK == SINCE_RELATIVE_BLOCKS && since & SINCE_VALUE_MASK >= blocks
}

const SINCE_RELATIVE_BLOCKS: u64 = 0x8000_0000_0000_0000;
const SINCE_VALUE_MASK: u64 = 0x00ff_ffff_ffff_ffff;

/// Why the intent lock refuses a transaction; the discriminant is its exit code.
/// Kept apart from the pool type script's codes and the pool lock's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum IntentError {
    /// A syscall failed unexpectedly.
    Syscall = 5,
    /// The lock args are not `pool_type_hash | commitment | owner_lock_hash | reclaim_since`.
    ScriptArgs = 6,
    /// The pool being spent would not count this cell as a deposit: its args are
    /// not a v2 pool's, or name another intent lock than this cell's.
    PoolMismatch = 60,
    /// An intent cell being deposited has a type script or carries data.
    IntentShape = 61,
    /// Neither the pool nor a cell of the owner is among the inputs.
    Unauthorized = 62,
    /// The owner takes the intent back without the `since` that proves the wait.
    ReclaimTooEarly = 63,
}

impl IntentError {
    pub const fn code(self) -> i8 {
        self as i8
    }
}

pub const fn state_len(root_history: u16) -> usize {
    8 + 32 * root_history as usize + 32 * TREE_DEPTH + 32
}

/// The pool cell's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub next_index: u32,
    pub root_pos: u32,
    /// Ring buffer of recent commitment-tree roots; never empty.
    pub roots: Vec<Bytes32>,
    pub frontier: [Bytes32; TREE_DEPTH],
    pub nullifier_root: Bytes32,
}

impl PoolState {
    /// State of a freshly created pool: no deposits, no spent nullifiers.
    ///
    /// `root_history` must not be zero; [`PoolConfig::parse`] guarantees it.
    pub fn empty(root_history: u16) -> Self {
        let mut roots = alloc::vec![[0u8; 32]; root_history as usize];
        roots[0] = fr_to_le_bytes(&merkle::empty_root());
        Self {
            next_index: 0,
            root_pos: 0,
            roots,
            frontier: [[0u8; 32]; TREE_DEPTH],
            nullifier_root: smt::EMPTY_ROOT,
        }
    }

    pub fn from_bytes(data: &[u8], root_history: u16) -> Result<Self, Error> {
        if root_history == 0 || data.len() != state_len(root_history) {
            return Err(Error::StateEncoding);
        }
        let (header, rest) = data.split_at(8);
        let (roots_bytes, rest) = rest.split_at(32 * root_history as usize);
        let (frontier_bytes, nullifier_root) = rest.split_at(32 * TREE_DEPTH);

        let next_index = u32::from_le_bytes(header[..4].try_into().map_err(|_| Error::StateEncoding)?);
        let root_pos = u32::from_le_bytes(header[4..].try_into().map_err(|_| Error::StateEncoding)?);
        if next_index > TREE_CAPACITY || root_pos >= u32::from(root_history) {
            return Err(Error::StateEncoding);
        }

        let mut state = Self {
            next_index,
            root_pos,
            roots: Vec::with_capacity(root_history as usize),
            frontier: [[0u8; 32]; TREE_DEPTH],
            nullifier_root: [0u8; 32],
        };
        for chunk in roots_bytes.chunks_exact(32) {
            state.roots.push(bytes32(chunk).ok_or(Error::StateEncoding)?);
        }
        for (slot, chunk) in state.frontier.iter_mut().zip(frontier_bytes.chunks_exact(32)) {
            slot.copy_from_slice(chunk);
        }
        state.nullifier_root.copy_from_slice(nullifier_root);
        Ok(state)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 32 * self.roots.len() + 32 * TREE_DEPTH + 32);
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

    /// Mark a note as withdrawn; the same rules as v1's
    /// `mixer_core::state::PoolState::withdraw`.
    ///
    /// A batch applies its withdrawals before its deposits, so each one is
    /// checked against the roots that existed before the transaction, and
    /// against the nullifier set as the withdrawals before it left it.
    pub fn withdraw(
        &mut self,
        root: &Bytes32,
        nullifier_hash: &Bytes32,
        nullifier_proof: &[u8],
    ) -> Result<(), Error> {
        if fr_from_le_bytes(root).is_none() {
            return Err(Error::NonCanonicalRoot);
        }
        if fr_from_le_bytes(nullifier_hash).is_none() {
            return Err(Error::NonCanonicalNullifier);
        }
        if !self.is_known_root(root) {
            return Err(Error::UnknownRoot);
        }
        self.nullifier_root = smt::mark_spent(&self.nullifier_root, nullifier_hash, nullifier_proof)?;
        Ok(())
    }

    /// Append a batch's deposits to the commitment tree, in order, and record
    /// one new root for all of them. Does nothing for an empty batch.
    pub fn deposit(&mut self, commitments: &[Bytes32]) -> Result<(), Error> {
        if commitments.is_empty() {
            return Ok(());
        }
        let mut leaves = Vec::with_capacity(commitments.len());
        for commitment in commitments {
            leaves.push(fr_from_le_bytes(commitment).ok_or(Error::NonCanonicalCommitment)?);
        }
        let root = merkle::append(self.next_index, &mut self.frontier, &leaves)?;
        self.next_index += leaves.len() as u32;
        self.root_pos = (self.root_pos + 1) % self.roots.len() as u32;
        self.roots[self.root_pos as usize] = fr_to_le_bytes(&root);
        Ok(())
    }
}

/// Decode a batch action into its withdrawals. `None` for anything that is not
/// exactly one well-formed batch, trailing bytes included.
pub fn decode_batch(bytes: &[u8]) -> Option<Vec<Withdraw<'_>>> {
    let (tag, rest) = bytes.split_first()?;
    if *tag != TAG_BATCH || rest.len() < 2 {
        return None;
    }
    let (count, mut rest) = rest.split_at(2);
    let count = u16::from_le_bytes(count.try_into().ok()?) as usize;
    // Checked before allocating, so a short witness cannot ask for a large buffer.
    if rest.len() < count.checked_mul(WITHDRAWAL_FIXED_LEN)? {
        return None;
    }

    let mut withdrawals = Vec::with_capacity(count);
    for _ in 0..count {
        if rest.len() < WITHDRAWAL_FIXED_LEN {
            return None;
        }
        let (fee, tail) = rest.split_at(8);
        let (root, tail) = tail.split_at(32);
        let (nullifier_hash, tail) = tail.split_at(32);
        let (proof, tail) = tail.split_at(PROOF_LEN);
        let (len, tail) = tail.split_at(2);
        let len = u16::from_le_bytes(len.try_into().ok()?) as usize;
        if tail.len() < len {
            return None;
        }
        let (nullifier_proof, tail) = tail.split_at(len);
        withdrawals.push(Withdraw {
            fee: u64::from_le_bytes(fee.try_into().ok()?),
            root: bytes32(root)?,
            nullifier_hash: bytes32(nullifier_hash)?,
            proof,
            nullifier_proof,
        });
        rest = tail;
    }
    if !rest.is_empty() {
        return None;
    }
    Some(withdrawals)
}

/// Encode a batch action. `None` if there are more withdrawals, or a longer
/// nullifier-set proof, than the two-byte length fields can express, or a
/// Groth16 proof of the wrong size.
pub fn encode_batch(withdrawals: &[Withdraw<'_>]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    out.push(TAG_BATCH);
    out.extend_from_slice(&u16::try_from(withdrawals.len()).ok()?.to_le_bytes());
    for withdraw in withdrawals {
        if withdraw.proof.len() != PROOF_LEN {
            return None;
        }
        out.extend_from_slice(&withdraw.fee.to_le_bytes());
        out.extend_from_slice(&withdraw.root);
        out.extend_from_slice(&withdraw.nullifier_hash);
        out.extend_from_slice(withdraw.proof);
        out.extend_from_slice(&u16::try_from(withdraw.nullifier_proof.len()).ok()?.to_le_bytes());
        out.extend_from_slice(withdraw.nullifier_proof);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_bn254::Fr;
    use mixer_core::state;

    fn commitment(n: u64) -> Bytes32 {
        fr_to_le_bytes(&Fr::from(n))
    }

    fn config() -> PoolConfig {
        PoolConfig {
            type_id: [1u8; 32],
            denomination: 0x0102_0304_0506_0708,
            vk_hash: [2u8; 32],
            pool_lock: ScriptId { code_hash: [3u8; 32], hash_type: 4 },
            intent_lock: ScriptId { code_hash: [5u8; 32], hash_type: 2 },
            root_history: 0x0180,
        }
    }

    #[test]
    fn pool_args_layout() {
        let config = config();
        let bytes = config.to_bytes();
        assert_eq!(bytes.len(), 140);
        assert_eq!(&bytes[..32], &[1u8; 32]);
        assert_eq!(&bytes[32..40], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&bytes[40..72], &[2u8; 32]);
        assert_eq!(&bytes[72..104], &[3u8; 32]);
        assert_eq!(bytes[104], 4);
        assert_eq!(&bytes[105..137], &[5u8; 32]);
        assert_eq!(bytes[137], 2);
        assert_eq!(&bytes[138..], &[0x80, 0x01]);
        assert_eq!(PoolConfig::parse(&bytes), Ok(config));
    }

    #[test]
    fn pool_args_rejects_malformed() {
        let good = config().to_bytes();
        assert_eq!(PoolConfig::parse(&good[..139]), Err(Error::ScriptArgs));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(PoolConfig::parse(&long), Err(Error::ScriptArgs));
        // v1 args are not v2 args.
        assert_eq!(PoolConfig::parse(&[1u8; 73]), Err(Error::ScriptArgs));

        let mut no_denomination = good.clone();
        no_denomination[32..40].fill(0);
        assert_eq!(PoolConfig::parse(&no_denomination), Err(Error::ScriptArgs));

        let mut no_history = good;
        no_history[138..].fill(0);
        assert_eq!(PoolConfig::parse(&no_history), Err(Error::ScriptArgs));
    }

    #[test]
    fn intent_args_layout() {
        let args = IntentArgs {
            pool_type_hash: [1u8; 32],
            commitment: [2u8; 32],
            owner_lock_hash: [3u8; 32],
            reclaim_since: 0x0102,
        };
        let bytes = args.to_bytes();
        assert_eq!(bytes.len(), 104);
        assert_eq!(&bytes[..32], &[1u8; 32]);
        assert_eq!(&bytes[32..64], &[2u8; 32]);
        assert_eq!(&bytes[64..96], &[3u8; 32]);
        assert_eq!(&bytes[96..], &[2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(IntentArgs::parse(&bytes), Some(args));
        assert_eq!(IntentArgs::parse(&bytes[..103]), None);
        assert_eq!(IntentArgs::parse(&[bytes.as_slice(), &[0]].concat()), None);
    }

    #[test]
    fn reclaim_needs_a_relative_block_since() {
        assert_eq!(reclaim_since(100), 0x8000_0000_0000_0064);
        assert!(since_allows_reclaim(reclaim_since(100), 100));
        assert!(since_allows_reclaim(reclaim_since(101), 100));
        assert!(!since_allows_reclaim(reclaim_since(99), 100));
        assert!(since_allows_reclaim(reclaim_since(0), 0));

        // No since at all, and an absolute block number however large.
        assert!(!since_allows_reclaim(0, 0));
        assert!(!since_allows_reclaim(1_000_000, 100));
        // Relative, but in epochs (metric 01) or seconds (metric 10).
        assert!(!since_allows_reclaim(0xa000_0000_0000_0000 | 1_000_000, 100));
        assert!(!since_allows_reclaim(0xc000_0000_0000_0000 | 1_000_000, 100));
        // Reserved bits set.
        assert!(!since_allows_reclaim(0x8100_0000_0000_0000 | 1_000_000, 100));
        // A wait that does not fit the 56-bit value can never be met.
        assert!(!since_allows_reclaim(reclaim_since(SINCE_VALUE_MASK), u64::MAX));
    }

    #[test]
    fn type_id_depends_on_input_and_index() {
        let input = [7u8; 44];
        let id = type_id(&input, 0);
        assert_ne!(id, type_id(&input, 1));
        let mut other = input;
        other[43] ^= 1;
        assert_ne!(id, type_id(&other, 0));
        assert_eq!(id, type_id(&input, 0));
    }

    #[test]
    fn state_layout_matches_v1_at_30_roots() {
        assert_eq!(state_len(30), state::STATE_LEN);
        assert_eq!(state_len(1), 712);
        assert_eq!(state_len(128), 4776);
        assert_eq!(PoolState::empty(30).to_bytes(), state::PoolState::empty().to_bytes());

        let mut old = state::PoolState::empty();
        let mut new = PoolState::empty(30);
        for n in 1..=40u64 {
            old.deposit(&commitment(n)).unwrap();
            new.deposit(&[commitment(n)]).unwrap();
            assert_eq!(new.to_bytes(), old.to_bytes());
        }
        let nullifier = commitment(77);
        let root = *new.current_root();
        old.withdraw(&root, &nullifier, &[0u8; 32]).unwrap();
        new.withdraw(&root, &nullifier, &[0u8; 32]).unwrap();
        assert_eq!(new.to_bytes(), old.to_bytes());
        assert_eq!(PoolState::from_bytes(&new.to_bytes(), 30), Ok(new));
    }

    #[test]
    fn state_rejects_malformed() {
        let good = PoolState::empty(4).to_bytes();
        assert_eq!(good.len(), state_len(4));
        assert_eq!(PoolState::from_bytes(&good, 4), Ok(PoolState::empty(4)));
        // The length is fixed by the pool's args, not by the data.
        assert_eq!(PoolState::from_bytes(&good, 3), Err(Error::StateEncoding));
        assert_eq!(PoolState::from_bytes(&good, 5), Err(Error::StateEncoding));
        assert_eq!(PoolState::from_bytes(&good, 0), Err(Error::StateEncoding));
        assert_eq!(PoolState::from_bytes(&good[..good.len() - 1], 4), Err(Error::StateEncoding));

        let mut bad_pos = good.clone();
        bad_pos[4] = 4;
        assert_eq!(PoolState::from_bytes(&bad_pos, 4), Err(Error::StateEncoding));

        let mut bad_index = good;
        bad_index[..4].copy_from_slice(&(TREE_CAPACITY + 1).to_le_bytes());
        assert_eq!(PoolState::from_bytes(&bad_index, 4), Err(Error::StateEncoding));
    }

    #[test]
    fn root_history_length_is_respected() {
        for history in [1u16, 2, 5] {
            let mut state = PoolState::empty(history);
            let mut seen = alloc::vec![*state.current_root()];
            for n in 0..10u64 {
                state.deposit(&[commitment(n + 1)]).unwrap();
                seen.push(*state.current_root());
                let known = seen.iter().filter(|root| state.is_known_root(root)).count();
                assert_eq!(known, seen.len().min(history as usize));
                // The newest roots are the ones kept.
                assert!(seen.iter().rev().take(history as usize).all(|root| state.is_known_root(root)));
            }
            assert_eq!(PoolState::from_bytes(&state.to_bytes(), history), Ok(state));
        }
    }

    #[test]
    fn batch_of_deposits_records_one_root() {
        let commitments: Vec<Bytes32> = (1..=5u64).map(commitment).collect();
        let mut batch = PoolState::empty(8);
        batch.deposit(&commitments).unwrap();
        assert_eq!((batch.next_index, batch.root_pos), (5, 1));

        let mut single = PoolState::empty(8);
        for commitment in &commitments {
            single.deposit(core::slice::from_ref(commitment)).unwrap();
        }
        assert_eq!(batch.current_root(), single.current_root());

        // No deposits: nothing changes, no root is burned.
        let before = batch.clone();
        batch.deposit(&[]).unwrap();
        assert_eq!(batch, before);
    }

    #[test]
    fn deposit_rejects_bad_input() {
        let mut state = PoolState::empty(8);
        assert_eq!(state.deposit(&[commitment(1), [0xff; 32]]), Err(Error::NonCanonicalCommitment));
        assert_eq!(state, PoolState::empty(8));

        state.next_index = TREE_CAPACITY - 1;
        assert_eq!(state.deposit(&[commitment(1), commitment(2)]), Err(Error::TreeFull));
    }

    #[test]
    fn withdraw_checks_root_and_nullifier() {
        let mut state = PoolState::empty(8);
        state.deposit(&[commitment(1)]).unwrap();
        let root = *state.current_root();
        let nullifier = commitment(77);
        let empty_set_proof = [0u8; 32];

        assert_eq!(state.withdraw(&[0xff; 32], &nullifier, &empty_set_proof), Err(Error::NonCanonicalRoot));
        assert_eq!(state.withdraw(&root, &[0xff; 32], &empty_set_proof), Err(Error::NonCanonicalNullifier));
        assert_eq!(state.withdraw(&commitment(5), &nullifier, &empty_set_proof), Err(Error::UnknownRoot));
        assert_eq!(state.withdraw(&[0u8; 32], &nullifier, &empty_set_proof), Err(Error::UnknownRoot));
        assert_eq!(state.withdraw(&root, &nullifier, &empty_set_proof), Ok(()));
        assert_eq!(state.withdraw(&root, &nullifier, &empty_set_proof), Err(Error::NullifierSpent));
    }

    fn sample<'a>(n: u8, proof: &'a [u8], nullifier_proof: &'a [u8]) -> Withdraw<'a> {
        Withdraw {
            fee: u64::from(n) << 40 | 9,
            root: [n; 32],
            nullifier_hash: [n.wrapping_add(1); 32],
            proof,
            nullifier_proof,
        }
    }

    #[test]
    fn batch_roundtrip() {
        assert_eq!(encode_batch(&[]), Some(alloc::vec![TAG_BATCH, 0, 0]));
        assert_eq!(decode_batch(&[TAG_BATCH, 0, 0]), Some(Vec::new()));

        let proof_a = [7u8; PROOF_LEN];
        let proof_b = [8u8; PROOF_LEN];
        let long = [5u8; 32 + 64];
        let withdrawals = [sample(1, &proof_a, &[0u8; 32]), sample(2, &proof_b, &long), sample(3, &proof_a, &[])];
        let bytes = encode_batch(&withdrawals).unwrap();
        assert_eq!(bytes.len(), 3 + 3 * WITHDRAWAL_FIXED_LEN + 32 + 96);
        assert_eq!(&bytes[..3], &[TAG_BATCH, 3, 0]);
        assert_eq!(decode_batch(&bytes).as_deref(), Some(&withdrawals[..]));
    }

    #[test]
    fn batch_rejects_malformed() {
        let proof = [7u8; PROOF_LEN];
        let good = encode_batch(&[sample(1, &proof, &[0u8; 32]), sample(2, &proof, &[0u8; 32])]).unwrap();
        assert!(decode_batch(&good).is_some());

        assert_eq!(decode_batch(&[]), None);
        assert_eq!(decode_batch(&[TAG_BATCH]), None);
        assert_eq!(decode_batch(&[TAG_BATCH, 0]), None);
        // v1 actions are not batches.
        assert_eq!(decode_batch(&[0u8; 33]), None);
        assert_eq!(decode_batch(&[1u8; 300]), None);

        // Trailing bytes, a truncated tail, and a count that disagrees with the body.
        assert_eq!(decode_batch(&[good.as_slice(), &[0]].concat()), None);
        assert_eq!(decode_batch(&good[..good.len() - 1]), None);
        for count in [0u8, 1, 3] {
            let mut wrong = good.clone();
            wrong[1] = count;
            assert_eq!(decode_batch(&wrong), None, "count {count}");
        }
        // A count far beyond the body is refused without allocating for it.
        assert_eq!(decode_batch(&[TAG_BATCH, 0xff, 0xff]), None);

        // An inner length that reaches into the next withdrawal.
        let mut overlong = good.clone();
        overlong[3 + WITHDRAWAL_FIXED_LEN - 2] = 33;
        assert_eq!(decode_batch(&overlong), None);

        assert_eq!(encode_batch(&[sample(1, &proof[..127], &[])]), None);
    }
}
