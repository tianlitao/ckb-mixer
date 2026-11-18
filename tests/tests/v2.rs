//! Pool v2 against the real contract binaries in CKB-VM: intent deposits and
//! batches.
//!
//! The first group runs the honest flows. Every other test plays an attacker,
//! following the security analysis in `docs/intent-aggregation.md`: it starts
//! from the transaction an honest party would send, changes one thing, and
//! checks which script rejects it and why.
//!
//! ckb-testtool runs the scripts only. That a relative `since` is not yet
//! satisfied is consensus' business and is not exercised here; what is tested
//! is that the intent lock demands the `since` that makes consensus check it.

use ckb_testtool::ckb_types::{
    bytes::Bytes,
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{CellDep, CellInput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use mixer_core::{action, Error};
use mixer_core_v2::{self as v2, IntentArgs, IntentError, PoolState};
use mixer_tests::harness::{
    assert_rejected, assert_rejected_one_of, capacity_of, plain_cell, prove_note, script_hash, Env,
    Note, CKB, DENOMINATION, TX_FEE,
};
use mixer_tests::harness_v2::{
    action_witness, assert_intent_rejected, BatchParams, PoolSetup, PoolV2, RECLAIM_AFTER,
    ROOT_HISTORY, TIP,
};

/// Fee the withdrawals in these tests leave to the aggregator and the miner.
const FEE: u64 = CKB / 100;

fn setup() -> PoolSetup {
    PoolSetup::new(DENOMINATION)
}

/// A pool with `count` notes deposited through intents, in one batch.
fn pool_with_notes(count: u64) -> (Env, PoolV2, Vec<Note>) {
    let mut env = Env::new();
    let (mut pool, _) = PoolV2::create(&mut env, &setup());
    let notes: Vec<Note> = (0..count).map(Note::from_seed).collect();
    if !notes.is_empty() {
        pool.deposit_notes(&mut env, &notes);
    }
    (env, pool, notes)
}

fn with_witnesses(tx: &TransactionView, witnesses: Vec<Bytes>) -> TransactionView {
    tx.as_advanced_builder()
        .set_witnesses(witnesses.into_iter().map(|witness| witness.pack()).collect())
        .build()
}

/// A transaction that takes intent cells back: the intents with their `since`,
/// then `others`, everything paid to `to`.
fn reclaim_tx(env: &mut Env, intents: &[(OutPoint, u64)], others: &[OutPoint], to: &Script) -> TransactionView {
    let mut builder = TransactionBuilder::default();
    let mut total = 0;
    for (out_point, since) in intents {
        builder = builder.input(CellInput::new(out_point.clone(), *since));
        total += capacity_of(&env.context.get_cell(out_point).expect("live cell").0);
    }
    for out_point in others {
        builder = builder.input(CellInput::new(out_point.clone(), 0));
        total += capacity_of(&env.context.get_cell(out_point).expect("live cell").0);
    }
    let tx = builder
        .output(plain_cell(to, total - TX_FEE))
        .output_data(Bytes::new().pack())
        .build();
    env.context.complete_tx(tx)
}

/// A transaction that spends several pool cells. `pools` lists, per pool, its
/// batch and the position of its outputs: every pool's cell is followed by its
/// batch's outputs.
fn multi_pool_tx(env: &mut Env, pools: &[(&PoolV2, &BatchParams)]) -> TransactionView {
    let mut builder = TransactionBuilder::default();
    for (pool, _) in pools {
        builder = builder.input(CellInput::new(pool.cell.clone(), 0));
    }
    for (pool, params) in pools {
        builder = builder
            .inputs(params.inputs.clone())
            .witness(params.witness().pack())
            .output(pool.pool_output(&params.pool_lock, params.pool_capacity))
            .output_data(Bytes::from(params.pool_data.clone()).pack());
        for (cell, data) in &params.outputs {
            builder = builder.output(cell.clone()).output_data(data.pack());
        }
    }
    builder = builder.cell_dep(env.vk_dep());
    env.context.complete_tx(builder.build())
}

// ---------------------------------------------------------------- honest flows

#[test]
fn intents_are_deposited_and_withdrawn_in_batches() {
    let mut env = Env::new();
    let (mut pool, _) = PoolV2::create(&mut env, &setup());
    let empty_capacity = pool.capacity;
    assert_eq!(pool.state.to_bytes().len(), 680 + 32 * ROOT_HISTORY as usize);

    let notes = [Note::from_seed(1), Note::from_seed(2)];
    let owners = [env.user_lock(0xd1), env.user_lock(0xd2)];
    let intents: Vec<OutPoint> = notes
        .iter()
        .zip(&owners)
        .map(|(note, owner)| pool.place_intent(&mut env, &note.commitment(), owner))
        .collect();

    let (tx, _) = pool.settle(&mut env, &intents, &[]);
    // The depositors take no part in the transaction: none of its inputs is
    // under their locks, so nothing in it needed their signature.
    for out_point in tx.input_pts_iter() {
        let (cell, _) = env.context.get_cell(&out_point).expect("input");
        assert!(!owners.contains(&cell.lock()));
    }
    assert_eq!(pool.capacity, empty_capacity + 2 * DENOMINATION);
    // Two leaves, one new root.
    assert_eq!((pool.state.next_index, pool.state.root_pos), (2, 1));
    // The tips went to the aggregator's output, the last one.
    let collected = tx.outputs().into_iter().last().expect("aggregator output");
    assert_eq!(capacity_of(&collected), 100 * CKB - TX_FEE + 2 * TIP);

    let recipients = [env.user_lock(0xa1), env.user_lock(0xa2)];
    let proofs = [
        pool.prove_withdraw(&env, &notes[1], 1, &recipients[1], FEE),
        pool.prove_withdraw(&env, &notes[0], 0, &recipients[0], 2 * FEE),
    ];
    let (tx, _) = pool.settle(&mut env, &[], &proofs);
    assert_eq!(tx.output(1).expect("first recipient"), plain_cell(&recipients[1], DENOMINATION - FEE));
    assert_eq!(tx.output(2).expect("second recipient"), plain_cell(&recipients[0], DENOMINATION - 2 * FEE));
    assert_eq!(pool.capacity, empty_capacity);
    assert_eq!(pool.nullifiers.len(), 2);
    // Withdrawals alone do not use up a slot of the root history.
    assert_eq!(pool.state.root_pos, 1);
}

#[test]
fn one_batch_deposits_and_withdraws() {
    let (mut env, mut pool, notes) = pool_with_notes(2);
    let capacity = pool.capacity;
    let root_before = *pool.state.current_root();

    let owner = env.user_lock(0xd1);
    let fresh = [Note::from_seed(10), Note::from_seed(11), Note::from_seed(12)];
    let intents: Vec<OutPoint> = fresh
        .iter()
        .map(|note| pool.place_intent(&mut env, &note.commitment(), &owner))
        .collect();
    let recipient = env.user_lock(0xaa);
    let proofs = [
        pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE),
        pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE),
    ];
    assert_eq!(proofs[0].root, root_before);

    pool.settle(&mut env, &intents, &proofs);
    assert_eq!(pool.capacity, capacity + DENOMINATION);
    assert_eq!(pool.state.next_index, 5);
    assert_ne!(*pool.state.current_root(), root_before);

    // The notes deposited by that batch are withdrawable from the next one on.
    let proof = pool.prove_withdraw(&env, &fresh[2], 4, &recipient, FEE);
    pool.settle(&mut env, &[], &[proof]);
    assert_eq!(pool.capacity, capacity);
}

