//! Consensus-critical logic of the CKB mixer PoC.
//!
//! Everything the pool type script decides lives here so that the host side
//! (tests, a future SDK) runs the exact code that runs on chain.

#![no_std]

extern crate alloc;

pub mod action;
pub mod error;
pub mod field;
pub mod merkle;
pub mod poseidon;
mod poseidon_constants;
pub mod smt;
pub mod state;

pub use ark_bn254::Fr;
pub use error::Error;
pub use field::Bytes32;
