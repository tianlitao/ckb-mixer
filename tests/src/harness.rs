//! A mock chain (ckb-testtool) with the mixer scripts deployed, and an
//! off-chain mirror of a pool, kept the way a wallet or indexer would keep it.
//!
//! Transactions are built from explicit parameter structs. The `*_params`
//! functions return what an honest client would send; tests change single
//! fields to play an attacker.

use std::path::{Path, PathBuf};

use ckb_testtool::builtin::ALWAYS_SUCCESS;
use ckb_testtool::ckb_error::Error as CkbError;
use ckb_testtool::ckb_types::{
    bytes::Bytes,
    core::{
        tx_pool::get_transaction_weight, Capacity, Cycle, ScriptHashType, TransactionBuilder,
        TransactionView,
    },
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use ckb_testtool::context::Context;
use mixer_core::action::{self, Withdraw};
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes, recipient_field};
use mixer_core::poseidon::hash2;
use mixer_core::smt::NullifierSet;
use mixer_core::state::PoolState;
use mixer_core::{Bytes32, Fr};

use crate::snark::{self, WithdrawWitness};

pub const CKB: u64 = 100_000_000;
/// Denomination of the test pool.
pub const DENOMINATION: u64 = 1_000 * CKB;
/// Mainnet's `max_block_cycles`; no transaction can use more.
pub const MAX_CYCLES: u64 = 3_500_000_000;
/// Miner fee of the transactions that are funded by a user cell.
pub const TX_FEE: u64 = 10_000;

/// Exit code of the pool lock when it guards a cell without the pool type script.
pub const POOL_LOCK_NOT_A_POOL_CELL: i8 = 103;

/// Where the contract binaries of a build variant end up, see
/// `scripts/build-contracts.sh`.
pub fn contracts_dir(variant: &str) -> PathBuf {
    let target_dir = match variant {
        "default" => "target".to_string(),
        other => format!("target-{other}"),
    };
    snark::repo_root()
        .join("contracts")
        .join(target_dir)
        .join("riscv64imac-unknown-none-elf/release")
}

fn read_contract(dir: &Path, name: &str) -> Bytes {
    let path = dir.join(name);
    std::fs::read(&path)
        .unwrap_or_else(|_| panic!("{} is missing; run `make contracts` first", path.display()))
        .into()
}

pub fn script_hash(script: &Script) -> Bytes32 {
    script.calc_script_hash().as_slice().try_into().expect("32 bytes")
}

pub fn capacity_of(cell: &CellOutput) -> u64 {
    cell.capacity().unpack()
}

/// Serialized size of a transaction.
pub fn tx_size(tx: &TransactionView) -> usize {
    tx.data().as_slice().len()
}

/// What the transaction pool charges the fee rate on: the size, or the cycles
/// converted to bytes at the block's bytes-per-cycle ratio, whichever is larger.
pub fn tx_weight(tx: &TransactionView, cycles: Cycle) -> u64 {
    get_transaction_weight(tx_size(tx), cycles)
}

/// A plain cell: no type script, no data.
pub fn plain_cell(lock: &Script, capacity: u64) -> CellOutput {
    CellOutput::new_builder().capacity(capacity).lock(lock.clone()).build()
}

/// Assert that a script rejected the transaction with exit code `code`.
pub fn assert_exit_code(result: Result<Cycle, CkbError>, code: i8) {
    let error = result.expect_err("the transaction should have been rejected").to_string();
    assert!(
        error.contains(&format!("error code {code} ")),
        "expected exit code {code}, got: {error}"
    );
}

/// Assert that the pool type script rejected the transaction for `reason`.
pub fn assert_rejected(result: Result<Cycle, CkbError>, reason: mixer_core::Error) {
    assert_exit_code(result, reason.code());
}