#[test]
fn anyone_can_settle_and_no_extra_input_is_needed() {
    let (mut env, mut pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);

    // A withdrawal on its own, as in v1: the pool cell is the only input and
    // the fee goes to the miner. This is what a user falls back to when no
    // aggregator serves them.
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let mut params = pool.batch_params(&mut env, &[], &[proof]);
    params.inputs.clear();
    params.outputs.truncate(1);
    let tx = pool.batch_tx(&mut env, &params);
    assert_eq!(tx.inputs().len(), 1);
    env.verify(&tx).expect("a withdrawal needs no other input");
    pool.apply_batch(&mut env, &tx, &params);

    // An intent filled with nothing but the pool cell: the tip pays the miner.
    let owner = env.user_lock(0xd1);
    let intent = pool.place_intent(&mut env, &Note::from_seed(7).commitment(), &owner);
    let mut params = pool.batch_params(&mut env, &[intent], &[]);
    params.inputs.truncate(1);
    params.outputs.clear();
    let tx = pool.batch_tx(&mut env, &params);
    assert_eq!(tx.inputs().len(), 2);
    env.verify(&tx).expect("an intent needs no other input");
    pool.apply_batch(&mut env, &tx, &params);
}

#[test]
fn the_root_history_is_as_long_as_the_args_say() {
    let mut env = Env::new();
    let mut short = setup();
    short.root_history = 3;
    let (mut pool, _) = PoolV2::create(&mut env, &short);
    assert_eq!(pool.state.to_bytes().len(), 680 + 32 * 3);

    let note = Note::from_seed(0);
    pool.deposit_notes(&mut env, &[note]);
    let recipient = env.user_lock(0xaa);
    let proof = pool.prove_withdraw(&env, &note, 0, &recipient, FEE);

    // Two more batches with deposits, however many: the proof's root is the
    // oldest of the three kept.
    pool.deposit_notes(&mut env, &[Note::from_seed(1), Note::from_seed(2), Note::from_seed(3)]);
    pool.deposit_notes(&mut env, &[Note::from_seed(4)]);
    assert_eq!((pool.state.next_index, pool.state.root_pos), (5, 0));
    let params = pool.batch_params(&mut env, &[], &[proof.clone()]);
    let tx = pool.batch_tx(&mut env, &params);
    env.verify(&tx).expect("the root is still in the history");

    // One more and it is gone.
    pool.deposit_notes(&mut env, &[Note::from_seed(5)]);
    let params = pool.batch_params(&mut env, &[], &[proof]);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::UnknownRoot);

    // A fresh proof works again.
    let proof = pool.prove_withdraw(&env, &note, 0, &recipient, FEE);
    pool.settle(&mut env, &[], &[proof]);
}

#[test]
fn a_withdrawal_cannot_use_the_root_its_own_batch_creates() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let recipient = env.user_lock(0xaa);
    let fresh = Note::from_seed(9);
    let intent = pool.place_intent(&mut env, &fresh.commitment(), &owner);

    // A proof for the tree as it will be after this very batch.
    let future_leaves = [notes[0].commitment(), fresh.commitment()];
    let proof = prove_note(&env.vk, &future_leaves, &fresh, 1, &recipient, FEE);
    let params = pool.batch_params(&mut env, &[intent], &[proof]);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::UnknownRoot);
}

// --------------------------------------------------------------------- intents

