//! WebAssembly bindings of the mixer's core logic, for the web front end and
//! the aggregator Worker.
//!
//! Nothing is reimplemented here: Poseidon, the commitment tree, the nullifier
//! set, the pool state and every encoding are the crates the on-chain scripts
//! are built from, and the snarkjs conversions are `mixer-prover`'s. What this
//! crate adds is [`Ledger`], the replay of a pool's transactions that
//! `tools/testnet/src/v2.rs` does on the host, with the chain access left to
//! the caller.
//!
//! Conventions: field elements, hashes and commitments cross the boundary as
//! bytes (`Uint8Array`), 32 each, in the on-chain little-endian encoding;
//! shannons as `u64` (`bigint`); anything structured as a JSON string.

use std::collections::BTreeSet;

use ark_ff::PrimeField;
use blake2b_ref::Blake2bBuilder;
use mixer_core::action::{Withdraw, PROOF_LEN};
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes, recipient_field};
use mixer_core::poseidon::hash2;
use mixer_core::smt::NullifierSet;
use mixer_core::{Bytes32, Fr};
use mixer_core_v2::{self as core, IntentArgs, PoolConfig, PoolState};
use mixer_prover::WithdrawWitness;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

/// The longest wait a relative `since` can express, in blocks. The intent lock
/// takes any `u64`, and no script runs when a cell is created: an intent made
/// with a longer wait can be deposited, but never taken back.
const LONGEST_WAIT: u64 = (1 << 56) - 1;

/// One withdrawal of a batch as the caller hands it over, without its
/// nullifier-set proof: fee (8, LE) | root | nullifier hash | Groth16 proof.
const REQUEST_LEN: usize = 8 + 32 + 32 + PROOF_LEN;

type Res<T> = Result<T, JsError>;

fn err(message: impl AsRef<str>) -> JsError {
    JsError::new(message.as_ref())
}

fn bytes32(bytes: &[u8], what: &str) -> Res<Bytes32> {
    bytes.try_into().map_err(|_| err(format!("{what}: expected 32 bytes, got {}", bytes.len())))
}