/// Assert that the pool type script rejected the transaction for one of
/// `reasons`, for inputs where more than one is a correct answer.
pub fn assert_rejected_one_of(result: Result<Cycle, CkbError>, reasons: &[mixer_core::Error]) {
    let error = result.expect_err("the transaction should have been rejected").to_string();
    assert!(
        reasons.iter().any(|reason| error.contains(&format!("error code {} ", reason.code()))),
        "expected one of {reasons:?}, got: {error}"
    );
}

/// The mock chain.
pub struct Env {
    pub context: Context,
    pool_type_code: OutPoint,
    pool_lock_code: OutPoint,
    always_success_code: OutPoint,
    /// The withdraw circuit's verifying key, in on-chain encoding.
    pub vk: Vec<u8>,
    pub vk_cell: OutPoint,
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    /// A chain with the default build of the contracts, or with the build in
    /// `MIXER_CONTRACTS_DIR` if that is set.
    pub fn new() -> Self {
        match std::env::var_os("MIXER_CONTRACTS_DIR") {
            Some(dir) => Self::with_contracts(Path::new(&dir)),
            None => Self::with_contracts(&contracts_dir("default")),
        }
    }

    /// A chain with the contract binaries found in `dir`.
    pub fn with_contracts(dir: &Path) -> Self {
        let mut context = Context::default();
        let pool_type_code = context.deploy_cell(read_contract(dir, "pool-type"));
        let pool_lock_code = context.deploy_cell(read_contract(dir, "pool-lock"));
        let always_success_code = context.deploy_cell(ALWAYS_SUCCESS.clone());
        let vk = snark::verifying_key();
        let vk_cell = context.deploy_cell(vk.clone().into());
        Self { context, pool_type_code, pool_lock_code, always_success_code, vk, vk_cell }
    }

    /// A lock that anyone can unlock. `owner` only makes the lock hash differ
    /// from user to user; signatures are not what these tests are about.
    pub fn user_lock(&mut self, owner: u8) -> Script {
        self.context
            .build_script(&self.always_success_code, Bytes::from(vec![owner]))
            .expect("always-success is deployed")
    }

    /// Give `lock` a live cell with `capacity` shannons.
    pub fn fund(&mut self, lock: &Script, capacity: u64) -> OutPoint {
        self.context.create_cell(plain_cell(lock, capacity), Bytes::new())
    }

    pub fn vk_dep(&self) -> CellDep {
        CellDep::new_builder().out_point(self.vk_cell.clone()).build()
    }

    /// Type and lock script of a pool with the given denomination.
    ///
    /// Both refer to their code by data hash, so nobody can swap the code under
    /// a live pool.
    pub fn pool_scripts(&mut self, denomination: u64) -> (Script, Script) {
        self.pool_scripts_with(denomination, ScriptHashType::Data2)
    }

    /// Like [`Env::pool_scripts`], with the pool lock referenced through
    /// `lock_hash_type`. The lock's hash type is part of the type script's
    /// args, so this gives a second, independent pool type with the same
    /// denomination and the same circuit.
    pub fn pool_scripts_with(&mut self, denomination: u64, lock_hash_type: ScriptHashType) -> (Script, Script) {
        let (_, lock_code) = self.context.get_cell(&self.pool_lock_code).expect("deployed");

        let mut args = Vec::with_capacity(73);
        args.extend_from_slice(&denomination.to_le_bytes());
        args.extend_from_slice(CellOutput::calc_data_hash(&self.vk).as_slice());
        args.extend_from_slice(CellOutput::calc_data_hash(&lock_code).as_slice());
        args.push(lock_hash_type.into());

        let type_script = self
            .context
            .build_script_with_hash_type(&self.pool_type_code, ScriptHashType::Data2, args.into())
            .expect("pool-type is deployed");
        let lock_script = self
            .context
            .build_script_with_hash_type(
                &self.pool_lock_code,
                lock_hash_type,
                type_script.calc_script_hash().as_bytes(),
            )
            .expect("pool-lock is deployed");
        (type_script, lock_script)
    }