#[test]
fn an_intent_is_spent_only_by_depositing_it() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let thief = env.user_lock(0xee);
    let intent = pool.place_intent(&mut env, &Note::from_seed(50).commitment(), &owner);

    // On its own, with or without a `since`.
    for since in [0, v2::reclaim_since(RECLAIM_AFTER)] {
        let tx = reclaim_tx(&mut env, &[(intent.clone(), since)], &[], &thief);
        assert_intent_rejected(env.verify(&tx), IntentError::Unauthorized);
    }

    // Next to the pool, but with the pool left as if there were no deposit:
    // not in the tree, not in the capacity, or neither.
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &thief, FEE);
    let honest = pool.batch_params(&mut env, &[intent.clone()], &[proof.clone()]);
    let uncounted = pool.batch_params(&mut env, &[], &[proof]);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest batch is accepted");

    let mut params = honest.clone();
    params.pool_data = uncounted.pool_data.clone();
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    let mut params = honest.clone();
    params.pool_capacity = uncounted.pool_capacity;
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputCapacity);

    let mut params = honest.clone();
    params.pool_data = uncounted.pool_data.clone();
    params.pool_capacity = uncounted.pool_capacity;
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // The same without any withdrawal: the pool cell recreated unchanged.
    let mut params = pool.batch_params(&mut env, &[intent], &[]);
    params.pool_data = pool.state.to_bytes();
    params.pool_capacity = pool.capacity;
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);
}

#[test]
fn another_pool_is_no_cover_for_taking_an_intent() {
    let mut env = Env::new();
    let (pool_a, _) = PoolV2::create(&mut env, &setup());
    let (pool_b, _) = PoolV2::create(&mut env, &setup());
    // Same parameters, same code: only the Type ID tells them apart.
    assert_ne!(pool_a.type_hash(), pool_b.type_hash());
    assert_eq!(pool_a.config.to_bytes()[32..], pool_b.config.to_bytes()[32..]);

    let owner = env.user_lock(0xd1);
    let for_a = pool_a.place_intent(&mut env, &Note::from_seed(1).commitment(), &owner);
    let for_b = pool_b.place_intent(&mut env, &Note::from_seed(2).commitment(), &owner);

    // An honest batch of pool B that drags pool A's intent along. Pool B does
    // not count it, and without pool A among the inputs its lock stays shut.
    let mut params = pool_b.batch_params(&mut env, &[for_b], &[]);
    params.inputs.push(CellInput::new(for_a, 0));
    let tx = pool_b.batch_tx(&mut env, &params);
    assert_intent_rejected(env.verify(&tx), IntentError::Unauthorized);
}

#[test]
fn intents_are_counted_by_their_own_pool() {
    let mut env = Env::new();
    let (pool_a, _) = PoolV2::create(&mut env, &setup());
    let (pool_b, _) = PoolV2::create(&mut env, &setup());
    let owner = env.user_lock(0xd1);
    let for_a = pool_a.place_intent(&mut env, &Note::from_seed(1).commitment(), &owner);
    let for_b = pool_b.place_intent(&mut env, &Note::from_seed(2).commitment(), &owner);

    // Both pools in one transaction, each taking its own intent.
    let batch_a = pool_a.batch_params(&mut env, &[for_a.clone()], &[]);
    let batch_b = pool_b.batch_params(&mut env, &[for_b.clone()], &[]);
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &batch_a), (&pool_b, &batch_b)]);
    env.verify(&tx).expect("two pools settle side by side");

    // Each pool credited with the other's intent.
    let swapped_a = pool_a.batch_params(&mut env, &[for_b.clone()], &[]);
    let swapped_b = pool_b.batch_params(&mut env, &[for_a.clone()], &[]);
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &swapped_a), (&pool_b, &swapped_b)]);
    assert_rejected(env.verify(&tx), Error::OutputState);

    // Both intents credited to pool B, pool A recreated unchanged.
    let mut idle_a = pool_a.batch_params(&mut env, &[], &[]);
    idle_a.inputs.clear();
    idle_a.outputs.clear();
    let both_b = pool_b.batch_params(&mut env, &[for_a, for_b], &[]);
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &idle_a), (&pool_b, &both_b)]);
    assert_rejected(env.verify(&tx), Error::OutputState);
}

#[test]
fn two_intents_with_one_commitment_are_two_deposits() {
    let (mut env, mut pool, _) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let note = Note::from_seed(50);
    // Same commitment, same owner: one lock script, one script group, two cells.
    let intents = [
        pool.place_intent(&mut env, &note.commitment(), &owner),
        pool.place_intent(&mut env, &note.commitment(), &owner),
    ];
    let honest = pool.batch_params(&mut env, &intents, &[]);
    let single = pool.batch_params(&mut env, &intents[..1], &[]);

    // One leaf and one denomination for the two of them.
    let mut params = honest.clone();
    params.pool_data = single.pool_data.clone();
    params.pool_capacity = single.pool_capacity;
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputState);

    let mut params = honest.clone();
    params.pool_capacity = single.pool_capacity;
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::OutputCapacity);

    let capacity = pool.capacity;
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("both are deposited");
    pool.apply_batch(&mut env, &tx, &honest);
    assert_eq!(pool.state.next_index, 3);
    assert_eq!(pool.leaves[1], pool.leaves[2]);
    assert_eq!(pool.capacity, capacity + 2 * DENOMINATION);
}

