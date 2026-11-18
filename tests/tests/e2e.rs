//! End-to-end tests against the real contract binaries in CKB-VM.
//!
//! The first group runs the honest flows. Every other test plays an attacker:
//! it starts from the transaction an honest client would send, changes one
//! thing, and checks that the pool rejects it for the expected reason.

use ark_ff::{BigInteger, PrimeField};
use ckb_testtool::ckb_types::{
    bytes::Bytes,
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{CellDep, CellInput, CellOutput, WitnessArgs},
    prelude::*,
};
use mixer_core::field::fr_to_le_bytes;
use mixer_core::state::{PoolState, ROOT_HISTORY};
use mixer_core::{action, smt, Bytes32, Error, Fr};
use mixer_tests::harness::{
    assert_exit_code, assert_rejected, assert_rejected_one_of, capacity_of, plain_cell, prove_note,
    script_hash, Env, Note, Pool, WithdrawParams, CKB, DENOMINATION, POOL_LOCK_NOT_A_POOL_CELL, TX_FEE,
};
use mixer_tests::snark::{self, WithdrawWitness};

/// Fee the withdrawals in these tests leave to the relayer and the miner.
const FEE: u64 = CKB / 100;

/// A pool with `count` notes deposited, one transaction each.
fn pool_with_notes(count: u64) -> (Env, Pool, Vec<Note>) {
    let mut env = Env::new();
    let (mut pool, _) = Pool::create(&mut env, DENOMINATION);
    let notes: Vec<Note> = (0..count).map(Note::from_seed).collect();
    for note in &notes {
        pool.deposit(&mut env, &[note.commitment()]);
    }
    (env, pool, notes)
}

/// `value + r`: another 32-byte string that stands for the same field element.
fn alias(value: &Bytes32) -> Bytes32 {
    let modulus = Fr::MODULUS.to_bytes_le();
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in 0..32 {
        let sum = u16::from(value[i]) + u16::from(modulus[i]) + carry;
        out[i] = sum as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0, "the alias fits in 32 bytes");
    out
}

fn with_witness(tx: &TransactionView, witness: Bytes) -> TransactionView {
    tx.as_advanced_builder().set_witnesses(vec![witness.pack()]).build()
}

fn action_witness(action: Vec<u8>) -> Bytes {
    WitnessArgs::new_builder().input_type(Some(Bytes::from(action)).pack()).build().as_bytes()
}

/// The prover must fail on this witness, and because one of the circuit's own
/// constraints does not hold, not for some unrelated reason.
fn assert_unprovable(witness: &WithdrawWitness) {
    let error = snark::prove(witness).expect_err("the circuit should not accept this witness");
    assert!(error.contains("Assert Failed"), "the prover failed for another reason: {error}");
}

// ---------------------------------------------------------------- honest flows

#[test]
fn deposit_then_withdraw() {
    let (mut env, mut pool, notes) = pool_with_notes(1);
    let empty_pool_capacity = pool.capacity - DENOMINATION;
    assert_eq!(pool.state.next_index, 1);

    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let (tx, _) = pool.withdraw(&mut env, &proof);

    // Nobody signs a withdrawal: the pool cell is the only input.
    assert_eq!(tx.inputs().len(), 1);
    let payout = tx.output(1).expect("recipient output");
    assert_eq!(payout.lock(), recipient);
    assert_eq!(capacity_of(&payout), DENOMINATION - FEE);
    assert_eq!(pool.capacity, empty_pool_capacity);
    assert!(pool.nullifiers.contains(&proof.nullifier_hash));
    // A withdrawal does not touch the commitment tree.
    assert_eq!(pool.state.next_index, 1);
    assert_eq!(*pool.state.current_root(), proof.root);
}