    /// A transaction that creates a pool cell holding `data`.
    pub fn create_pool_tx(&mut self, type_script: &Script, lock_script: &Script, data: Vec<u8>) -> TransactionView {
        let pool = CellOutput::new_builder()
            .lock(lock_script.clone())
            .type_(Some(type_script.clone()).pack())
            .build_exact_capacity(Capacity::bytes(data.len()).expect("capacity"))
            .expect("capacity");
        let creator = self.user_lock(0xc0);
        let funding = self.fund(&creator, capacity_of(&pool) + 100 * CKB);

        let tx = TransactionBuilder::default()
            .input(CellInput::new(funding, 0))
            .output(pool)
            .output_data(Bytes::from(data).pack())
            .output(plain_cell(&creator, 100 * CKB - TX_FEE))
            .output_data(Bytes::new().pack())
            .build();
        self.context.complete_tx(tx)
    }

    /// Run every script of the transaction. Returns the cycles consumed.
    pub fn verify(&self, tx: &TransactionView) -> Result<Cycle, CkbError> {
        self.assert_capacity_rules(tx);
        self.context.verify_tx(tx, MAX_CYCLES)
    }

    /// Like [`Env::verify`] for an accepted transaction, and also return the
    /// cycle checkpoints `(stage, cycles so far)` that a `profile` build of the
    /// pool type script prints. Other builds print none.
    pub fn verify_profiled(&mut self, tx: &TransactionView) -> (Cycle, Vec<(String, u64)>) {
        self.context.set_capture_debug(true);
        let seen = self.context.captured_messages().len();
        let cycles = self.verify(tx).expect("transaction is accepted");
        let checkpoints = self.context.captured_messages()[seen..]
            .iter()
            .filter_map(|message| {
                let mut words = message.message.split_whitespace();
                match (words.next(), words.next(), words.next()) {
                    (Some("profile"), Some(stage), Some(cycles)) => {
                        Some((stage.to_string(), cycles.parse().ok()?))
                    }
                    _ => None,
                }
            })
            .collect();
        (cycles, checkpoints)
    }

    /// Make the outputs of an accepted transaction spendable.
    pub fn commit(&mut self, tx: &TransactionView) {
        for (index, (cell, data)) in tx.outputs_with_data_iter().enumerate() {
            self.context
                .create_cell_with_out_point(OutPoint::new(tx.hash(), index as u32), cell, data);
        }
    }

    /// The capacity rules that consensus enforces outside of scripts and that
    /// ckb-testtool does not check: no output below its occupied capacity, and
    /// no more capacity out than in. A test transaction that breaks them would
    /// be rejected on chain for a reason the test is not about, so that is a bug
    /// in the test.
    fn assert_capacity_rules(&self, tx: &TransactionView) {
        let mut inputs = 0u64;
        for out_point in tx.input_pts_iter() {
            let (cell, _) = self.context.get_cell(&out_point).expect("input is a known cell");
            inputs += capacity_of(&cell);
        }
        let mut outputs = 0u64;
        for (index, (cell, data)) in tx.outputs_with_data_iter().enumerate() {
            let occupied = cell
                .occupied_capacity(Capacity::bytes(data.len()).expect("capacity"))
                .expect("capacity")
                .as_u64();
            let capacity = capacity_of(&cell);
            assert!(capacity >= occupied, "output {index}: {capacity} shannons, occupies {occupied}");
            outputs += capacity;
        }
        assert!(outputs <= inputs, "outputs hold {outputs} shannons, inputs only {inputs}");
    }
}

/// A deposit note: the two secrets behind a commitment.
#[derive(Debug, Clone, Copy)]
pub struct Note {
    pub nullifier: Fr,
    pub secret: Fr,
}

