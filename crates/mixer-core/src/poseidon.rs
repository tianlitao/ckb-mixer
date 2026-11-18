//! Poseidon over the BN254 scalar field, compatible with circomlib's
//! `Poseidon(2)` template and circomlibjs' `poseidon([a, b])`.
//!
//! Parameters: state width t = 3, 8 full rounds, 57 partial rounds, S-box x^5.
//! This is the plain reference permutation (no sparse-matrix optimisation), kept
//! simple on purpose: any deviation from circomlib makes every withdrawal proof
//! fail, because the circuit recomputes the tree that this code builds on chain.

use ark_bn254::Fr;
use ark_ff::{Field, Zero};

use crate::poseidon_constants::{C, M, ROUNDS_F, ROUNDS_P};

#[inline(always)]
fn pow5(x: Fr) -> Fr {
    let x2 = x.square();
    x2.square() * x
}

/// `Poseidon(a, b)`.
pub fn hash2(a: &Fr, b: &Fr) -> Fr {
    let mut s = [Fr::zero(), *a, *b];
    let half_full = ROUNDS_F / 2;

    for round in 0..ROUNDS_F + ROUNDS_P {
        s[0] += C[round * 3];
        s[1] += C[round * 3 + 1];
        s[2] += C[round * 3 + 2];

        s[0] = pow5(s[0]);
        if round < half_full || round >= half_full + ROUNDS_P {
            s[1] = pow5(s[1]);
            s[2] = pow5(s[2]);
        }

        s = [
            M[0][0] * s[0] + M[0][1] * s[1] + M[0][2] * s[2],
            M[1][0] * s[0] + M[1][1] * s[1] + M[1][2] * s[2],
            M[2][0] * s[0] + M[2][1] * s[1] + M[2][2] * s[2],
        ];
    }
    s[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poseidon_constants::{TEST_VECTORS, ZEROS};

    #[test]
    fn matches_circomlibjs_vectors() {
        for (a, b, expected) in TEST_VECTORS.iter() {
            assert_eq!(hash2(a, b), *expected);
        }
    }

    #[test]
    fn zeros_table_matches_circomlibjs() {
        // The table was computed by circomlibjs; recompute it with this implementation.
        assert!(ZEROS[0].is_zero());
        for i in 0..ZEROS.len() - 1 {
            assert_eq!(hash2(&ZEROS[i], &ZEROS[i]), ZEROS[i + 1], "level {i}");
        }
    }
}