#[test]
fn batch_deposit_and_withdrawals_in_any_order() {
    let mut env = Env::new();
    let (mut pool, _) = Pool::create(&mut env, DENOMINATION);
    let empty_pool_capacity = pool.capacity;
    let notes: Vec<Note> = (0..4).map(Note::from_seed).collect();
    let commitments: Vec<Fr> = notes.iter().map(Note::commitment).collect();

    pool.deposit(&mut env, &commitments);
    assert_eq!(pool.state.next_index, 4);
    assert_eq!(pool.capacity, empty_pool_capacity + 4 * DENOMINATION);

    for (owner, index) in [(0xa2u8, 2u32), (0xa0, 0), (0xa3, 3)] {
        let recipient = env.user_lock(owner);
        let proof = pool.prove_withdraw(&env, &notes[index as usize], index, &recipient, FEE);
        pool.withdraw(&mut env, &proof);
    }
    assert_eq!(pool.nullifiers.len(), 3);
    assert_eq!(pool.capacity, empty_pool_capacity + DENOMINATION);
}

#[test]
fn deposits_and_withdrawals_interleave() {
    let (mut env, mut pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);

    let first = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    pool.withdraw(&mut env, &first);

    let late = Note::from_seed(50);
    pool.deposit(&mut env, &[late.commitment()]);

    let second = pool.prove_withdraw(&env, &late, 2, &recipient, 0);
    pool.withdraw(&mut env, &second);
    let third = pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE);
    pool.withdraw(&mut env, &third);
    assert_eq!(pool.nullifiers.len(), 3);
}

#[test]
fn proof_stays_valid_while_its_root_is_in_the_history() {
    let (mut env, mut pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    // 29 more deposit transactions: the proof's root is now the oldest of the
    // 30 the pool remembers.
    for seed in 0..ROOT_HISTORY as u64 - 1 {
        pool.deposit(&mut env, &[Note::from_seed(1000 + seed).commitment()]);
    }
    assert_ne!(*pool.state.current_root(), proof.root);
    let params = pool.withdraw_params(&env, &proof);
    let tx = pool.withdraw_tx(&mut env, &params);
    env.verify(&tx).expect("the oldest remembered root is accepted");

    // One more and the root is gone.
    pool.deposit(&mut env, &[Note::from_seed(2000).commitment()]);
    let params = pool.withdraw_params(&env, &proof);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::UnknownRoot);

    // The note is not lost: a new proof against the current root works.
    let fresh = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    pool.withdraw(&mut env, &fresh);
}

#[test]
fn a_commitment_deposited_twice_is_withdrawable_twice() {
    let mut env = Env::new();
    let (mut pool, _) = Pool::create(&mut env, DENOMINATION);
    let note = Note::from_seed(7);
    let bystander = Note::from_seed(8);
    pool.deposit(&mut env, &[note.commitment(), note.commitment(), bystander.commitment()]);

    let recipient = env.user_lock(0xaa);
    let at_leaf_0 = pool.prove_withdraw(&env, &note, 0, &recipient, FEE);
    let at_leaf_1 = pool.prove_withdraw(&env, &note, 1, &recipient, FEE);
    // The nullifier hash depends on the leaf position, so the second deposit of
    // the same note is not locked up by the first withdrawal.
    assert_ne!(at_leaf_0.nullifier_hash, at_leaf_1.nullifier_hash);
    pool.withdraw(&mut env, &at_leaf_0);
    pool.withdraw(&mut env, &at_leaf_1);

    // Two deposits, two withdrawals, not three: the bystander's deposit stays.
    let params = pool.withdraw_params(&env, &at_leaf_0);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);
}

// ---------------------------------------------------------------- double spend

#[test]
fn a_spent_note_cannot_be_withdrawn_again() {
    let (mut env, mut pool, notes) = pool_with_notes(3);
    let recipient = env.user_lock(0xaa);

    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let before_spend = pool.withdraw_params(&env, &proof);
    pool.withdraw(&mut env, &proof);

    // Same proof again.
    let params = pool.withdraw_params(&env, &proof);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);

    // Someone else withdraws, so the nullifier set has moved on.
    let other = pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE);
    pool.withdraw(&mut env, &other);

    // The path of the spent nullifier in the current set, which ends in a spent leaf.
    let params = pool.withdraw_params(&env, &proof);
    assert_ne!(params.nullifier_proof, before_spend.nullifier_proof);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);

    // The absence proof from before the spend, replayed.
    let mut params = pool.withdraw_params(&env, &proof);
    params.nullifier_proof = before_spend.nullifier_proof.clone();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);

    // A new proof for the same note (new randomness, new recipient) carries the
    // same nullifier hash.
    let thief = env.user_lock(0xee);
    let again = pool.prove_withdraw(&env, &notes[0], 0, &thief, FEE);
    assert_eq!(again.nullifier_hash, proof.nullifier_hash);
    assert_ne!(again.proof, proof.proof);
    let params = pool.withdraw_params(&env, &again);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);
}