impl Note {
    /// A deterministic note for tests. A real note is two fresh random field
    /// elements.
    pub fn from_seed(seed: u64) -> Self {
        Self {
            nullifier: hash2(&Fr::from(seed), &Fr::from(1u64)),
            secret: hash2(&Fr::from(seed), &Fr::from(2u64)),
        }
    }

    pub fn commitment(&self) -> Fr {
        hash2(&self.nullifier, &self.secret)
    }

    /// The nullifier hash of this note when it sits at `leaf_index`.
    pub fn nullifier_hash(&self, leaf_index: u32) -> Fr {
        hash2(&self.nullifier, &Fr::from(leaf_index))
    }
}

/// A withdrawal proof together with the public values it is bound to.
#[derive(Debug, Clone)]
pub struct WithdrawProof {
    pub root: Bytes32,
    pub nullifier_hash: Bytes32,
    pub recipient_lock: Script,
    pub fee: u64,
    pub proof: Vec<u8>,
}

/// Prove that `note` is leaf `leaf_index` of the tree made of `leaves`, for a
/// payout to `recipient_lock`. The tree does not have to be a pool's.
pub fn prove_note(
    vk: &[u8],
    leaves: &[Fr],
    note: &Note,
    leaf_index: u32,
    recipient_lock: &Script,
    fee: u64,
) -> WithdrawProof {
    let lock_hash = script_hash(recipient_lock);
    let recipient = fr_from_le_bytes(&recipient_field(&lock_hash)).expect("31 bytes fit the field");
    let nullifier_hash = note.nullifier_hash(leaf_index);
    let witness = WithdrawWitness::for_leaf(
        leaves,
        leaf_index,
        note.nullifier,
        note.secret,
        nullifier_hash,
        recipient,
        fee,
    );
    let proof = snark::prove(&witness).expect("the prover accepts an honest witness");

    let result = WithdrawProof {
        root: fr_to_le_bytes(&witness.root),
        nullifier_hash: fr_to_le_bytes(&nullifier_hash),
        recipient_lock: recipient_lock.clone(),
        fee,
        proof,
    };
    // The converted proof has to satisfy the verifier that is linked into the
    // pool type script.
    let public_inputs = action::public_inputs(&result.root, &result.nullifier_hash, &lock_hash, fee);
    verifier_core::verify(vk, &result.proof, &public_inputs).expect("converted proof verifies off chain");
    result
}

/// Everything a deposit transaction is made of.
#[derive(Debug, Clone)]
pub struct DepositParams {
    pub commitments: Vec<Bytes32>,
    /// Data and capacity of the pool cell the transaction creates.
    pub pool_data: Vec<u8>,
    pub pool_capacity: u64,
}

/// Everything a withdrawal transaction is made of.
#[derive(Debug, Clone)]
pub struct WithdrawParams {
    pub fee: u64,
    pub root: Bytes32,
    pub nullifier_hash: Bytes32,
    pub proof: Vec<u8>,
    pub nullifier_proof: Vec<u8>,
    /// Lock, data and capacity of the pool cell the transaction creates.
    pub pool_lock: Script,
    pub pool_data: Vec<u8>,
    pub pool_capacity: u64,
    /// The output right after the pool cell.
    pub recipient: Option<(CellOutput, Bytes)>,
    /// Outputs after the recipient.
    pub extra_outputs: Vec<(CellOutput, Bytes)>,
    pub vk_dep: Option<CellDep>,
}

impl WithdrawParams {
    /// The witness of the pool input: the withdraw action in `input_type`.
    pub fn witness(&self) -> Bytes {
        let action = action::encode_withdraw(&Withdraw {
            fee: self.fee,
            root: self.root,
            nullifier_hash: self.nullifier_hash,
            proof: &self.proof,
            nullifier_proof: &self.nullifier_proof,
        });
        WitnessArgs::new_builder().input_type(Some(Bytes::from(action)).pack()).build().as_bytes()
    }
}

