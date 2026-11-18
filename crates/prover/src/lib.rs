//! Bridge between snarkjs (which produces the proofs) and the on-chain verifier
//! (which reads arkworks' compressed encoding).
//!
//! Host-only: it shells out to `circuits/scripts/prove.mjs`, so the circuit's
//! `build/` directory has to exist (`make setup`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use ark_bn254::{Bn254, Fq, Fq2, G1Affine, G2Affine};
use ark_ff::PrimeField;
use ark_groth16::{Proof, VerifyingKey};
use ark_serialize::CanonicalSerialize;
use mixer_core::merkle::{self, TREE_DEPTH};
use mixer_core::Fr;
use serde_json::{json, Value};

/// The repository root: the crate sits at `<root>/crates/prover`.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Where the compiled circuit and the proving key live; `MIXER_CIRCUITS_DIR`
/// overrides the default `circuits/` of the repository.
pub fn circuits_dir() -> PathBuf {
    match std::env::var_os("MIXER_CIRCUITS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => repo_root().join("circuits"),
    }
}

fn fq(value: &Value) -> Fq {
    let digits = value.as_str().expect("coordinate is a decimal string");
    Fq::from_str(digits).expect("coordinate is a base field element")
}

/// snarkjs writes G1 points as `[x, y, "1"]`.
fn g1(value: &Value) -> G1Affine {
    assert_eq!(value[2], "1", "point is not in affine form");
    // `new` panics unless the point is on the curve and in the right subgroup.
    G1Affine::new(fq(&value[0]), fq(&value[1]))
}

/// snarkjs writes G2 points as `[[x.c0, x.c1], [y.c0, y.c1], ["1", "0"]]`.
fn g2(value: &Value) -> G2Affine {
    assert_eq!(value[2][0], "1", "point is not in affine form");
    assert_eq!(value[2][1], "0", "point is not in affine form");
    G2Affine::new(
        Fq2::new(fq(&value[0][0]), fq(&value[0][1])),
        Fq2::new(fq(&value[1][0]), fq(&value[1][1])),
    )
}

fn compressed(value: &impl CanonicalSerialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.serialize_compressed(&mut bytes).expect("serialize");
    bytes
}

/// Convert snarkjs' `verification_key.json` into the bytes stored in the
/// verifying-key cell.
pub fn vk_from_snarkjs(json: &str) -> Vec<u8> {
    let vk: Value = serde_json::from_str(json).expect("verification key is JSON");
    assert_eq!(vk["protocol"], "groth16");
    assert_eq!(vk["curve"], "bn128");
    let ic = vk["IC"].as_array().expect("IC is an array");
    compressed(&VerifyingKey::<Bn254> {
        alpha_g1: g1(&vk["vk_alpha_1"]),
        beta_g2: g2(&vk["vk_beta_2"]),
        gamma_g2: g2(&vk["vk_gamma_2"]),
        delta_g2: g2(&vk["vk_delta_2"]),
        gamma_abc_g1: ic.iter().map(g1).collect(),
    })
}

/// Convert snarkjs' `proof.json` into the 128 bytes that go into the witness.
pub fn proof_from_snarkjs(json: &str) -> Vec<u8> {
    let proof: Value = serde_json::from_str(json).expect("proof is JSON");
    compressed(&Proof::<Bn254> {
        a: g1(&proof["pi_a"]),
        b: g2(&proof["pi_b"]),
        c: g1(&proof["pi_c"]),
    })
}

/// The verifying key of the withdraw circuit, in on-chain encoding.
pub fn verifying_key() -> Vec<u8> {
    let path = circuits_dir().join("build").join("verification_key.json");
    let json = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("{} is missing; run `make setup` first", path.display())
    });
    vk_from_snarkjs(&json)
}

fn decimal(x: &Fr) -> String {
    x.into_bigint().to_string()
}

/// Everything the withdraw circuit takes as input.
#[derive(Debug, Clone)]
pub struct WithdrawWitness {
    // public
    pub root: Fr,
    pub nullifier_hash: Fr,
    pub recipient: Fr,
    pub fee: u64,
    // private
    pub nullifier: Fr,
    pub secret: Fr,
    pub path_elements: [Fr; TREE_DEPTH],
    pub leaf_index: u32,
}

impl WithdrawWitness {
    /// Fill in the Merkle path of leaf `leaf_index` in the tree with `leaves`.
    pub fn for_leaf(
        leaves: &[Fr],
        leaf_index: u32,
        nullifier: Fr,
        secret: Fr,
        nullifier_hash: Fr,
        recipient: Fr,
        fee: u64,
    ) -> Self {
        let (path_elements, root) =
            merkle::merkle_path(leaves, leaf_index as usize).expect("leaf index is in the tree");
        Self { root, nullifier_hash, recipient, fee, nullifier, secret, path_elements, leaf_index }
    }

    /// The circuit's input, as snarkjs reads it.
    pub fn to_json(&self) -> Value {
        json!({
            "root": decimal(&self.root),
            "nullifierHash": decimal(&self.nullifier_hash),
            "recipient": decimal(&self.recipient),
            "fee": self.fee.to_string(),
            "nullifier": decimal(&self.nullifier),
            "secret": decimal(&self.secret),
            "pathElements": self.path_elements.iter().map(decimal).collect::<Vec<_>>(),
            "pathIndices": (0..TREE_DEPTH)
                .map(|level| ((self.leaf_index >> level) & 1).to_string())
                .collect::<Vec<_>>(),
        })
    }

    /// The public signals in circuit order, as snarkjs prints them.
    fn public_signals(&self) -> Vec<String> {
        vec![
            decimal(&self.root),
            decimal(&self.nullifier_hash),
            decimal(&self.recipient),
            self.fee.to_string(),
        ]
    }
}

static PROOF_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run the snarkjs prover and return the proof in on-chain encoding.
///
/// Fails if the witness does not satisfy the circuit; snarkjs refuses to
/// produce a proof in that case.
pub fn prove(witness: &WithdrawWitness) -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join(format!(
        "ckb-mixer-proof-{}-{}",
        std::process::id(),
        PROOF_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = prove_in(witness, &dir);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn prove_in(witness: &WithdrawWitness, dir: &Path) -> Result<Vec<u8>, String> {
    let input_path = dir.join("input.json");
    fs::write(&input_path, witness.to_json().to_string()).map_err(|e| e.to_string())?;

    let output = Command::new("node")
        .arg(circuits_dir().join("scripts").join("prove.mjs"))
        .arg(&input_path)
        .arg(dir)
        .output()
        .map_err(|e| format!("cannot run node: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }

    let public: Vec<String> =
        serde_json::from_str(&fs::read_to_string(dir.join("public.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if public != witness.public_signals() {
        return Err(format!("unexpected public signals: {public:?}"));
    }
    let proof = fs::read_to_string(dir.join("proof.json")).map_err(|e| e.to_string())?;
    Ok(proof_from_snarkjs(&proof))
}