/// The pool recognises intents by the lock's code hash *and* hash type. The
/// same lock code under another hash type is invisible to it, so if such a
/// cell opened next to the pool it would be spent without being deposited.
#[test]
fn an_intent_under_a_hash_type_the_pool_does_not_count_cannot_be_taken() {
    let (mut env, pool, _) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let owner_cell = env.fund(&owner, 100 * CKB);
    let configured: u8 = ScriptHashType::Data2.into();
    assert_eq!(pool.config.intent_lock.hash_type, configured);

    for (seed, hash_type) in [(50, ScriptHashType::Type), (51, ScriptHashType::Data1)] {
        let args = pool.intent_args(&Note::from_seed(seed).commitment(), &owner);
        let lock = env.intent_lock_with(hash_type, args.to_bytes());
        let stray = env.fund(&lock, DENOMINATION + TIP);

        // An otherwise honest batch that takes the stray cell along and keeps
        // what it holds.
        let counted = pool.place_intent(&mut env, &Note::from_seed(seed + 100).commitment(), &owner);
        let mut params = pool.batch_params(&mut env, &[counted], &[]);
        params.inputs.insert(0, CellInput::new(stray.clone(), 0));
        let (aggregator, _) = params.outputs.pop().expect("aggregator output");
        let loot = capacity_of(&aggregator) + DENOMINATION + TIP;
        params.outputs.push((aggregator.as_builder().capacity(loot).build(), Bytes::new()));
        let tx = pool.batch_tx(&mut env, &params);
        assert_intent_rejected(env.verify(&tx), IntentError::PoolMismatch);

        // Its owner can still take it back.
        let since = v2::reclaim_since(RECLAIM_AFTER);
        let tx = reclaim_tx(&mut env, &[(stray, since)], &[owner_cell.clone()], &owner);
        env.verify(&tx).expect("the owner reclaims the stray cell");
    }
}

#[test]
fn a_pool_may_name_its_intent_lock_by_type_hash() {
    let mut env = Env::new();
    let mut by_type = setup();
    by_type.intent_lock_hash_type = ScriptHashType::Type;
    let (mut pool, _) = PoolV2::create(&mut env, &by_type);
    let owner = env.user_lock(0xd1);

    // Now the data-hash form is the one that is not counted.
    let args = pool.intent_args(&Note::from_seed(1).commitment(), &owner);
    let lock = env.intent_lock_with(ScriptHashType::Data2, args.to_bytes());
    let stray = env.fund(&lock, DENOMINATION + TIP);
    let counted = pool.place_intent(&mut env, &Note::from_seed(2).commitment(), &owner);
    let mut params = pool.batch_params(&mut env, &[counted.clone()], &[]);
    params.inputs.insert(0, CellInput::new(stray, 0));
    let tx = pool.batch_tx(&mut env, &params);
    assert_intent_rejected(env.verify(&tx), IntentError::PoolMismatch);

    pool.settle(&mut env, &[counted], &[]);
    assert_eq!(pool.state.next_index, 1);
}

#[test]
fn only_the_owner_takes_an_intent_back_and_only_after_the_wait() {
    let mut env = Env::new();
    let (pool, _) = PoolV2::create(&mut env, &setup());
    let owner = env.user_lock(0xd1);
    let stranger = env.user_lock(0xee);
    let commitment = Note::from_seed(1).commitment();
    let intent = pool.place_intent(&mut env, &commitment, &owner);
    let owner_cell = env.fund(&owner, 100 * CKB);
    let stranger_cell = env.fund(&stranger, 100 * CKB);
    let waited = v2::reclaim_since(RECLAIM_AFTER);

    for since in [waited, v2::reclaim_since(RECLAIM_AFTER + 1_000)] {
        let tx = reclaim_tx(&mut env, &[(intent.clone(), since)], &[owner_cell.clone()], &owner);
        env.verify(&tx).expect("the owner reclaims after the wait");
    }

    // Somebody else's cell is not the owner's.
    let tx = reclaim_tx(&mut env, &[(intent.clone(), waited)], &[stranger_cell], &stranger);
    assert_intent_rejected(env.verify(&tx), IntentError::Unauthorized);

    // The owner, but without a `since` that makes the chain enforce the wait:
    // none, too short, absolute, in epochs, in seconds.
    let wrong = [
        0,
        v2::reclaim_since(RECLAIM_AFTER - 1),
        RECLAIM_AFTER,
        0x00ff_ffff_ffff_ffff,
        0xa000_0000_0000_0000 | 1_000_000,
        0xc000_0000_0000_0000 | 1_000_000,
    ];
    for since in wrong {
        let tx = reclaim_tx(&mut env, &[(intent.clone(), since)], &[owner_cell.clone()], &owner);
        assert_intent_rejected(env.verify(&tx), IntentError::ReclaimTooEarly);
    }

    // Two intents under the same lock: each input needs the `since`.
    let second = pool.place_intent(&mut env, &commitment, &owner);
    let tx = reclaim_tx(&mut env, &[(intent.clone(), waited), (second.clone(), 0)], &[owner_cell.clone()], &owner);
    assert_intent_rejected(env.verify(&tx), IntentError::ReclaimTooEarly);
    let tx = reclaim_tx(&mut env, &[(intent, waited), (second, waited)], &[owner_cell], &owner);
    env.verify(&tx).expect("both are reclaimed");
}

