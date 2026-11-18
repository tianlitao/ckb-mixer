//! Lock script of the mixer pool cell.
//!
//! args: the hash of the pool type script (32 bytes).
//!
//! The pool cell belongs to nobody, so there is no signature to check. Anyone
//! may spend it, provided the pool type script is attached to it: that script
//! then runs in the same transaction and enforces the deposit and withdrawal
//! rules. A cell that carries this lock without the pool type script can never
//! be spent.

#![no_std]
#![no_main]

use ckb_std::{
    ckb_constants::Source,
    high_level::{load_cell_type_hash, load_script, QueryIter},
};

ckb_std::entry!(program_entry);
ckb_std::default_alloc!();

// Kept apart from the pool type script's codes, so a failing transaction shows
// which of the two scripts rejected it.
const ERROR_SYSCALL: i8 = 101;
const ERROR_ARGS: i8 = 102;
const ERROR_NOT_A_POOL_CELL: i8 = 103;

fn program_entry() -> i8 {
    let script = match load_script() {
        Ok(script) => script,
        Err(_) => return ERROR_SYSCALL,
    };
    let args = script.args().raw_data();
    if args.len() != 32 {
        return ERROR_ARGS;
    }

    for type_hash in QueryIter::new(load_cell_type_hash, Source::GroupInput) {
        match type_hash {
            Some(hash) if hash[..] == args[..] => {}
            _ => return ERROR_NOT_A_POOL_CELL,
        }
    }
    0
}
