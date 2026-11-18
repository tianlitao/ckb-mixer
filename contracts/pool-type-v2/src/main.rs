//! Type script of the mixer pool cell, v2.
//!
//! args (140 bytes), see `mixer-core-v2`:
//!   type_id               32      makes this pool's script hash name one cell
//!   denomination          u64 LE  shannons per deposit and per withdrawal
//!   vk_hash               32      data hash of the cell holding the verifying key
//!   pool lock             32 + 1  code hash and hash type
//!   intent lock           32 + 1  code hash and hash type
//!   root_history          u16 LE  number of recent roots kept
//!
//! A transaction may do one of two things with cells of this type:
//!
//! * create the pool cell, in the empty state, with `type_id` derived from the
//!   transaction's first input, so that it can be done only once;
//! * settle a batch: consume the pool cell and recreate it after `w`
//!   withdrawals and `d` deposits, `d + w >= 1`.
//!
//! Withdrawals are listed in the pool input's witness. The i-th one pays
//! `denomination - fee` to the i-th output after the pool cell, the recipient
//! named in its zero-knowledge proof.
//!
//! Deposits are not listed anywhere. They are the transaction's inputs that
//! carry the intent lock bound to this pool: each one adds the commitment in
//! its lock args to the tree, in input order. The intent lock in turn lets such
//! a cell be spent only when this script runs, so every intent that is spent is
//! counted, and the capacity rule below makes the pool keep its denomination.
//!
//! The pool cell's capacity changes by exactly `(d - w) × denomination`.
//! A pool cell can never be destroyed, and it carries the pool lock, which
//! lets anyone spend it as long as this script is attached.
//!
//! The state rules live in `mixer-core-v2`; this file binds them to the
//! transaction.

#![no_std]
#![no_main]


use ckb_std::{
    ckb_constants::Source,
    ckb_types::prelude::*,
    error::SysError,
    high_level::{
        load_cell_capacity, load_cell_data, load_cell_lock, load_cell_lock_hash,
        load_cell_type_hash, load_input, load_script, load_script_hash, load_witness_args,
        look_for_dep_with_data_hash, QueryIter,
    },
};
use mixer_core::{
    action::{self, Withdraw},
    Bytes32, Error,
};
use mixer_core_v2::{self as v2, IntentArgs, PoolConfig, PoolState};
use verifier_core::VerifyError;

ckb_std::entry!(program_entry);
ckb_std::default_alloc!();

use alloc::vec::Vec;

fn syscall(_: SysError) -> Error {
    Error::Syscall
}

/// With the `profile` feature, print the cycles consumed so far. The benchmark
/// build uses it to split a transaction's cost into stages; the normal build
/// compiles it to nothing.
#[cfg(feature = "profile")]
fn checkpoint(stage: &str) {
    ckb_std::syscalls::debug(alloc::format!(
        "profile {stage} {}",
        ckb_std::syscalls::current_cycles()
    ));
}

#[cfg(not(feature = "profile"))]
#[inline(always)]
fn checkpoint(_stage: &str) {}

fn program_entry() -> i8 {
    match run() {
        Ok(()) => 0,
        Err(error) => error.code(),
    }
}

struct Pool {
    config: PoolConfig,
    /// Hash of this type script: what the pool lock and the intents refer to.
    hash: Bytes32,
    /// Position of the output pool cell among the transaction's outputs.
    output_index: usize,
}

fn run() -> Result<(), Error> {
    checkpoint("start");
    let script = load_script().map_err(syscall)?;
    let config = PoolConfig::parse(&script.args().raw_data())?;
    let hash = load_script_hash().map_err(syscall)?;

    let inputs = QueryIter::new(load_cell_capacity, Source::GroupInput).count();
    let outputs = QueryIter::new(load_cell_capacity, Source::GroupOutput).count();
    if !matches!((inputs, outputs), (0, 1) | (1, 1)) {
        return Err(Error::GroupShape);
    }
    let output_index = QueryIter::new(load_cell_type_hash, Source::Output)
        .position(|type_hash| type_hash == Some(hash))
        .ok_or(Error::Syscall)?;
    let pool = Pool { config, hash, output_index };

    // Checked on every output, so every live pool cell has the pool lock.
    let lock = load_cell_lock(0, Source::GroupOutput).map_err(syscall)?;
    let is_pool_lock = lock.code_hash().as_slice() == pool.config.pool_lock.code_hash
        && lock.hash_type().as_slice() == [pool.config.pool_lock.hash_type]
        && lock.args().raw_data()[..] == pool.hash;
    if !is_pool_lock {
        return Err(Error::PoolLock);
    }

    if inputs == 0 {
        create(&pool)
    } else {
        settle(&pool)
    }
}

fn create(pool: &Pool) -> Result<(), Error> {
    let first_input = load_input(0, Source::Input).map_err(syscall)?;
    if v2::type_id(first_input.as_slice(), pool.output_index as u64) != pool.config.type_id {
        return Err(Error::TypeId);
    }
    let data = load_cell_data(0, Source::GroupOutput).map_err(syscall)?;
    if data != PoolState::empty(pool.config.root_history).to_bytes() {
        return Err(Error::InitialState);
    }
    Ok(())
}