#[test]
fn an_intent_with_a_type_script_or_data_can_only_be_taken_back() {
    let (mut env, pool, _) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let owner_cell = env.fund(&owner, 100 * CKB);
    let some_type = env.user_lock(0x77);

    let args = pool.intent_args(&Note::from_seed(50).commitment(), &owner);
    let lock = pool.intent_lock(&mut env, &args);
    let plain = plain_cell(&lock, DENOMINATION + TIP);
    let with_data = env.context.create_cell(plain.clone(), Bytes::from_static(&[1, 2, 3, 4]));
    let with_type = env
        .context
        .create_cell(plain.as_builder().type_(Some(some_type).pack()).build(), Bytes::new());

    for odd in [with_data, with_type] {
        let params = pool.batch_params(&mut env, &[odd.clone()], &[]);
        let tx = pool.batch_tx(&mut env, &params);
        assert_intent_rejected(env.verify(&tx), IntentError::IntentShape);

        let since = v2::reclaim_since(RECLAIM_AFTER);
        let tx = reclaim_tx(&mut env, &[(odd, since)], &[owner_cell.clone()], &owner);
        env.verify(&tx).expect("the owner takes it back");
    }
}

#[test]
fn an_intent_with_a_bad_commitment_is_not_deposited() {
    let (mut env, pool, _) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let owner_cell = env.fund(&owner, 100 * CKB);
    let good = pool.place_intent(&mut env, &Note::from_seed(50).commitment(), &owner);

    // Not a canonical field element.
    let args = IntentArgs {
        pool_type_hash: pool.type_hash(),
        commitment: [0xff; 32],
        owner_lock_hash: script_hash(&owner),
        reclaim_after: RECLAIM_AFTER,
    };
    let lock = pool.intent_lock(&mut env, &args);
    let bad = env.fund(&lock, DENOMINATION + TIP);
    let params = pool.batch_params(&mut env, &[good, bad.clone()], &[]);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::NonCanonicalCommitment);

    let since = v2::reclaim_since(RECLAIM_AFTER);
    let tx = reclaim_tx(&mut env, &[(bad, since)], &[owner_cell.clone()], &owner);
    env.verify(&tx).expect("the owner takes it back");

    // Lock args of the wrong length name no owner: the pool does not count the
    // cell and the lock never opens.
    let short = env.intent_lock_with(ScriptHashType::Data2, args.to_bytes()[..103].to_vec());
    let stuck = env.fund(&short, DENOMINATION + TIP);
    let tx = reclaim_tx(&mut env, &[(stuck, since)], &[owner_cell], &owner);
    assert_intent_rejected(env.verify(&tx), IntentError::ScriptArgs);
}

// ----------------------------------------------------------- batch withdrawals

/// A pool with two notes and a proof for each, to two different recipients.
fn two_withdrawals() -> (Env, PoolV2, BatchParams, [Script; 2]) {
    let (mut env, pool, notes) = pool_with_notes(2);
    let recipients = [env.user_lock(0xa1), env.user_lock(0xa2)];
    let proofs = [
        pool.prove_withdraw(&env, &notes[0], 0, &recipients[0], FEE),
        pool.prove_withdraw(&env, &notes[1], 1, &recipients[1], FEE),
    ];
    let params = pool.batch_params(&mut env, &[], &proofs);
    let tx = pool.batch_tx(&mut env, &params);
    env.verify(&tx).expect("the honest batch is accepted");
    (env, pool, params, recipients)
}

#[test]
fn each_withdrawal_needs_its_own_recipient_output() {
    let (mut env, pool, notes) = pool_with_notes(2);
    let recipient = env.user_lock(0xaa);
    // Two notes, same recipient, same fee: the two payouts look alike.
    let proofs = [
        pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE),
        pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE),
    ];
    let honest = pool.batch_params(&mut env, &[], &proofs);
    assert_eq!(honest.outputs[0], honest.outputs[1]);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("two outputs for two withdrawals");

    // One recipient output and nothing after it.
    let mut params = honest.clone();
    params.outputs.truncate(1);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientMissing);

    // One recipient output followed by the aggregator's own: wrong amount,
    // and with the amount made to fit, a lock the proof was not made for.
    let mut params = honest.clone();
    params.outputs.remove(1);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::RecipientCapacity);

    let (aggregator, _) = params.outputs.pop().expect("aggregator output");
    params.outputs.push((aggregator.as_builder().capacity(DENOMINATION - FEE).build(), Bytes::new()));
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);
}

#[test]
fn recipients_follow_the_order_of_the_withdrawals() {
    let (mut env, pool, honest, _) = two_withdrawals();

    let mut params = honest.clone();
    params.outputs.swap(0, 1);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&tx), Error::ProofRejected);

    // Swapping the withdrawals along with their outputs is a different batch
    // with the same result. The nullifier-set proofs depend on the order, so
    // the aggregator has to redo them.
    let mut params = honest.clone();
    params.outputs.swap(0, 1);
    params.withdrawals.swap(0, 1);
    let tx = pool.batch_tx(&mut env, &params);
    assert_rejected_one_of(env.verify(&tx), &[Error::NullifierSpent, Error::NullifierProof]);

    let mut state = pool.state.clone();
    let mut nullifiers = pool.nullifiers.clone();
    for entry in &mut params.withdrawals {
        entry.nullifier_proof = nullifiers.proof(&entry.nullifier_hash);
        state.withdraw(&entry.root, &entry.nullifier_hash, &entry.nullifier_proof).expect("unspent");
        nullifiers.insert(entry.nullifier_hash);
    }
    assert_eq!(state.to_bytes(), honest.pool_data, "the order changes nothing in the outcome");
    let tx = pool.batch_tx(&mut env, &params);
    env.verify(&tx).expect("the reordered batch is accepted");
}

