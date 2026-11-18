//! Lock script of a deposit intent cell (pool v2).
//!
//! args (104 bytes), see `mixer-core-v2`:
//!   pool_type_hash   32      script hash of the pool's type script
//!   commitment       32      commitment of the note being deposited
//!   owner_lock_hash  32      who may take the intent back
//!   reclaim_after    u64 LE  blocks the owner has to wait before doing so
//!
//! A depositor puts `denomination + tip` into a cell with this lock and is
//! done: no signature of theirs is needed later. The cell can be spent in two
//! ways.
//!
//! * Deposit. The pool named in the args is among the inputs, so its type
//!   script runs, counts this cell as a deposit of `commitment` and requires
//!   the pool to grow by the denomination. Anyone can build this transaction.
//!
//! * Reclaim. A cell of the owner is among the inputs, which takes the owner's
//!   signature, and every intent input carries a relative `since` of at least
//!   `reclaim_after` blocks. The wait gives whoever batches intents a window
//!   in which the owner cannot pull one out from under a pending batch.
//!   `owner_lock_hash` must name a lock only the owner can unlock.
//!
//! The pool counts an input as a deposit when its lock is the intent lock named
//! in the pool's own args: that code hash and that hash type. The same code
//! referenced through another hash type is a different lock to the pool, and a
//! cell locked that way would be spent without being counted. So on the deposit
//! path this script reads the pool's args and insists that they name this very
//! script.

#![no_std]
#![no_main]

use ckb_std::{
    ckb_constants::Source,
    ckb_types::prelude::*,
    error::SysError,
    high_level::{
        load_cell_data, load_cell_lock_hash, load_cell_type, load_cell_type_hash, load_input_since,
        load_script, QueryIter,
    },
};
use mixer_core_v2::{since_allows_reclaim, IntentArgs, IntentError, PoolConfig};

ckb_std::entry!(program_entry);
ckb_std::default_alloc!();

fn syscall(_: SysError) -> IntentError {
    IntentError::Syscall
}

fn program_entry() -> i8 {
    match run() {
        Ok(()) => 0,
        Err(error) => error.code(),
    }
}

fn run() -> Result<(), IntentError> {
    let script = load_script().map_err(syscall)?;
    let args = IntentArgs::parse(&script.args().raw_data()).ok_or(IntentError::ScriptArgs)?;

    let pool_input = QueryIter::new(load_cell_type_hash, Source::Input)
        .position(|type_hash| type_hash == Some(args.pool_type_hash));
    match pool_input {
        Some(index) => deposit(&script, index),
        None => reclaim(&args),
    }
}

fn deposit(script: &ckb_std::ckb_types::packed::Script, pool_input: usize) -> Result<(), IntentError> {
    let pool_type = load_cell_type(pool_input, Source::Input)
        .map_err(syscall)?
        .ok_or(IntentError::Syscall)?;
    let config =
        PoolConfig::parse(&pool_type.args().raw_data()).map_err(|_| IntentError::PoolMismatch)?;
    let counted = script.code_hash().as_slice() == config.intent_lock.code_hash
        && script.hash_type().as_slice() == [config.intent_lock.hash_type];
    if !counted {
        return Err(IntentError::PoolMismatch);
    }

    // An intent is plain CKB. Whatever else a cell carried would be destroyed
    // by a transaction its owner never signed.
    for index in 0.. {
        match load_cell_type_hash(index, Source::GroupInput) {
            Ok(None) => {}
            Ok(Some(_)) => return Err(IntentError::IntentShape),
            Err(SysError::IndexOutOfBound) => break,
            Err(_) => return Err(IntentError::Syscall),
        }
        if !load_cell_data(index, Source::GroupInput).map_err(syscall)?.is_empty() {
            return Err(IntentError::IntentShape);
        }
    }
    Ok(())
}

fn reclaim(args: &IntentArgs) -> Result<(), IntentError> {
    let owner_present = QueryIter::new(load_cell_lock_hash, Source::Input)
        .any(|lock_hash| lock_hash == args.owner_lock_hash);
    if !owner_present {
        return Err(IntentError::Unauthorized);
    }
    for since in QueryIter::new(load_input_since, Source::GroupInput) {
        if !since_allows_reclaim(since, args.reclaim_after) {
            return Err(IntentError::ReclaimTooEarly);
        }
    }
    Ok(())
}