#[test]
fn an_aliased_nullifier_hash_does_not_spend_the_note_again() {
    let (mut env, mut pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    pool.withdraw(&mut env, &proof);

    // nullifier_hash + r is the same field element, so the Groth16 equation
    // would hold for it, but it is a different key in the nullifier set: the
    // set honestly says it is unspent.
    let aliased = alias(&proof.nullifier_hash);
    assert!(!pool.nullifiers.contains(&aliased));
    let absence = pool.nullifiers.proof(&aliased);
    let mut next = pool.state.clone();
    next.nullifier_root = smt::mark_spent(&pool.state.nullifier_root, &aliased, &absence)
        .expect("the alias really is absent from the nullifier set");

    let mut params = pool.withdraw_params(&env, &proof);
    params.nullifier_hash = aliased;
    params.nullifier_proof = absence;
    params.pool_data = next.to_bytes();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NonCanonicalNullifier);

    // Second line of defence: the verifier refuses non-canonical public inputs too.
    let public_inputs = action::public_inputs(&proof.root, &aliased, &script_hash(&recipient), FEE);
    assert_eq!(
        verifier_core::verify(&env.vk, &proof.proof, &public_inputs),
        Err(verifier_core::VerifyError::InvalidPublicInputs)
    );
}

#[test]
fn an_aliased_root_is_rejected() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    let mut params = pool.withdraw_params(&env, &proof);
    params.root = alias(&proof.root);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NonCanonicalRoot);
}

#[test]
fn nullifier_set_proofs_cannot_be_forged_or_padded() {
    let (mut env, mut pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    let spent = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    pool.withdraw(&mut env, &spent);
    let proof = pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE);
    let honest = pool.withdraw_params(&env, &proof);
    // One nullifier is spent, so the honest proof carries exactly one sibling.
    assert_eq!(honest.nullifier_proof.len(), 64);

    // Trailing bytes.
    let mut params = honest.clone();
    params.nullifier_proof.push(0);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierProof);

    // A sibling that the bitmap announces but the proof does not contain.
    let mut params = honest.clone();
    params.nullifier_proof.truncate(63);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierProof);

    // An empty sibling written out instead of left to the bitmap.
    let mut params = honest.clone();
    params.nullifier_proof[32..].fill(0);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierProof);

    // A wrong sibling: the path no longer leads to the pool's nullifier root.
    let mut params = honest.clone();
    params.nullifier_proof[40] ^= 1;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);

    // Pretending the set is empty.
    let mut params = honest.clone();
    params.nullifier_proof = vec![0u8; 32];
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NullifierSpent);

    let tx = pool.withdraw_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest proof is accepted");
}

// ------------------------------------------------- recipient and fee binding

#[test]
fn a_proof_pays_only_its_recipient() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let thief = env.user_lock(0xee);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    // A front-runner copies the proof out of the mempool and pays themselves.
    let mut params = pool.withdraw_params(&env, &proof);
    params.recipient = Some((plain_cell(&thief, DENOMINATION - FEE), Bytes::new()));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);
}

#[test]
fn a_proof_fixes_the_fee() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    // A relayer raises its fee and pays the recipient that much less.
    let greedy_fee = 500 * CKB;
    let mut params = pool.withdraw_params(&env, &proof);
    params.fee = greedy_fee;
    params.recipient = Some((plain_cell(&recipient, DENOMINATION - greedy_fee), Bytes::new()));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);

    // Keeping the proven fee and simply paying less.
    let mut params = pool.withdraw_params(&env, &proof);
    params.recipient = Some((plain_cell(&recipient, DENOMINATION - greedy_fee), Bytes::new()));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientCapacity);

    // A fee above the denomination.
    let mut params = pool.withdraw_params(&env, &proof);
    params.fee = DENOMINATION + 1;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::FeeTooLarge);
}