#[test]
fn a_note_is_withdrawn_once_per_batch_and_once_ever() {
    // Three notes, so the pool could pay for the double withdrawals tried below.
    let (mut env, mut pool, notes) = pool_with_notes(3);
    let recipient = env.user_lock(0xa1);
    let other = env.user_lock(0xa2);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let second_proof = pool.prove_withdraw(&env, &notes[0], 0, &other, FEE);

    // The same proof twice, and two proofs for the same note.
    for pair in [[proof.clone(), proof.clone()], [proof.clone(), second_proof.clone()]] {
        let honest = pool.batch_params(&mut env, &[], &pair);
        let tx = pool.batch_tx(&mut env, &honest);
        assert_rejected(env.verify(&tx), Error::NullifierSpent);

        // With the absence proof that was good for the first one, and with the
        // pool paying out twice.
        let mut params = honest.clone();
        params.withdrawals[1].nullifier_proof = params.withdrawals[0].nullifier_proof.clone();
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::NullifierSpent);
    }

    pool.settle(&mut env, &[], &[proof.clone()]);
    for replay in [proof, second_proof] {
        let other_note = pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE);
        let params = pool.batch_params(&mut env, &[], &[other_note, replay]);
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::NullifierSpent);
    }
}

#[test]
fn every_withdrawal_of_a_batch_binds_recipient_fee_and_payout() {
    let (mut env, pool, honest, _) = two_withdrawals();
    let thief = env.user_lock(0xee);
    let payout = DENOMINATION - FEE;
    let reject = |env: &mut Env, params: &BatchParams, reason: Error| {
        let tx = pool.batch_tx(env, params);
        assert_rejected(env.verify(&tx), reason);
    };

    for position in 0..2 {
        // Paid to somebody else.
        let mut params = honest.clone();
        params.outputs[position] = (plain_cell(&thief, payout), Bytes::new());
        reject(&mut env, &params, Error::ProofRejected);

        // A higher fee, declared and kept.
        let mut params = honest.clone();
        params.withdrawals[position].fee = 2 * FEE;
        params.outputs[position].0 = params.outputs[position].0.clone().as_builder().capacity(DENOMINATION - 2 * FEE).build();
        reject(&mut env, &params, Error::ProofRejected);

        // A higher fee kept without declaring it, and a shannon too much.
        for capacity in [payout - 1, payout + 1] {
            let mut params = honest.clone();
            params.outputs[position].0 = params.outputs[position].0.clone().as_builder().capacity(capacity).build();
            reject(&mut env, &params, Error::RecipientCapacity);
        }

        // A fee above the denomination.
        let mut params = honest.clone();
        params.withdrawals[position].fee = DENOMINATION + 1;
        reject(&mut env, &params, Error::FeeTooLarge);

        // A recipient cell that carries data or a type script.
        let mut params = honest.clone();
        params.outputs[position].1 = Bytes::from_static(&[1]);
        reject(&mut env, &params, Error::RecipientShape);
        let mut params = honest.clone();
        params.outputs[position].0 =
            params.outputs[position].0.clone().as_builder().type_(Some(thief.clone()).pack()).build();
        reject(&mut env, &params, Error::RecipientShape);

        // A tampered proof, and the other withdrawal's proof.
        let mut params = honest.clone();
        params.withdrawals[position].proof[40] ^= 1;
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected_one_of(env.verify(&tx), &[Error::InvalidProof, Error::ProofRejected]);
        let mut params = honest.clone();
        params.withdrawals[position].proof = honest.withdrawals[1 - position].proof.clone();
        reject(&mut env, &params, Error::ProofRejected);
    }

    let mut params = honest.clone();
    params.vk_dep = None;
    reject(&mut env, &params, Error::VkNotFound);
    let mut params = honest;
    params.vk_dep = Some(CellDep::new_builder().out_point(env.fund(&thief, 100 * CKB)).build());
    reject(&mut env, &params, Error::VkNotFound);
}

#[test]
fn one_output_cannot_be_the_payout_of_two_pools() {
    let mut env = Env::new();
    let (mut pool_a, _) = PoolV2::create(&mut env, &setup());
    let (mut pool_b, _) = PoolV2::create(&mut env, &setup());
    let note = Note::from_seed(1);
    pool_a.deposit_notes(&mut env, &[note]);
    pool_b.deposit_notes(&mut env, &[note]);

    // The same note, recipient and fee in both pools: the two payouts are the
    // same cell, byte for byte.
    let recipient = env.user_lock(0xaa);
    let proof_a = pool_a.prove_withdraw(&env, &note, 0, &recipient, FEE);
    let proof_b = pool_b.prove_withdraw(&env, &note, 0, &recipient, FEE);
    let mut batch_a = pool_a.batch_params(&mut env, &[], &[proof_a]);
    let mut batch_b = pool_b.batch_params(&mut env, &[], &[proof_b]);
    for batch in [&mut batch_a, &mut batch_b] {
        batch.inputs.clear();
        batch.outputs.truncate(1);
    }
    assert_eq!(batch_a.outputs, batch_b.outputs);
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &batch_a), (&pool_b, &batch_b)]);
    env.verify(&tx).expect("one payout per pool is accepted");

    // Outputs: pool A, the payout, pool B. Pool B has no output after it.
    let mut unpaid_b = batch_b.clone();
    unpaid_b.outputs.clear();
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &batch_a), (&pool_b, &unpaid_b)]);
    assert_rejected(env.verify(&tx), Error::RecipientMissing);

    // Outputs: pool A, pool B, the payout. Pool A's recipient is pool B's cell.
    let mut unpaid_a = batch_a.clone();
    unpaid_a.outputs.clear();
    let tx = multi_pool_tx(&mut env, &[(&pool_a, &unpaid_a), (&pool_b, &batch_b)]);
    assert_rejected(env.verify(&tx), Error::RecipientShape);
}