fn settle(pool: &Pool) -> Result<(), Error> {
    let denomination = pool.config.denomination;
    let input_data = load_cell_data(0, Source::GroupInput).map_err(syscall)?;
    let mut state = PoolState::from_bytes(&input_data, pool.config.root_history)?;
    let input_capacity = load_cell_capacity(0, Source::GroupInput).map_err(syscall)?;

    let witness = load_witness_args(0, Source::GroupInput).map_err(|_| Error::Witness)?;
    let action_bytes = witness.input_type().to_opt().ok_or(Error::Witness)?.raw_data();
    let withdrawals = v2::decode_batch(&action_bytes).ok_or(Error::ActionEncoding)?;
    let deposits = intent_commitments(pool)?;
    checkpoint("loaded");

    // A transaction that changes nothing would still replace the pool cell and
    // so invalidate everything built on the old one, for the price of a fee.
    if withdrawals.is_empty() && deposits.is_empty() {
        return Err(Error::EmptyBatch);
    }

    // Withdrawals first: each proof refers to a root that existed before this
    // transaction, and to the nullifier set as the withdrawals before it left it.
    if !withdrawals.is_empty() {
        let vk_index =
            look_for_dep_with_data_hash(&pool.config.vk_hash).map_err(|_| Error::VkNotFound)?;
        let vk = load_cell_data(vk_index, Source::CellDep).map_err(syscall)?;
        for (position, withdraw) in withdrawals.iter().enumerate() {
            // Recipients follow the pool cell, one output each, in the order of
            // the withdrawals. Fixed positions mean no output can be counted as
            // the payout of two withdrawals, of this pool or of another.
            let recipient_index = pool.output_index + 1 + position;
            check_withdrawal(denomination, &vk, &mut state, withdraw, recipient_index)?;
        }
    }
    state.deposit(&deposits)?;
    checkpoint("tree");

    // The output must be exactly the state the batch produces: byte for byte,
    // so there is no room to smuggle in another root, index or nullifier set.
    let output_data = load_cell_data(0, Source::GroupOutput).map_err(syscall)?;
    if output_data != state.to_bytes() {
        return Err(Error::OutputState);
    }
    let paid_in = (deposits.len() as u64).checked_mul(denomination);
    let paid_out = (withdrawals.len() as u64).checked_mul(denomination);
    let expected_capacity = paid_in
        .and_then(|paid_in| input_capacity.checked_add(paid_in))
        .zip(paid_out)
        .and_then(|(capacity, paid_out)| capacity.checked_sub(paid_out))
        .ok_or(Error::OutputCapacity)?;
    if load_cell_capacity(0, Source::GroupOutput).map_err(syscall)? != expected_capacity {
        return Err(Error::OutputCapacity);
    }
    checkpoint("end");
    Ok(())
}

/// The commitments of the transaction's intent inputs bound to this pool, in
/// input order.
///
/// An input is an intent for this pool when its lock is the intent lock named
/// in the args, by code hash and by hash type, with this script's hash in the
/// lock args. The intent lock checks the same from its side, see its source.
fn intent_commitments(pool: &Pool) -> Result<Vec<Bytes32>, Error> {
    let intent_lock = &pool.config.intent_lock;
    let mut commitments = Vec::new();
    for lock in QueryIter::new(load_cell_lock, Source::Input) {
        if lock.code_hash().as_slice() != intent_lock.code_hash
            || lock.hash_type().as_slice() != [intent_lock.hash_type]
        {
            continue;
        }
        match IntentArgs::parse(&lock.args().raw_data()) {
            Some(intent) if intent.pool_type_hash == pool.hash => commitments.push(intent.commitment),
            _ => {}
        }
    }
    Ok(commitments)
}

fn check_withdrawal(
    denomination: u64,
    vk: &[u8],
    state: &mut PoolState,
    withdraw: &Withdraw<'_>,
    recipient_index: usize,
) -> Result<(), Error> {
    let payout = denomination.checked_sub(withdraw.fee).ok_or(Error::FeeTooLarge)?;

    // Known root, canonical and unspent nullifier; marks the nullifier spent.
    state.withdraw(&withdraw.root, &withdraw.nullifier_hash, withdraw.nullifier_proof)?;
    checkpoint("nullifier");

    let recipient_lock_hash =
        load_cell_lock_hash(recipient_index, Source::Output).map_err(|_| Error::RecipientMissing)?;
    // The transaction is not signed by the recipient and can be rebuilt by
    // whoever sees the proof. Requiring a plain cell stops them from attaching a
    // type script or data that would leave the recipient with an unspendable cell.
    let has_type = load_cell_type_hash(recipient_index, Source::Output)
        .map_err(syscall)?
        .is_some();
    let has_data = !load_cell_data(recipient_index, Source::Output)
        .map_err(syscall)?
        .is_empty();
    if has_type || has_data {
        return Err(Error::RecipientShape);
    }
    if load_cell_capacity(recipient_index, Source::Output).map_err(syscall)? != payout {
        return Err(Error::RecipientCapacity);
    }

    // The proof shows knowledge of a note in the tree with this root and this
    // nullifier hash, and it is bound to this recipient and this fee.
    let public_inputs = action::public_inputs(
        &withdraw.root,
        &withdraw.nullifier_hash,
        &recipient_lock_hash,
        withdraw.fee,
    );
    checkpoint("recipient");
    let verdict = verifier_core::verify(vk, withdraw.proof, &public_inputs);
    checkpoint("groth16");
    verdict.map_err(|error| match error {
        VerifyError::InvalidVk => Error::InvalidVk,
        VerifyError::InvalidProof => Error::InvalidProof,
        VerifyError::InvalidPublicInputs => Error::InvalidPublicInputs,
        VerifyError::PublicInputCountMismatch => Error::PublicInputCountMismatch,
        VerifyError::VerificationFailed => Error::ProofRejected,
    })
}