#[test]
fn the_recipient_gets_a_plain_cell_right_after_the_pool() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let honest = pool.withdraw_params(&env, &proof);
    let (payout, _) = honest.recipient.clone().expect("recipient");

    // A type script on the payout could make the cell unspendable for its owner.
    let foreign_type = env.user_lock(0x77);
    let mut params = honest.clone();
    params.recipient = Some((
        payout.clone().as_builder().type_(Some(foreign_type).pack()).build(),
        Bytes::new(),
    ));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientShape);

    // So could data.
    let mut params = honest.clone();
    params.recipient = Some((payout, Bytes::from(vec![1u8])));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientShape);

    // No payout at all: the capacity would go to the miner.
    let mut params = honest;
    params.recipient = None;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientMissing);
}

// ------------------------------------------------------------- forged proofs

#[test]
fn a_proof_for_a_tree_the_pool_never_had_is_rejected() {
    let (mut env, pool, _) = pool_with_notes(2);
    let thief = env.user_lock(0xee);

    // The thief never deposits. They append their note to a private copy of the
    // tree, which gives a perfectly valid proof for a root the pool never had.
    let note = Note::from_seed(666);
    let mut leaves = pool.leaves.clone();
    leaves.push(note.commitment());
    let proof = prove_note(&env.vk, &leaves, &note, 2, &thief, FEE);

    let params = pool.withdraw_params(&env, &proof);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::UnknownRoot);
}

#[test]
fn proofs_cannot_be_mixed_tampered_or_made_up() {
    let (mut env, pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    let proof_0 = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let proof_1 = pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE);

    // The proof of one note with the nullifier hash of another.
    let mut params = pool.withdraw_params(&env, &proof_1);
    params.proof = proof_0.proof.clone();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);

    // A nullifier hash that no note produces.
    let made_up = fr_to_le_bytes(&Fr::from(123_456_789u64));
    let absence = pool.nullifiers.proof(&made_up);
    let mut next = pool.state.clone();
    next.withdraw(&proof_0.root, &made_up, &absence).expect("the pool state allows it");
    let mut params = pool.withdraw_params(&env, &proof_0);
    params.nullifier_hash = made_up;
    params.nullifier_proof = absence;
    params.pool_data = next.to_bytes();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);

    // One flipped bit in each of the proof's three points. Depending on the bit
    // the point either stops being a curve point or stops satisfying the
    // pairing check.
    for byte in [5usize, 40, 100] {
        let mut params = pool.withdraw_params(&env, &proof_0);
        params.proof[byte] ^= 0x04;
        let tx = pool.withdraw_tx(&mut env, &params);
        assert_rejected_one_of(env.verify(&tx), &[Error::InvalidProof, Error::ProofRejected]);
    }

    // No proof at all.
    let mut params = pool.withdraw_params(&env, &proof_0);
    params.proof = vec![0u8; action::PROOF_LEN];
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected_one_of(env.verify(&tx), &[Error::InvalidProof, Error::ProofRejected]);
}

#[test]
fn the_verifying_key_cannot_be_swapped() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    let mut params = pool.withdraw_params(&env, &proof);
    params.vk_dep = None;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::VkNotFound);

    // Another key in a cell dep does not help: the pool's type script args
    // commit to the hash of the one key it accepts.
    let mut other_vk = env.vk.clone();
    *other_vk.last_mut().expect("key bytes") ^= 1;
    let other_vk_cell = env.context.deploy_cell(other_vk.into());
    let mut params = pool.withdraw_params(&env, &proof);
    params.vk_dep = Some(CellDep::new_builder().out_point(other_vk_cell).build());
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::VkNotFound);
}

