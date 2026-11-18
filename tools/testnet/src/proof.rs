//! Withdrawal proofs: the prover of the circuit, bound to a recipient lock.

use ark_ff::PrimeField;
use ckb_types::packed::Script;
use mixer_core::action;
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes, recipient_field};
use mixer_core::Fr;
use mixer_prover::WithdrawWitness;

/// The withdraw circuit's verifying key, in the encoding of the cell that holds
/// it on chain.
pub fn verifying_key() -> Vec<u8> {
    mixer_prover::verifying_key()
}

/// A proof together with everything it is bound to.
#[derive(Debug, Clone)]
pub struct Withdrawal {
    pub root: [u8; 32],
    pub nullifier_hash: [u8; 32],
    pub recipient_lock_hash: [u8; 32],
    pub fee: u64,
    pub proof: Vec<u8>,
}

impl Withdrawal {
    /// The `input_type` action the pool expects.
    pub fn action(&self, nullifier_proof: &[u8]) -> Vec<u8> {
        action::encode_withdraw(&action::Withdraw {
            fee: self.fee,
            root: self.root,
            nullifier_hash: self.nullifier_hash,
            proof: &self.proof,
            nullifier_proof,
        })
    }
}

/// Prove that `note` is leaf `leaf_index` of `leaves`, paying `recipient`.
pub fn prove(
    leaves: &[Fr],
    leaf_index: u32,
    nullifier: Fr,
    secret: Fr,
    recipient: &Script,
    fee: u64,
) -> Result<Withdrawal, String> {
    use ckb_types::prelude::*;
    let lock_hash: [u8; 32] = recipient.calc_script_hash().as_slice().try_into().map_err(|_| "hash")?;
    let recipient_field = recipient_field(&lock_hash);
    let recipient_fr = fr_from_le_bytes(&recipient_field).expect("31 bytes fit the field");
    let nullifier_hash = mixer_core::poseidon::hash2(&nullifier, &Fr::from(leaf_index));

    let witness = WithdrawWitness::for_leaf(
        leaves,
        leaf_index,
        nullifier,
        secret,
        nullifier_hash,
        recipient_fr,
        fee,
    );
    let proof = mixer_prover::prove(&witness)?;
    Ok(Withdrawal {
        root: fr_to_le_bytes(&witness.root),
        nullifier_hash: fr_to_le_bytes(&nullifier_hash),
        recipient_lock_hash: lock_hash,
        fee,
        proof,
    })
}

/// Verify a withdrawal off chain with the same verifier the pool links in.
pub fn verify(vk: &[u8], withdrawal: &Withdrawal) -> Result<(), String> {
    let public_inputs = action::public_inputs(
        &withdrawal.root,
        &withdrawal.nullifier_hash,
        &withdrawal.recipient_lock_hash,
        withdrawal.fee,
    );
    verifier_core::verify(vk, &withdrawal.proof, &public_inputs).map_err(|error| format!("{error:?}"))
}

/// The nullifier hash as a decimal string, for logs.
pub fn decimal(bytes: &[u8; 32]) -> String {
    fr_from_le_bytes(bytes).expect("canonical").into_bigint().to_string()
}