// ------------------------------------------------------------ state transition

#[test]
fn a_batch_moves_exactly_its_denominations() {
    let (mut env, pool, notes) = pool_with_notes(2);
    let owner = env.user_lock(0xd1);
    let recipient = env.user_lock(0xaa);
    let intent = pool.place_intent(&mut env, &Note::from_seed(9).commitment(), &owner);
    let proofs = [
        pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE),
        pool.prove_withdraw(&env, &notes[1], 1, &recipient, FEE),
    ];
    // One in, two out: the pool shrinks by one denomination.
    let honest = pool.batch_params(&mut env, &[intent], &proofs);
    assert_eq!(honest.pool_capacity, pool.capacity - DENOMINATION);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest batch is accepted");

    let wrong = [
        honest.pool_capacity - 1,
        honest.pool_capacity + 1,
        // The deposit not paid in, a withdrawal not paid out.
        pool.capacity - 2 * DENOMINATION,
        pool.capacity,
    ];
    let aggregator = env.user_lock(0xa9);
    for capacity in wrong {
        let mut params = honest.clone();
        params.pool_capacity = capacity;
        // Whatever the pool is short of or gets too much goes to, or comes
        // from, the aggregator, so that the transaction balances.
        let extra = 3 * DENOMINATION;
        params.inputs.push(CellInput::new(env.fund(&aggregator, extra), 0));
        let (collected, _) = params.outputs.pop().expect("aggregator output");
        let rest = capacity_of(&collected) + extra + honest.pool_capacity - capacity;
        params.outputs.push((collected.as_builder().capacity(rest).build(), Bytes::new()));
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::OutputCapacity);
    }
}

#[test]
fn a_batch_must_produce_exactly_the_next_state() {
    let (mut env, pool, notes) = pool_with_notes(2);
    let owner = env.user_lock(0xd1);
    let recipient = env.user_lock(0xaa);
    let intent = pool.place_intent(&mut env, &Note::from_seed(9).commitment(), &owner);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let honest = pool.batch_params(&mut env, &[intent], &[proof]);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest batch is accepted");

    let len = honest.pool_data.len();
    // next_index, root_pos, a used and an unused root slot, the frontier, the
    // nullifier root.
    let roots = 8;
    let frontier = roots + 32 * ROOT_HISTORY as usize;
    for offset in [0, 4, roots + 32 * 2, roots + 32 * 5, frontier, frontier + 32 * 19, len - 32, len - 1] {
        let mut params = honest.clone();
        params.pool_data[offset] ^= 1;
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::OutputState);
    }

    // The old state, and states of another length.
    let mut others = vec![pool.state.to_bytes(), honest.pool_data[..len - 32].to_vec()];
    let mut longer = honest.pool_data.clone();
    longer.extend_from_slice(&[0u8; 32]);
    others.push(longer);
    for data in others {
        let mut params = honest.clone();
        params.pool_data = data;
        let tx = pool.batch_tx(&mut env, &params);
        assert_rejected(env.verify(&tx), Error::OutputState);
    }
}

#[test]
fn malformed_and_empty_batches_are_rejected() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let owner = env.user_lock(0xd1);
    let recipient = env.user_lock(0xaa);
    let intent = pool.place_intent(&mut env, &Note::from_seed(9).commitment(), &owner);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let honest = pool.batch_params(&mut env, &[intent], &[proof]);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest batch is accepted");

    // No witness, or one without the action.
    assert_rejected(env.verify(&with_witnesses(&tx, vec![])), Error::Witness);
    let no_action = WitnessArgs::new_builder().lock(Some(Bytes::from_static(&[0; 65])).pack()).build();
    assert_rejected(env.verify(&with_witnesses(&tx, vec![no_action.as_bytes()])), Error::Witness);
    assert_rejected(env.verify(&with_witnesses(&tx, vec![Bytes::from_static(&[1, 2, 3])])), Error::Witness);

    let good = honest.action();
    let mut trailing = good.clone();
    trailing.push(0);
    let mut miscounted = good.clone();
    miscounted[1] = 2;
    let mut no_withdrawals = good.clone();
    no_withdrawals[1] = 0;
    let mut short_proof = good.clone();
    let len_at = 3 + 8 + 32 + 32 + 128;
    short_proof[len_at] -= 1;
    let v1_withdraw = [&[action::TAG_WITHDRAW][..], &good[3..len_at], &good[len_at + 2..]].concat();
    let malformed = [
        vec![],
        vec![v2::TAG_BATCH],
        good[..good.len() - 1].to_vec(),
        trailing,
        miscounted,
        no_withdrawals,
        short_proof,
        // v1's actions mean nothing to a v2 pool.
        action::encode_deposit(&honest.deposits),
        v1_withdraw,
    ];
    for bytes in malformed {
        let tampered = with_witnesses(&tx, vec![action_witness(bytes)]);
        assert_rejected(env.verify(&tampered), Error::ActionEncoding);
    }

    // A well-formed batch of nothing: no intents among the inputs, no
    // withdrawals in the witness.
    let empty = pool.batch_params(&mut env, &[], &[]);
    assert_eq!(empty.action(), [v2::TAG_BATCH, 0, 0]);
    assert_eq!((&empty.pool_data, empty.pool_capacity), (&pool.state.to_bytes(), pool.capacity));
    let tx = pool.batch_tx(&mut env, &empty);
    assert_rejected(env.verify(&tx), Error::EmptyBatch);
}