#[test]
fn the_prover_refuses_false_statements() {
    let (env, pool, notes) = pool_with_notes(2);
    let note = notes[0];
    let recipient = Fr::from(42u64);
    let honest = WithdrawWitness::for_leaf(
        &pool.leaves,
        0,
        note.nullifier,
        note.secret,
        note.nullifier_hash(0),
        recipient,
        FEE,
    );
    snark::prove(&honest).expect("an honest witness is provable");
    drop(env);

    // Not knowing the secret.
    let mut witness = honest.clone();
    witness.secret += Fr::from(1u64);
    assert_unprovable(&witness);

    // A second nullifier hash for the same deposit: first by just naming it,
    // then by also claiming the position it belongs to.
    let mut witness = honest.clone();
    witness.nullifier_hash = note.nullifier_hash(1);
    assert_unprovable(&witness);
    let mut witness = honest.clone();
    witness.leaf_index = 1;
    witness.nullifier_hash = note.nullifier_hash(1);
    assert_unprovable(&witness);

    // A root the path does not lead to.
    let mut witness = honest.clone();
    witness.root += Fr::from(1u64);
    assert_unprovable(&witness);

    // A note that was never deposited.
    let stranger = Note::from_seed(999);
    let mut witness = honest;
    witness.nullifier = stranger.nullifier;
    witness.secret = stranger.secret;
    witness.nullifier_hash = stranger.nullifier_hash(0);
    assert_unprovable(&witness);
}

// ------------------------------------------------------ pool state and funds

#[test]
fn a_withdrawal_releases_exactly_one_denomination() {
    let (mut env, pool, notes) = pool_with_notes(3);
    let recipient = env.user_lock(0xaa);
    let thief = env.user_lock(0xee);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);

    // One valid proof, two denominations out: the second goes to the thief.
    let mut params = pool.withdraw_params(&env, &proof);
    params.pool_capacity -= DENOMINATION;
    params.extra_outputs.push((plain_cell(&thief, DENOMINATION), Bytes::new()));
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputCapacity);

    // Draining the pool down to the capacity its cell occupies.
    let mut params = pool.withdraw_params(&env, &proof);
    let drained = params.pool_capacity - 2 * DENOMINATION;
    params.extra_outputs.push((plain_cell(&thief, params.pool_capacity - drained), Bytes::new()));
    params.pool_capacity = drained;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputCapacity);
}

#[test]
fn a_withdrawal_must_produce_exactly_the_next_state() {
    let (mut env, pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let honest = pool.withdraw_params(&env, &proof);
    let next = PoolState::from_bytes(&honest.pool_data).expect("state");

    // The nullifier is not recorded, so the note could be withdrawn again.
    let mut params = honest.clone();
    params.pool_data = pool.state.to_bytes();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // A root of the attacker's choosing slipped into the history.
    let mut tampered = next.clone();
    tampered.roots[7] = fr_to_le_bytes(&Fr::from(1234u64));
    let mut params = honest.clone();
    params.pool_data = tampered.to_bytes();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // The leaf counter rolled back, so the next deposit would overwrite a leaf.
    let mut tampered = next.clone();
    tampered.next_index = 0;
    let mut params = honest.clone();
    params.pool_data = tampered.to_bytes();
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // Extra bytes after the state.
    let mut params = honest;
    params.pool_data.push(0);
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);
}

#[test]
fn a_deposit_pays_exactly_the_denomination_per_commitment() {
    let (mut env, pool, _) = pool_with_notes(1);
    let one = [Note::from_seed(10).commitment()];
    let two = [Note::from_seed(10).commitment(), Note::from_seed(11).commitment()];

    // Nothing, one shannon short, one shannon over, and taking money out.
    for capacity in [
        pool.capacity,
        pool.capacity + DENOMINATION - 1,
        pool.capacity + DENOMINATION + 1,
        pool.capacity - DENOMINATION,
    ] {
        let mut params = pool.deposit_params(&one);
        params.pool_capacity = capacity;
        let tx = pool.deposit_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::OutputCapacity);
    }

    // Two notes for the price of one.
    let mut params = pool.deposit_params(&two);
    params.pool_capacity = pool.capacity + DENOMINATION;
    let tx = pool.deposit_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputCapacity);

    let params = pool.deposit_params(&two);
    let tx = pool.deposit_tx(&mut env, &params);
    env.verify(&tx).expect("two notes for two denominations");
}