/// Off-chain mirror of one pool.
pub struct Pool {
    pub denomination: u64,
    pub type_script: Script,
    pub lock_script: Script,
    /// The live pool cell.
    pub cell: OutPoint,
    pub capacity: u64,
    pub state: PoolState,
    /// Every commitment deposited so far, in tree order.
    pub leaves: Vec<Fr>,
    pub nullifiers: NullifierSet,
}

impl Pool {
    /// Create a pool on chain. Returns the pool and the cycles of the creation.
    pub fn create(env: &mut Env, denomination: u64) -> (Self, Cycle) {
        Self::create_with(env, denomination, ScriptHashType::Data2)
    }

    /// Like [`Pool::create`], see [`Env::pool_scripts_with`].
    pub fn create_with(env: &mut Env, denomination: u64, lock_hash_type: ScriptHashType) -> (Self, Cycle) {
        let (type_script, lock_script) = env.pool_scripts_with(denomination, lock_hash_type);
        let state = PoolState::empty();
        let tx = env.create_pool_tx(&type_script, &lock_script, state.to_bytes());
        let cycles = env.verify(&tx).expect("pool creation is accepted");
        env.commit(&tx);

        let capacity = capacity_of(&tx.output(0).expect("pool output"));
        let pool = Self {
            denomination,
            type_script,
            lock_script,
            cell: OutPoint::new(tx.hash(), 0),
            capacity,
            state,
            leaves: Vec::new(),
            nullifiers: NullifierSet::new(),
        };
        (pool, cycles)
    }

    /// A cell with this pool's type script.
    pub fn pool_output(&self, lock: &Script, capacity: u64) -> CellOutput {
        CellOutput::new_builder()
            .capacity(capacity)
            .lock(lock.clone())
            .type_(Some(self.type_script.clone()).pack())
            .build()
    }

    // ---- deposit

    /// An honest deposit of `commitments`.
    pub fn deposit_params(&self, commitments: &[Fr]) -> DepositParams {
        let commitments: Vec<Bytes32> = commitments.iter().map(fr_to_le_bytes).collect();
        let mut state = self.state.clone();
        state.deposit(&commitments.concat()).expect("valid deposit");
        DepositParams {
            pool_data: state.to_bytes(),
            pool_capacity: self.capacity + commitments.len() as u64 * self.denomination,
            commitments,
        }
    }

    pub fn deposit_tx(&self, env: &mut Env, params: &DepositParams) -> TransactionView {
        let depositor = env.user_lock(0xd0);
        let added = params.pool_capacity.saturating_sub(self.capacity);
        let funding_capacity = added + 100 * CKB;
        let funding = env.fund(&depositor, funding_capacity);
        let change = self.capacity + funding_capacity - params.pool_capacity - TX_FEE;

        let witness = WitnessArgs::new_builder()
            .input_type(Some(Bytes::from(action::encode_deposit(&params.commitments))).pack())
            .build();
        let tx = TransactionBuilder::default()
            .input(CellInput::new(self.cell.clone(), 0))
            .input(CellInput::new(funding, 0))
            .output(self.pool_output(&self.lock_script, params.pool_capacity))
            .output_data(Bytes::from(params.pool_data.clone()).pack())
            .output(plain_cell(&depositor, change))
            .output_data(Bytes::new().pack())
            .witness(witness.as_bytes().pack())
            .build();
        env.context.complete_tx(tx)
    }

    /// Record a deposit that the chain accepted.
    pub fn apply_deposit(&mut self, env: &mut Env, tx: &TransactionView, params: &DepositParams) {
        env.commit(tx);
        self.cell = OutPoint::new(tx.hash(), 0);
        self.capacity = params.pool_capacity;
        self.state = PoolState::from_bytes(&params.pool_data).expect("accepted state is well formed");
        for commitment in &params.commitments {
            self.leaves.push(fr_from_le_bytes(commitment).expect("accepted commitment is canonical"));
        }
    }

