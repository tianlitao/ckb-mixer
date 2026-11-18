//! The action a transaction asks the pool to perform, carried in the
//! `input_type` field of the pool input's `WitnessArgs`.
//!
//! ```text
//! deposit:  0x00 | commitment (32) × n                                  n >= 1
//! withdraw: 0x01 | fee (8, LE) | root (32) | nullifier_hash (32)
//!                | groth16 proof (128) | nullifier-set proof (rest)
//! ```

use alloc::vec::Vec;

use crate::field::{fee_field, recipient_field, Bytes32};

pub const TAG_DEPOSIT: u8 = 0;
pub const TAG_WITHDRAW: u8 = 1;

/// Size of a compressed arkworks Groth16 proof on BN254.
pub const PROOF_LEN: usize = 128;

const WITHDRAW_FIXED_LEN: usize = 8 + 32 + 32 + PROOF_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<'a> {
    Deposit {
        /// `n × 32` bytes, `n >= 1`.
        commitments: &'a [u8],
    },
    Withdraw(Withdraw<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdraw<'a> {
    /// Shannons kept out of the denomination; whatever the transaction does not
    /// hand to another output ends up as the miner fee.
    pub fee: u64,
    pub root: Bytes32,
    pub nullifier_hash: Bytes32,
    pub proof: &'a [u8],
    pub nullifier_proof: &'a [u8],
}

pub fn decode(bytes: &[u8]) -> Option<Action<'_>> {
    let (tag, body) = bytes.split_first()?;
    match *tag {
        TAG_DEPOSIT => {
            if body.is_empty() || body.len() % 32 != 0 {
                return None;
            }
            Some(Action::Deposit { commitments: body })
        }
        TAG_WITHDRAW => {
            if body.len() < WITHDRAW_FIXED_LEN {
                return None;
            }
            let (fee, rest) = body.split_at(8);
            let (root, rest) = rest.split_at(32);
            let (nullifier_hash, rest) = rest.split_at(32);
            let (proof, nullifier_proof) = rest.split_at(PROOF_LEN);
            Some(Action::Withdraw(Withdraw {
                fee: u64::from_le_bytes(fee.try_into().ok()?),
                root: root.try_into().ok()?,
                nullifier_hash: nullifier_hash.try_into().ok()?,
                proof,
                nullifier_proof,
            }))
        }
        _ => None,
    }
}

pub fn encode_deposit(commitments: &[Bytes32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 32 * commitments.len());
    out.push(TAG_DEPOSIT);
    for commitment in commitments {
        out.extend_from_slice(commitment);
    }
    out
}

pub fn encode_withdraw(withdraw: &Withdraw<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + WITHDRAW_FIXED_LEN + withdraw.nullifier_proof.len());
    out.push(TAG_WITHDRAW);
    out.extend_from_slice(&withdraw.fee.to_le_bytes());
    out.extend_from_slice(&withdraw.root);
    out.extend_from_slice(&withdraw.nullifier_hash);
    out.extend_from_slice(withdraw.proof);
    out.extend_from_slice(withdraw.nullifier_proof);
    out
}

/// The withdraw circuit's public inputs in the wire format of `verifier-core`:
/// a `u32` count followed by 32-byte little-endian field elements, in the
/// circuit's order `[root, nullifierHash, recipient, fee]`.
pub fn public_inputs(
    root: &Bytes32,
    nullifier_hash: &Bytes32,
    recipient_lock_hash: &Bytes32,
    fee: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 4 * 32);
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(root);
    out.extend_from_slice(nullifier_hash);
    out.extend_from_slice(&recipient_field(recipient_lock_hash));
    out.extend_from_slice(&fee_field(fee));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_roundtrip() {
        let commitments = [[1u8; 32], [2u8; 32]];
        let bytes = encode_deposit(&commitments);
        assert_eq!(bytes.len(), 65);
        match decode(&bytes) {
            Some(Action::Deposit { commitments: body }) => {
                assert_eq!(&body[..32], &[1u8; 32]);
                assert_eq!(&body[32..], &[2u8; 32]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn withdraw_roundtrip() {
        let proof = [7u8; PROOF_LEN];
        let withdraw = Withdraw {
            fee: 0x0102_0304_0506_0708,
            root: [3u8; 32],
            nullifier_hash: [4u8; 32],
            proof: &proof,
            nullifier_proof: &[0u8; 32],
        };
        let bytes = encode_withdraw(&withdraw);
        assert_eq!(bytes.len(), 1 + 8 + 32 + 32 + 128 + 32);
        assert_eq!(decode(&bytes), Some(Action::Withdraw(withdraw)));
    }

    #[test]
    fn rejects_malformed_actions() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[TAG_DEPOSIT]), None);
        assert_eq!(decode(&[TAG_DEPOSIT, 1, 2, 3]), None);
        assert_eq!(decode(&[2u8; 33]), None);
        assert_eq!(decode(&[TAG_WITHDRAW; WITHDRAW_FIXED_LEN]), None);
        // An empty nullifier-set proof decodes; it is rejected later, by the set.
        assert!(decode(&[TAG_WITHDRAW; 1 + WITHDRAW_FIXED_LEN]).is_some());
    }

    #[test]
    fn public_inputs_layout() {
        let bytes = public_inputs(&[1u8; 32], &[2u8; 32], &[0xab; 32], 500);
        assert_eq!(bytes.len(), 4 + 4 * 32);
        assert_eq!(&bytes[..4], &[4, 0, 0, 0]);
        assert_eq!(&bytes[4..36], &[1u8; 32]);
        assert_eq!(&bytes[36..68], &[2u8; 32]);
        assert_eq!(&bytes[68..99], &[0xab; 31]);
        assert_eq!(bytes[99], 0);
        assert_eq!(&bytes[100..108], &500u64.to_le_bytes());
        assert!(bytes[108..].iter().all(|b| *b == 0));
    }
}