#[test]
fn a_deposit_must_produce_exactly_the_next_state() {
    let (mut env, mut pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    pool.withdraw(&mut env, &proof);

    let commitment = [Note::from_seed(10).commitment()];
    let honest = pool.deposit_params(&commitment);
    let next = PoolState::from_bytes(&honest.pool_data).expect("state");

    // The root of a tree that holds a note the depositor never paid for.
    let mut forged_leaves = pool.leaves.clone();
    forged_leaves.push(commitment[0]);
    forged_leaves.push(Note::from_seed(666).commitment());
    let (_, forged_root) =
        mixer_core::merkle::merkle_path(&forged_leaves, 0).expect("path in the forged tree");
    let mut tampered = next.clone();
    tampered.roots[tampered.root_pos as usize] = fr_to_le_bytes(&forged_root);
    let mut params = honest.clone();
    params.pool_data = tampered.to_bytes();
    let tx = pool.deposit_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // Wiping the nullifier set would make every withdrawn note spendable again.
    let mut tampered = next.clone();
    tampered.nullifier_root = smt::EMPTY_ROOT;
    let mut params = honest.clone();
    params.pool_data = tampered.to_bytes();
    let tx = pool.deposit_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // Paying, but leaving the state as it was: the deposit would not exist.
    let mut params = honest.clone();
    params.pool_data = pool.state.to_bytes();
    let tx = pool.deposit_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // A commitment that is not a canonical field element. Its alias would be a
    // second leaf value for the same note.
    let mut params = honest.clone();
    params.commitments[0] = alias(&params.commitments[0]);
    let tx = pool.deposit_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NonCanonicalCommitment);

    let tx = pool.deposit_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest deposit is accepted");
}

#[test]
fn malformed_actions_are_rejected() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let params = pool.deposit_params(&[Note::from_seed(10).commitment()]);
    let deposit = pool.deposit_tx(&mut env, &params);

    // No witness for the pool input, a witness without an action, an empty action.
    let tx = deposit.as_advanced_builder().set_witnesses(Vec::new()).build();
    assert_rejected(env.verify(&tx), Error::Witness);
    let tx = with_witness(&deposit, WitnessArgs::new_builder().build().as_bytes());
    assert_rejected(env.verify(&tx), Error::Witness);
    let tx = with_witness(&deposit, action_witness(Vec::new()));
    assert_rejected(env.verify(&tx), Error::ActionEncoding);

    // An unknown action, a deposit of nothing, a commitment cut short.
    let tx = with_witness(&deposit, action_witness(vec![2u8; 33]));
    assert_rejected(env.verify(&tx), Error::ActionEncoding);
    let tx = with_witness(&deposit, action_witness(vec![action::TAG_DEPOSIT]));
    assert_rejected(env.verify(&tx), Error::ActionEncoding);
    let mut short = action::encode_deposit(&params.commitments);
    short.pop();
    let tx = with_witness(&deposit, action_witness(short));
    assert_rejected(env.verify(&tx), Error::ActionEncoding);

    // A withdrawal cut short inside the proof.
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let params = pool.withdraw_params(&env, &proof);
    let withdrawal = pool.withdraw_tx(&mut env, &params);
    let mut short = action::encode_withdraw(&action::Withdraw {
        fee: params.fee,
        root: params.root,
        nullifier_hash: params.nullifier_hash,
        proof: &params.proof,
        nullifier_proof: &[],
    });
    short.pop();
    let tx = with_witness(&withdrawal, action_witness(short));
    assert_rejected(env.verify(&tx), Error::ActionEncoding);
}

// ------------------------------------------------------------ pool life cycle

#[test]
fn a_pool_starts_empty_and_under_the_pool_lock() {
    let mut env = Env::new();
    let (type_script, lock_script) = env.pool_scripts(DENOMINATION);

    // A pool born with a root in its history: whoever knows a note under that
    // root could withdraw other people's deposits later.
    let mut preloaded = PoolState::empty();
    preloaded.roots[1] = fr_to_le_bytes(&Note::from_seed(666).commitment());
    let tx = env.create_pool_tx(&type_script, &lock_script, preloaded.to_bytes());
    assert_rejected(env.verify(&tx), Error::InitialState);

    // A pool that somebody holds the key to.
    let owner = env.user_lock(0xee);
    let tx = env.create_pool_tx(&type_script, &owner, PoolState::empty().to_bytes());
    assert_rejected(env.verify(&tx), Error::PoolLock);

    // The pool lock, but bound to another pool type.
    let (_, other_lock) = env.pool_scripts(2 * DENOMINATION);
    let tx = env.create_pool_tx(&type_script, &other_lock, PoolState::empty().to_bytes());
    assert_rejected(env.verify(&tx), Error::PoolLock);

    let tx = env.create_pool_tx(&type_script, &lock_script, PoolState::empty().to_bytes());
    env.verify(&tx).expect("an empty pool under the pool lock is accepted");
}