    /// Deposit `commitments` in one transaction. Returns its cycles.
    pub fn deposit(&mut self, env: &mut Env, commitments: &[Fr]) -> Cycle {
        let params = self.deposit_params(commitments);
        let tx = self.deposit_tx(env, &params);
        let cycles = env.verify(&tx).expect("deposit is accepted");
        self.apply_deposit(env, &tx, &params);
        cycles
    }

    // ---- withdraw

    /// Prove ownership of the note at `leaf_index` against the current root.
    pub fn prove_withdraw(
        &self,
        env: &Env,
        note: &Note,
        leaf_index: u32,
        recipient_lock: &Script,
        fee: u64,
    ) -> WithdrawProof {
        prove_note(&env.vk, &self.leaves, note, leaf_index, recipient_lock, fee)
    }

    /// An honest withdrawal transaction for `proof`.
    ///
    /// If the pool state does not allow the withdrawal (spent note, root no
    /// longer known), the pool data is left unchanged; the chain rejects such a
    /// transaction before it looks at the output.
    pub fn withdraw_params(&self, env: &Env, proof: &WithdrawProof) -> WithdrawParams {
        let nullifier_proof = self.nullifiers.proof(&proof.nullifier_hash);
        let mut state = self.state.clone();
        let _ = state.withdraw(&proof.root, &proof.nullifier_hash, &nullifier_proof);

        WithdrawParams {
            fee: proof.fee,
            root: proof.root,
            nullifier_hash: proof.nullifier_hash,
            proof: proof.proof.clone(),
            nullifier_proof,
            pool_lock: self.lock_script.clone(),
            pool_data: state.to_bytes(),
            pool_capacity: self.capacity - self.denomination,
            recipient: Some((
                plain_cell(&proof.recipient_lock, self.denomination - proof.fee),
                Bytes::new(),
            )),
            extra_outputs: Vec::new(),
            vk_dep: Some(env.vk_dep()),
        }
    }

    /// The only input is the pool cell: nobody signs a withdrawal, and its fee
    /// comes out of the withdrawn amount.
    pub fn withdraw_tx(&self, env: &mut Env, params: &WithdrawParams) -> TransactionView {
        let mut builder = TransactionBuilder::default()
            .input(CellInput::new(self.cell.clone(), 0))
            .output(self.pool_output(&params.pool_lock, params.pool_capacity))
            .output_data(Bytes::from(params.pool_data.clone()).pack())
            .witness(params.witness().pack());
        for (cell, data) in params.recipient.iter().chain(params.extra_outputs.iter()) {
            builder = builder.output(cell.clone()).output_data(data.pack());
        }
        if let Some(dep) = &params.vk_dep {
            builder = builder.cell_dep(dep.clone());
        }
        env.context.complete_tx(builder.build())
    }

    /// Record a withdrawal that the chain accepted.
    pub fn apply_withdraw(&mut self, env: &mut Env, tx: &TransactionView, params: &WithdrawParams) {
        env.commit(tx);
        self.cell = OutPoint::new(tx.hash(), 0);
        self.capacity = params.pool_capacity;
        self.state = PoolState::from_bytes(&params.pool_data).expect("accepted state is well formed");
        assert!(self.nullifiers.insert(params.nullifier_hash), "the chain accepted a spent nullifier");
        assert_eq!(self.nullifiers.root(), self.state.nullifier_root);
    }

    /// Withdraw with `proof`. Returns the transaction and its cycles.
    pub fn withdraw(&mut self, env: &mut Env, proof: &WithdrawProof) -> (TransactionView, Cycle) {
        let params = self.withdraw_params(env, proof);
        let tx = self.withdraw_tx(env, &params);
        let cycles = env.verify(&tx).expect("withdrawal is accepted");
        self.apply_withdraw(env, &tx, &params);
        (tx, cycles)
    }
}