fn field(bytes: &[u8], what: &str) -> Res<Fr> {
    fr_from_le_bytes(&bytes32(bytes, what)?).ok_or_else(|| err(format!("{what}: not a canonical field element")))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn chunks32(bytes: &[u8], what: &str) -> Res<Vec<Bytes32>> {
    if bytes.len() % 32 != 0 {
        return Err(err(format!("{what}: {} bytes is not a whole number of 32-byte values", bytes.len())));
    }
    Ok(bytes.chunks_exact(32).map(|chunk| chunk.try_into().expect("32 bytes")).collect())
}

// ---- notes

/// `Poseidon(nullifier, secret)`: the leaf a note puts into the tree.
#[wasm_bindgen]
pub fn commitment(nullifier: &[u8], secret: &[u8]) -> Res<Vec<u8>> {
    Ok(fr_to_le_bytes(&hash2(&field(nullifier, "nullifier")?, &field(secret, "secret")?)).to_vec())
}

/// `Poseidon(nullifier, leaf_index)`: what a withdrawal reveals.
#[wasm_bindgen]
pub fn nullifier_hash(nullifier: &[u8], leaf_index: u32) -> Res<Vec<u8>> {
    Ok(fr_to_le_bytes(&hash2(&field(nullifier, "nullifier")?, &Fr::from(leaf_index))).to_vec())
}

/// Whether 32 bytes are a canonical field element; a commitment has to be one.
#[wasm_bindgen]
pub fn is_field_element(bytes: &[u8]) -> bool {
    <Bytes32>::try_from(bytes).ok().and_then(|bytes| fr_from_le_bytes(&bytes)).is_some()
}

// ---- hashing and script layouts

/// CKB's default hash: blake2b-256 personalised with `ckb-default-hash`.
#[wasm_bindgen]
pub fn ckb_hash(data: &[u8]) -> Vec<u8> {
    let mut hasher = Blake2bBuilder::new(32).personal(b"ckb-default-hash").build();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out.to_vec()
}

fn config_json(config: &PoolConfig) -> Value {
    json!({
        "type_id": hex(&config.type_id),
        "denomination": config.denomination.to_string(),
        "vk_hash": hex(&config.vk_hash),
        "pool_lock": { "code_hash": hex(&config.pool_lock.code_hash), "hash_type": config.pool_lock.hash_type },
        "intent_lock": { "code_hash": hex(&config.intent_lock.code_hash), "hash_type": config.intent_lock.hash_type },
        "root_history": config.root_history,
    })
}

fn parse_config(args: &[u8]) -> Res<PoolConfig> {
    PoolConfig::parse(args).map_err(|_| err("these are not the args of a v2 pool type script"))
}

/// The fields of a v2 pool type script's args, as JSON. `hash_type` is the
/// byte the script carries (4 is `data2`).
#[wasm_bindgen]
pub fn pool_config(args: &[u8]) -> Res<String> {
    Ok(config_json(&parse_config(args)?).to_string())
}

/// The 104 bytes of an intent lock's args.
///
/// `reclaim_after` is a number of blocks, not an encoded `since`: a value of
/// 2^56 or more is refused here, because the cell it would make could never be
/// taken back and no script would say so when it is created.
#[wasm_bindgen]
pub fn intent_args(pool_type_hash: &[u8], commitment: &[u8], owner_lock_hash: &[u8], reclaim_after: u64) -> Res<Vec<u8>> {
    if reclaim_after > LONGEST_WAIT {
        return Err(err("reclaim_after is a number of blocks below 2^56, not an encoded since"));
    }
    field(commitment, "commitment")?;
    Ok(IntentArgs {
        pool_type_hash: bytes32(pool_type_hash, "pool type hash")?,
        commitment: bytes32(commitment, "commitment")?,
        owner_lock_hash: bytes32(owner_lock_hash, "owner lock hash")?,
        reclaim_after,
    }
    .to_bytes())
}

/// The fields of an intent lock's args as JSON, or `undefined` if they are not
/// 104 bytes. `reclaimable` says whether any `since` can express the wait.
#[wasm_bindgen]
pub fn parse_intent_args(args: &[u8]) -> Option<String> {
    let args = IntentArgs::parse(args)?;
    Some(
        json!({
            "pool_type_hash": hex(&args.pool_type_hash),
            "commitment": hex(&args.commitment),
            "owner_lock_hash": hex(&args.owner_lock_hash),
            "reclaim_after": args.reclaim_after.to_string(),
            "reclaimable": args.reclaim_after <= LONGEST_WAIT,
            "canonical": fr_from_le_bytes(&args.commitment).is_some(),
        })
        .to_string(),
    )
}

/// The `since` the input of a reclaim needs after a wait of `blocks` blocks.
#[wasm_bindgen]
pub fn reclaim_since(blocks: u64) -> Res<u64> {
    if blocks > LONGEST_WAIT {
        return Err(err("no since can express a wait of 2^56 blocks or more"));
    }
    Ok(core::reclaim_since(blocks))
}

/// The withdrawals of a batch action, as a JSON array; `undefined` if the
/// bytes are not one well-formed batch.
#[wasm_bindgen]
pub fn decode_batch(action: &[u8]) -> Option<String> {
    let withdrawals = core::decode_batch(action)?;
    let list: Vec<Value> = withdrawals
        .iter()
        .map(|withdraw| {
            json!({
                "fee": withdraw.fee.to_string(),
                "root": hex(&withdraw.root),
                "nullifier_hash": hex(&withdraw.nullifier_hash),
                "proof": hex(withdraw.proof),
            })
        })
        .collect();
    Some(Value::Array(list).to_string())
}

// ---- snarkjs

/// snarkjs' `proof.json` as the 128 bytes the pool's verifier reads.
#[wasm_bindgen]
pub fn proof_from_snarkjs(json: &str) -> Vec<u8> {
    mixer_prover::proof_from_snarkjs(json)
}

/// snarkjs' `verification_key.json` as the bytes of the verifying-key cell.
/// Their [`ckb_hash`] has to be the `vk_hash` of the pool's args.
#[wasm_bindgen]
pub fn vk_from_snarkjs(json: &str) -> Vec<u8> {
    mixer_prover::vk_from_snarkjs(json)
}

// ---- the pool, replayed

/// What a pool's transactions so far add up to: the state its cell holds, and
/// the commitment tree and nullifier set behind the roots in that state.
#[wasm_bindgen]
pub struct Ledger {
    config: PoolConfig,
    state: PoolState,
    leaves: Vec<Fr>,
    spent: BTreeSet<Bytes32>,
    capacity: u64,
}

impl Ledger {
    fn nullifiers(&self) -> NullifierSet {
        let mut set = NullifierSet::new();
        for key in &self.spent {
            set.insert(*key);
        }
        set
    }
}

#[wasm_bindgen]
impl Ledger {
    /// A pool as its creation leaves it: the empty state, in a cell of
    /// `capacity` shannons. `pool_args` are the args of its type script.
    #[wasm_bindgen(constructor)]
    pub fn new(pool_args: &[u8], capacity: u64) -> Res<Ledger> {
        let config = parse_config(pool_args)?;
        let state = PoolState::empty(config.root_history);
        Ok(Ledger { config, state, leaves: Vec::new(), spent: BTreeSet::new(), capacity })
    }

    /// A ledger saved with [`Ledger::snapshot`]. The caller still has to hold
    /// the result against a pool cell on chain: a snapshot is a cache.
    pub fn restore(pool_args: &[u8], snapshot: &str) -> Res<Ledger> {
        let config = parse_config(pool_args)?;
        let value: Value = serde_json::from_str(snapshot).map_err(|error| err(format!("snapshot: {error}")))?;
        let unhex = |value: &Value, what: &str| -> Res<Vec<u8>> {
            let text = value.as_str().ok_or_else(|| err(format!("snapshot: no {what}")))?;
            let digits = text.strip_prefix("0x").unwrap_or(text);
            if digits.len() % 2 != 0 || !digits.is_ascii() {
                return Err(err(format!("snapshot: {what} is not hex")));
            }
            (0..digits.len() / 2)
                .map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).map_err(|_| err(format!("snapshot: {what} is not hex"))))
                .collect()
        };
        let state = PoolState::from_bytes(&unhex(&value["state"], "state")?, config.root_history)
            .map_err(|_| err("snapshot: the state does not fit this pool"))?;
        let mut leaves = Vec::new();
        for leaf in chunks32(&unhex(&value["leaves"], "leaves")?, "snapshot leaves")? {
            leaves.push(field(&leaf, "snapshot leaf")?);
        }
        let spent: BTreeSet<Bytes32> = chunks32(&unhex(&value["nullifiers"], "nullifiers")?, "snapshot nullifiers")?.into_iter().collect();
        let capacity = value["capacity"].as_str().and_then(|text| text.parse().ok()).ok_or_else(|| err("snapshot: no capacity"))?;
        if leaves.len() != state.next_index as usize {
            return Err(err("snapshot: the leaves are not the ones the state counts"));
        }
        Ok(Ledger { config, state, leaves, spent, capacity })
    }

    pub fn snapshot(&self) -> String {
        let leaves: Vec<u8> = self.leaves.iter().flat_map(fr_to_le_bytes).collect();
        let nullifiers: Vec<u8> = self.spent.iter().flatten().copied().collect();
        json!({
            "state": hex(&self.state.to_bytes()),
            "leaves": hex(&leaves),
            "nullifiers": hex(&nullifiers),
            "capacity": self.capacity.to_string(),
        })
        .to_string()
    }

    /// Apply one pool transaction the way the type script did: the withdrawals
    /// of `action` (the `input_type` of the pool input's witness) first, then
    /// one deposit per intent input, whose `commitments` (n × 32 bytes) the
    /// caller read from the locks of the transaction's inputs, in input order.
    ///
    /// The nullifier-set proofs are the transaction's own, checked against the
    /// root kept here exactly as the script checked them. Leaves the ledger
    /// untouched if anything is refused.
    pub fn apply(&mut self, action: &[u8], commitments: &[u8]) -> Res<()> {
        let withdrawals = core::decode_batch(action).ok_or_else(|| err("not a batch action"))?;
        let commitments = chunks32(commitments, "commitments")?;
        if withdrawals.is_empty() && commitments.is_empty() {
            return Err(err("EmptyBatch"));
        }
        let mut state = self.state.clone();
        let mut spent = Vec::with_capacity(withdrawals.len());
        for withdraw in &withdrawals {
            state
                .withdraw(&withdraw.root, &withdraw.nullifier_hash, withdraw.nullifier_proof)
                .map_err(|error| err(format!("{error:?}")))?;
            spent.push(withdraw.nullifier_hash);
        }
        state.deposit(&commitments).map_err(|error| err(format!("{error:?}")))?;
        let capacity = (self.capacity + commitments.len() as u64 * self.config.denomination)
            .checked_sub(withdrawals.len() as u64 * self.config.denomination)
            .ok_or_else(|| err("pool overdrawn"))?;

        self.state = state;
        self.capacity = capacity;
        self.spent.extend(spent);
        self.leaves.extend(commitments.iter().map(|commitment| fr_from_le_bytes(commitment).expect("the deposit checked it")));
        Ok(())
    }

    /// The pool cell's data this ledger stands for.
    pub fn state_bytes(&self) -> Vec<u8> {
        self.state.to_bytes()
    }

    /// The pool cell's capacity this ledger stands for, in shannons.
    #[wasm_bindgen(getter)]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    #[wasm_bindgen(getter)]
    pub fn denomination(&self) -> u64 {
        self.config.denomination
    }

    #[wasm_bindgen(getter)]
    pub fn leaf_count(&self) -> u32 {
        self.leaves.len() as u32
    }

    #[wasm_bindgen(getter)]
    pub fn nullifier_count(&self) -> u32 {
        self.spent.len() as u32
    }

    pub fn current_root(&self) -> Vec<u8> {
        self.state.current_root().to_vec()
    }

    pub fn nullifier_root(&self) -> Vec<u8> {
        self.state.nullifier_root.to_vec()
    }

    pub fn is_known_root(&self, root: &[u8]) -> bool {
        <Bytes32>::try_from(root).is_ok_and(|root| self.state.is_known_root(&root))
    }

    pub fn is_spent(&self, nullifier_hash: &[u8]) -> bool {
        <Bytes32>::try_from(nullifier_hash).is_ok_and(|key| self.spent.contains(&key))
    }

    /// Where `commitment` sits in the tree, if it is there.
    pub fn leaf_index(&self, commitment: &[u8]) -> Option<u32> {
        let leaf = fr_from_le_bytes(&<Bytes32>::try_from(commitment).ok()?)?;
        self.leaves.iter().position(|other| *other == leaf).map(|index| index as u32)
    }

    /// The batch that pays `requests` and deposits `commitments`, in these
    /// orders, on top of this ledger.
    ///
    /// `requests` is w × 200 bytes: fee (8, LE) | root | nullifier hash |
    /// Groth16 proof. Returns JSON with the `action` for the pool input's
    /// witness, and the `state` and `capacity` the pool output has to have.
    /// Whether the proofs verify is not checked here.
    pub fn batch(&self, requests: &[u8], commitments: &[u8]) -> Res<String> {
        if requests.len() % REQUEST_LEN != 0 {
            return Err(err("requests: not a whole number of withdrawals"));
        }
        let commitments = chunks32(commitments, "commitments")?;
        if requests.is_empty() && commitments.is_empty() {
            return Err(err("EmptyBatch"));
        }
        let mut state = self.state.clone();
        let mut nullifiers = self.nullifiers();

        struct Item<'a> {
            fee: u64,
            root: Bytes32,
            nullifier_hash: Bytes32,
            proof: &'a [u8],
            nullifier_proof: Vec<u8>,
        }
        let mut items = Vec::new();
        for request in requests.chunks_exact(REQUEST_LEN) {
            let fee = u64::from_le_bytes(request[..8].try_into().expect("8 bytes"));
            let root: Bytes32 = request[8..40].try_into().expect("32 bytes");
            let nullifier_hash: Bytes32 = request[40..72].try_into().expect("32 bytes");
            if fee > self.config.denomination {
                return Err(err(format!("withdrawal {}: FeeTooLarge", hex(&nullifier_hash))));
            }
            // Each proof is against the set as the withdrawals before it leave it.
            let nullifier_proof = nullifiers.proof(&nullifier_hash);
            state
                .withdraw(&root, &nullifier_hash, &nullifier_proof)
                .map_err(|error| err(format!("withdrawal {}: {error:?}", hex(&nullifier_hash))))?;
            nullifiers.insert(nullifier_hash);
            items.push(Item { fee, root, nullifier_hash, proof: &request[72..], nullifier_proof });
        }
        state.deposit(&commitments).map_err(|error| err(format!("deposit: {error:?}")))?;

        let capacity = (self.capacity + commitments.len() as u64 * self.config.denomination)
            .checked_sub(items.len() as u64 * self.config.denomination)
            .ok_or_else(|| err("the pool does not hold that many notes"))?;
        let withdrawals: Vec<Withdraw<'_>> = items
            .iter()
            .map(|item| Withdraw {
                fee: item.fee,
                root: item.root,
                nullifier_hash: item.nullifier_hash,
                proof: item.proof,
                nullifier_proof: &item.nullifier_proof,
            })
            .collect();
        let action = core::encode_batch(&withdrawals).ok_or_else(|| err("the batch does not fit the encoding"))?;
        Ok(json!({ "action": hex(&action), "state": hex(&state.to_bytes()), "capacity": capacity.to_string() }).to_string())
    }

    /// Everything a withdrawal of the note at `leaf_index` needs before it is
    /// proved: the input of the withdraw circuit (`input`, for snarkjs), and
    /// the `root` and `nullifier_hash` the proof will be bound to, with the
    /// public signals snarkjs has to report (`public`).
    ///
    /// Refuses a note whose secrets do not give that leaf, and one that is
    /// withdrawn already.
    pub fn withdraw_input(&self, leaf_index: u32, nullifier: &[u8], secret: &[u8], recipient_lock_hash: &[u8], fee: u64) -> Res<String> {
        let (nullifier, secret) = (field(nullifier, "nullifier")?, field(secret, "secret")?);
        let leaf = self.leaves.get(leaf_index as usize).ok_or_else(|| err("there is no such leaf"))?;
        if hash2(&nullifier, &secret) != *leaf {
            return Err(err("the note's secrets do not give this leaf"));
        }
        let nullifier_hash = hash2(&nullifier, &Fr::from(leaf_index));
        if self.spent.contains(&fr_to_le_bytes(&nullifier_hash)) {
            return Err(err("this note is withdrawn already"));
        }
        if fee > self.config.denomination {
            return Err(err("the fee is more than the denomination"));
        }
        let recipient = recipient_field(&bytes32(recipient_lock_hash, "recipient lock hash")?);
        let recipient = fr_from_le_bytes(&recipient).expect("31 bytes fit the field");

        let witness = WithdrawWitness::for_leaf(&self.leaves, leaf_index, nullifier, secret, nullifier_hash, recipient, fee);
        if witness.root != fr_from_le_bytes(self.state.current_root()).expect("a root is canonical") {
            return Err(err("the leaves kept here do not give the pool's root"));
        }
        let decimal = |x: &Fr| x.into_bigint().to_string();
        Ok(json!({
            "input": witness.to_json(),
            "public": [decimal(&witness.root), decimal(&nullifier_hash), decimal(&recipient), fee.to_string()],
            "root": hex(&fr_to_le_bytes(&witness.root)),
            "nullifier_hash": hex(&fr_to_le_bytes(&nullifier_hash)),
        })
        .to_string())
    }
}