#[test]
fn the_pool_cell_keeps_the_pool_lock() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let thief = env.user_lock(0xee);

    // A deposit that moves the pool cell under the thief's lock.
    let params = pool.deposit_params(&[Note::from_seed(10).commitment()]);
    let honest = pool.deposit_tx(&mut env, &params);
    let stolen_pool = pool.pool_output(&thief, params.pool_capacity);
    let mut outputs: Vec<CellOutput> = honest.outputs().into_iter().collect();
    outputs[0] = stolen_pool;
    let tx = honest.as_advanced_builder().set_outputs(outputs).build();
    let tx = env.context.complete_tx(tx);
    assert_rejected(env.verify(&tx), Error::PoolLock);

    // The same through a withdrawal.
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let mut params = pool.withdraw_params(&env, &proof);
    params.pool_lock = thief;
    let tx = pool.withdraw_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::PoolLock);
}

#[test]
fn the_pool_cell_cannot_be_destroyed_split_or_merged() {
    let (mut env, pool, _) = pool_with_notes(2);
    let thief = env.user_lock(0xee);
    let action = action_witness(action::encode_deposit(&[fr_to_le_bytes(&Note::from_seed(10).commitment())]));

    // Spend the pool cell and keep everything in it.
    let tx = TransactionBuilder::default()
        .input(CellInput::new(pool.cell.clone(), 0))
        .output(plain_cell(&thief, pool.capacity - TX_FEE))
        .output_data(Bytes::new().pack())
        .witness(action.clone().pack())
        .build();
    let tx = env.context.complete_tx(tx);
    assert_rejected(env.verify(&tx), Error::GroupShape);

    // Split it in two, the second half for the thief to play with.
    let half = pool.capacity / 2;
    let tx = TransactionBuilder::default()
        .input(CellInput::new(pool.cell.clone(), 0))
        .output(pool.pool_output(&pool.lock_script, half))
        .output_data(Bytes::from(pool.state.to_bytes()).pack())
        .output(pool.pool_output(&pool.lock_script, pool.capacity - half - TX_FEE))
        .output_data(Bytes::from(pool.state.to_bytes()).pack())
        .witness(action.clone().pack())
        .build();
    let tx = env.context.complete_tx(tx);
    assert_rejected(env.verify(&tx), Error::GroupShape);

    // Merge two pool cells of the same type into one that remembers only the
    // deposits of the first.
    let (other, _) = Pool::create(&mut env, DENOMINATION);
    assert_eq!(other.type_script, pool.type_script);
    let tx = TransactionBuilder::default()
        .input(CellInput::new(pool.cell.clone(), 0))
        .input(CellInput::new(other.cell.clone(), 0))
        .output(pool.pool_output(&pool.lock_script, pool.capacity))
        .output_data(Bytes::from(pool.state.to_bytes()).pack())
        .output(plain_cell(&thief, other.capacity - TX_FEE))
        .output_data(Bytes::new().pack())
        .witness(action.pack())
        .build();
    let tx = env.context.complete_tx(tx);
    assert_rejected(env.verify(&tx), Error::GroupShape);
}

#[test]
fn the_pool_lock_only_unlocks_pool_cells() {
    let (mut env, pool, _) = pool_with_notes(1);
    let thief = env.user_lock(0xee);

    // The pool lock is not "anyone can spend": without the pool type script
    // next to it, it never unlocks.
    let bare = env.fund(&pool.lock_script, 500 * CKB);
    let tx = TransactionBuilder::default()
        .input(CellInput::new(bare, 0))
        .output(plain_cell(&thief, 500 * CKB - TX_FEE))
        .output_data(Bytes::new().pack())
        .build();
    let tx = env.context.complete_tx(tx);
    assert_exit_code(env.verify(&tx), POOL_LOCK_NOT_A_POOL_CELL);

    // Nor with some other type script.
    let other_type = env.user_lock(0x77);
    let typed = env.context.create_cell(
        plain_cell(&pool.lock_script, 500 * CKB)
            .as_builder()
            .type_(Some(other_type).pack())
            .build(),
        Bytes::new(),
    );
    let tx = TransactionBuilder::default()
        .input(CellInput::new(typed, 0))
        .output(plain_cell(&thief, 500 * CKB - TX_FEE))
        .output_data(Bytes::new().pack())
        .build();
    let tx = env.context.complete_tx(tx);
    assert_exit_code(env.verify(&tx), POOL_LOCK_NOT_A_POOL_CELL);
}

