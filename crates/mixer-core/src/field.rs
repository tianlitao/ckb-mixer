//! Byte encoding of BN254 scalar field elements.
//!
//! On chain a field element is always 32 bytes, little-endian, and canonical
//! (strictly below the field modulus r).
//!
//! The canonical check is load-bearing. r is about 2^253.8, so a 32-byte string
//! can hold x, x + r, x + 2r, ... (up to five values) that all reduce to the same
//! field element. A verifier that reduces silently would accept the same proof
//! for each of them, while the nullifier set, which is keyed by the raw bytes,
//! would see five different nullifiers: one deposit, five withdrawals.

use ark_bn254::Fr;
use ark_ff::{BigInt, PrimeField};

pub type Bytes32 = [u8; 32];

/// Decode a canonical little-endian field element. `None` if the value is >= r.
pub fn fr_from_le_bytes(bytes: &Bytes32) -> Option<Fr> {
    let mut limbs = [0u64; 4];
    for (limb, chunk) in limbs.iter_mut().zip(bytes.chunks_exact(8)) {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        *limb = u64::from_le_bytes(word);
    }
    // `from_bigint` rejects values >= r instead of reducing them.
    Fr::from_bigint(BigInt::new(limbs))
}

/// Encode a field element as 32 canonical little-endian bytes.
pub fn fr_to_le_bytes(x: &Fr) -> Bytes32 {
    let mut out = [0u8; 32];
    for (chunk, limb) in out.chunks_exact_mut(8).zip(x.into_bigint().0.iter()) {
        chunk.copy_from_slice(&limb.to_le_bytes());
    }
    out
}

/// Public input `recipient`: the first 31 bytes of the recipient's lock script
/// hash, read as a little-endian integer.
///
/// A 32-byte hash does not fit in the field, so the last byte is dropped. The
/// result is below 2^248 and therefore always canonical. Redirecting a
/// withdrawal would need a lock script whose hash collides on 248 bits.
pub fn recipient_field(lock_hash: &Bytes32) -> Bytes32 {
    let mut out = *lock_hash;
    out[31] = 0;
    out
}

/// Public input `fee`: shannons, as a field element.
pub fn fee_field(fee: u64) -> Bytes32 {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&fee.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_ff::MontFp;

    /// r, the BN254 scalar field modulus, little-endian.
    fn modulus_le() -> Bytes32 {
        let mut out = [0u8; 32];
        for (chunk, limb) in out.chunks_exact_mut(8).zip(Fr::MODULUS.0.iter()) {
            chunk.copy_from_slice(&limb.to_le_bytes());
        }
        out
    }

    /// 256-bit little-endian addition, panics on overflow.
    fn add_le(a: &Bytes32, b: &Bytes32) -> Bytes32 {
        let mut out = [0u8; 32];
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = a[i] as u16 + b[i] as u16 + carry;
            out[i] = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0, "overflow");
        out
    }

    #[test]
    fn roundtrip() {
        let x: Fr = MontFp!("7853200120776062878684798364095072458815029376092732009249414926327459813530");
        let bytes = fr_to_le_bytes(&x);
        assert_eq!(fr_from_le_bytes(&bytes), Some(x));
        assert_eq!(fr_to_le_bytes(&Fr::from(0x0102_0304u64))[..5], [4, 3, 2, 1, 0]);
    }

    #[test]
    fn rejects_values_at_or_above_modulus() {
        let r = modulus_le();
        assert_eq!(fr_from_le_bytes(&r), None);
        assert_eq!(fr_from_le_bytes(&[0xff; 32]), None);

        let mut r_minus_1 = r;
        r_minus_1[0] -= 1;
        assert_eq!(fr_from_le_bytes(&r_minus_1), Some(-Fr::from(1u64)));
    }

    #[test]
    fn rejects_every_alias_of_a_value() {
        // x, x + r, x + 2r, ... all mean the same field element; only x is accepted.
        let x = fr_to_le_bytes(&Fr::from(42u64));
        let r = modulus_le();
        let mut alias = x;
        for _ in 0..4 {
            alias = add_le(&alias, &r);
            assert_eq!(fr_from_le_bytes(&alias), None);
        }
        assert_eq!(fr_from_le_bytes(&x), Some(Fr::from(42u64)));
    }

    #[test]
    fn recipient_and_fee_are_canonical() {
        assert!(fr_from_le_bytes(&recipient_field(&[0xff; 32])).is_some());
        assert_eq!(fr_from_le_bytes(&fee_field(u64::MAX)), Some(Fr::from(u64::MAX)));
    }
}
