//! Type script of the mixer pool cell.
//!
//! args (73 bytes):
//!   denomination   u64 LE   shannons per deposit and per withdrawal
//!   vk_hash        32       data hash of the cell holding the Groth16 verifying key
//!   lock_code_hash 32       code hash of the pool lock script
//!   lock_hash_type 1        hash type of the pool lock script
//!
//! A transaction may do exactly one of three things with cells of this type:
//!
//! * create one pool cell, in the empty state;
//! * deposit: consume the pool cell and recreate it with n more commitments in
//!   the tree and n × denomination more capacity;
//! * withdraw: consume the pool cell and recreate it with one more spent
//!   nullifier and one denomination less capacity, paying `denomination - fee`
//!   to the recipient named in a zero-knowledge proof.
//!
//! A pool cell can never be destroyed. Every pool cell carries the pool lock,
//! which lets anyone spend it as long as this script is attached, so nobody
//! holds a key to the pool and nobody can take it hostage.
//!
//! The state rules live in `mixer-core`; this file binds them to the
//! transaction.

#![no_std]
#![no_main]

use ckb_std::{
    ckb_constants::Source,
    ckb_types::prelude::*,
    error::SysError,
    high_level::{
        load_cell_capacity, load_cell_data, load_cell_lock, load_cell_lock_hash,
        load_cell_type_hash, load_script, load_script_hash, load_witness_args,
        look_for_dep_with_data_hash, QueryIter,
    },
};
use mixer_core::{
    action::{self, Action, Withdraw},
    state::PoolState,
    Error,
};
use verifier_core::VerifyError;

ckb_std::entry!(program_entry);
ckb_std::default_alloc!();

const ARGS_LEN: usize = 8 + 32 + 32 + 1;

struct Config {
    denomination: u64,
    vk_hash: [u8; 32],
    lock_code_hash: [u8; 32],
    lock_hash_type: u8,
}

impl Config {
    fn parse(args: &[u8]) -> Result<Self, Error> {
        if args.len() != ARGS_LEN {
            return Err(Error::ScriptArgs);
        }
        let denomination = u64::from_le_bytes(args[..8].try_into().map_err(|_| Error::ScriptArgs)?);
        if denomination == 0 {
            return Err(Error::ScriptArgs);
        }
        Ok(Self {
            denomination,
            vk_hash: args[8..40].try_into().map_err(|_| Error::ScriptArgs)?,
            lock_code_hash: args[40..72].try_into().map_err(|_| Error::ScriptArgs)?,
            lock_hash_type: args[72],
        })
    }
}

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

fn run() -> Result<(), Error> {
    checkpoint("start");
    let script = load_script().map_err(syscall)?;
    let config = Config::parse(&script.args().raw_data())?;

    let inputs = QueryIter::new(load_cell_capacity, Source::GroupInput).count();
    let outputs = QueryIter::new(load_cell_capacity, Source::GroupOutput).count();
    match (inputs, outputs) {
        (0, 1) => create(&config),
        (1, 1) => update(&config),
        _ => Err(Error::GroupShape),
    }
}

/// The output pool cell must carry the pool lock, with this script's hash as
/// its args. Checked on every output, so every live pool cell has it.
fn check_pool_lock(config: &Config) -> Result<(), Error> {
    let lock = load_cell_lock(0, Source::GroupOutput).map_err(syscall)?;
    let own_hash = load_script_hash().map_err(syscall)?;
    let matches = lock.code_hash().as_slice() == config.lock_code_hash
        && lock.hash_type().as_slice() == [config.lock_hash_type]
        && lock.args().raw_data()[..] == own_hash;
    if matches {
        Ok(())
    } else {
        Err(Error::PoolLock)
    }
}

fn create(config: &Config) -> Result<(), Error> {
    check_pool_lock(config)?;
    let data = load_cell_data(0, Source::GroupOutput).map_err(syscall)?;
    if data != PoolState::empty().to_bytes() {
        return Err(Error::InitialState);
    }
    Ok(())
}