// --------------------------------------------------------------- the pool cell

#[test]
fn a_pool_is_created_once_empty_and_under_the_pool_lock() {
    let mut env = Env::new();
    let honest = env.v2_create_params(&setup());
    let tx = env.v2_create_tx(&honest);
    env.verify(&tx).expect("the honest creation is accepted");
    let first_input = CellInput::new(honest.funding.clone(), 0);

    // The same type script again, from another input: a second cell with the
    // same script hash, which intents could not tell from the first.
    let mut again = honest.clone();
    again.funding = env.v2_create_params(&setup()).funding;
    let tx = env.v2_create_tx(&again);
    assert_rejected(env.verify(&tx), Error::TypeId);

    // The pool cell at another output index than its Type ID says, then with
    // the Type ID that fits that index.
    let mut moved = honest.clone();
    moved.outputs_before = 1;
    let tx = env.v2_create_tx(&moved);
    assert_rejected(env.verify(&tx), Error::TypeId);
    moved.config.type_id = v2::type_id(first_input.as_slice(), 1);
    let tx = env.v2_create_tx(&moved);
    env.verify(&tx).expect("a pool may be created at any output index");

    // Two pool cells in one transaction.
    let tx = env.v2_create_tx(&honest);
    let twice = tx
        .as_advanced_builder()
        .output(tx.output(0).expect("pool output"))
        .output_data(tx.outputs_data().get(0).expect("pool data"))
        .set_inputs(vec![first_input.clone(), CellInput::new(again.funding.clone(), 0)])
        .build();
    assert_rejected(env.verify(&twice), Error::GroupShape);

    // Not empty: a deposit nobody paid for, or a state of another history length.
    let mut seeded = PoolState::empty(ROOT_HISTORY);
    seeded.deposit(&[mixer_core::field::fr_to_le_bytes(&Note::from_seed(1).commitment())]).expect("deposit");
    for data in [seeded.to_bytes(), PoolState::empty(ROOT_HISTORY + 1).to_bytes(), vec![]] {
        let mut params = honest.clone();
        params.data = data;
        let tx = env.v2_create_tx(&params);
        assert_rejected(env.verify(&tx), Error::InitialState);
    }

    // Under a lock somebody holds, or the pool lock bound to something else.
    let (type_script, lock_script) = env.v2_scripts(honest.config.to_bytes(), honest.pool_lock_hash_type);
    let creator = env.user_lock(0xc0);
    let unbound = lock_script.clone().as_builder().args(Bytes::from_static(&[0u8; 32]).pack()).build();
    for lock in [creator, unbound] {
        let tx = env.v2_create_tx_with(&honest, &type_script, &lock);
        assert_rejected(env.verify(&tx), Error::PoolLock);
    }

    // Args that are no pool: v1's, no denomination, no root history.
    let mut free = honest.config.clone();
    free.denomination = 0;
    let mut forgetful = honest.config.clone();
    forgetful.root_history = 0;
    for args in [honest.config.to_bytes()[..73].to_vec(), free.to_bytes(), forgetful.to_bytes()] {
        let (type_script, lock_script) = env.v2_scripts(args, honest.pool_lock_hash_type);
        let tx = env.v2_create_tx_with(&honest, &type_script, &lock_script);
        assert_rejected(env.verify(&tx), Error::ScriptArgs);
    }
}

#[test]
fn the_pool_cell_keeps_its_lock_and_cannot_be_destroyed_or_split() {
    let (mut env, pool, notes) = pool_with_notes(1);
    let recipient = env.user_lock(0xaa);
    let thief = env.user_lock(0xee);
    let proof = pool.prove_withdraw(&env, &notes[0], 0, &recipient, FEE);
    let honest = pool.batch_params(&mut env, &[], &[proof]);
    let tx = pool.batch_tx(&mut env, &honest);
    env.verify(&tx).expect("the honest batch is accepted");

    let mut params = honest.clone();
    params.pool_lock = thief.clone();
    let stolen = pool.batch_tx(&mut env, &params);
    assert_rejected(env.verify(&stolen), Error::PoolLock);

    // The pool cell spent and not recreated.
    let destroyed = TransactionBuilder::default()
        .input(CellInput::new(pool.cell.clone(), 0))
        .output(plain_cell(&thief, pool.capacity - TX_FEE))
        .output_data(Bytes::new().pack())
        .witness(honest.witness().pack())
        .build();
    let destroyed = env.context.complete_tx(destroyed);
    assert_rejected(env.verify(&destroyed), Error::GroupShape);

    // A second cell with the pool's type script next to the real one.
    let sibling = tx
        .as_advanced_builder()
        .output(pool.pool_output(&pool.lock_script, 2_000 * CKB))
        .output_data(Bytes::from(PoolState::empty(ROOT_HISTORY).to_bytes()).pack())
        .input(CellInput::new(env.fund(&thief, 2_100 * CKB), 0))
        .build();
    assert_rejected(env.verify(&sibling), Error::GroupShape);
}