// ----------------------------------------------------------------- two pools

/// A transaction that withdraws from two pools at once. `outputs` lists, in
/// order, the cells the transaction creates.
fn two_pool_tx(
    env: &mut Env,
    pools: [(&Pool, &WithdrawParams); 2],
    outputs: Vec<(CellOutput, Bytes)>,
) -> TransactionView {
    let mut builder = TransactionBuilder::default().cell_dep(env.vk_dep());
    for (pool, params) in pools {
        builder = builder.input(CellInput::new(pool.cell.clone(), 0)).witness(params.witness().pack());
    }
    for (cell, data) in outputs {
        builder = builder.output(cell).output_data(data.pack());
    }
    env.context.complete_tx(builder.build())
}

#[test]
fn one_payout_cannot_satisfy_two_pools() {
    // Two pools with the same denomination and the same circuit, but different
    // type scripts.
    let mut env = Env::new();
    let (mut pool_a, _) = Pool::create(&mut env, DENOMINATION);
    let (mut pool_b, _) = Pool::create_with(&mut env, DENOMINATION, ScriptHashType::Data1);
    assert_ne!(pool_a.type_script, pool_b.type_script);

    let (note_a, note_b) = (Note::from_seed(1), Note::from_seed(2));
    pool_a.deposit(&mut env, &[note_a.commitment()]);
    pool_b.deposit(&mut env, &[note_b.commitment()]);

    // One user withdraws from both, to the same address, with the same fee, and
    // hands both proofs to one relayer.
    let recipient = env.user_lock(0xaa);
    let thief = env.user_lock(0xee);
    let proof_a = pool_a.prove_withdraw(&env, &note_a, 0, &recipient, FEE);
    let proof_b = pool_b.prove_withdraw(&env, &note_b, 0, &recipient, FEE);
    let params_a = pool_a.withdraw_params(&env, &proof_a);
    let params_b = pool_b.withdraw_params(&env, &proof_b);

    let pool_cell = |pool: &Pool, params: &WithdrawParams| {
        (pool.pool_output(&params.pool_lock, params.pool_capacity), Bytes::from(params.pool_data.clone()))
    };
    let payout = (plain_cell(&recipient, DENOMINATION - FEE), Bytes::new());
    let loot = (plain_cell(&thief, DENOMINATION - FEE), Bytes::new());

    // The relayer pays the user once and keeps the second payout. Each pool
    // looks for its payout in the output right after its own cell, so the two
    // cannot both point at the same one.
    let tx = two_pool_tx(
        &mut env,
        [(&pool_a, &params_a), (&pool_b, &params_b)],
        vec![pool_cell(&pool_a, &params_a), pool_cell(&pool_b, &params_b), payout.clone(), loot.clone()],
    );
    assert_rejected(env.verify(&tx), Error::RecipientShape);

    let tx = two_pool_tx(
        &mut env,
        [(&pool_a, &params_a), (&pool_b, &params_b)],
        vec![pool_cell(&pool_a, &params_a), payout.clone(), pool_cell(&pool_b, &params_b), loot],
    );
    assert_rejected(env.verify(&tx), Error::ProofRejected);

    let tx = two_pool_tx(
        &mut env,
        [(&pool_a, &params_a), (&pool_b, &params_b)],
        vec![pool_cell(&pool_a, &params_a), payout.clone(), pool_cell(&pool_b, &params_b)],
    );
    assert_rejected(env.verify(&tx), Error::RecipientMissing);

    // Paid twice, the same transaction is fine.
    let tx = two_pool_tx(
        &mut env,
        [(&pool_a, &params_a), (&pool_b, &params_b)],
        vec![pool_cell(&pool_a, &params_a), payout.clone(), pool_cell(&pool_b, &params_b), payout],
    );
    env.verify(&tx).expect("each pool pays its own output");
}