fn update(config: &Config) -> Result<(), Error> {
    check_pool_lock(config)?;

    let input_data = load_cell_data(0, Source::GroupInput).map_err(syscall)?;
    let mut state = PoolState::from_bytes(&input_data)?;
    let input_capacity = load_cell_capacity(0, Source::GroupInput).map_err(syscall)?;

    let witness = load_witness_args(0, Source::GroupInput).map_err(|_| Error::Witness)?;
    let action_bytes = witness.input_type().to_opt().ok_or(Error::Witness)?.raw_data();
    checkpoint("loaded");

    let expected_capacity = match action::decode(&action_bytes).ok_or(Error::ActionEncoding)? {
        Action::Deposit { commitments } => {
            let count = state.deposit(commitments)?;
            checkpoint("tree");
            u64::from(count)
                .checked_mul(config.denomination)
                .and_then(|added| input_capacity.checked_add(added))
                .ok_or(Error::OutputCapacity)?
        }
        Action::Withdraw(withdraw) => {
            check_withdrawal(config, &mut state, &withdraw)?;
            input_capacity
                .checked_sub(config.denomination)
                .ok_or(Error::OutputCapacity)?
        }
    };

    // The output must be exactly the state the action produces: byte for byte,
    // so there is no room to smuggle in another root, index or nullifier set.
    let output_data = load_cell_data(0, Source::GroupOutput).map_err(syscall)?;
    if output_data != state.to_bytes() {
        return Err(Error::OutputState);
    }
    let output_capacity = load_cell_capacity(0, Source::GroupOutput).map_err(syscall)?;
    if output_capacity != expected_capacity {
        return Err(Error::OutputCapacity);
    }
    checkpoint("end");
    Ok(())
}

fn check_withdrawal(config: &Config, state: &mut PoolState, withdraw: &Withdraw<'_>) -> Result<(), Error> {
    let payout = config
        .denomination
        .checked_sub(withdraw.fee)
        .ok_or(Error::FeeTooLarge)?;

    // Known root, canonical and unspent nullifier; marks the nullifier spent.
    state.withdraw(&withdraw.root, &withdraw.nullifier_hash, withdraw.nullifier_proof)?;
    checkpoint("nullifier");

    // The recipient is the output right after the pool cell. Fixing its position
    // means two pools in one transaction can never count the same output as
    // their payout.
    let own_hash = load_script_hash().map_err(syscall)?;
    let pool_index = QueryIter::new(load_cell_type_hash, Source::Output)
        .position(|type_hash| type_hash == Some(own_hash))
        .ok_or(Error::Syscall)?;
    let recipient_index = pool_index + 1;

    let recipient_lock_hash =
        load_cell_lock_hash(recipient_index, Source::Output).map_err(|_| Error::RecipientMissing)?;
    // The withdrawal transaction is not signed by anyone and can be rebuilt by
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
    let vk_index = look_for_dep_with_data_hash(&config.vk_hash).map_err(|_| Error::VkNotFound)?;
    let vk = load_cell_data(vk_index, Source::CellDep).map_err(syscall)?;
    let public_inputs = action::public_inputs(
        &withdraw.root,
        &withdraw.nullifier_hash,
        &recipient_lock_hash,
        withdraw.fee,
    );
    checkpoint("recipient");
    let verdict = verifier_core::verify(&vk, withdraw.proof, &public_inputs);
    checkpoint("groth16");
    verdict.map_err(|error| match error {
        VerifyError::InvalidVk => Error::InvalidVk,
        VerifyError::InvalidProof => Error::InvalidProof,
        VerifyError::InvalidPublicInputs => Error::InvalidPublicInputs,
        VerifyError::PublicInputCountMismatch => Error::PublicInputCountMismatch,
        VerifyError::VerificationFailed => Error::ProofRejected,
    })
}
