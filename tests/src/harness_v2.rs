//! Pool v2 on the mock chain: intent cells, batches, and an off-chain mirror of
//! a pool, kept the way an aggregator would keep it.
//!
//! As in [`crate::harness`], transactions are built from explicit parameter
//! structs: the `*_params` functions return what an honest party would send,
//! and tests change single fields to play an attacker.

use ckb_testtool::ckb_types::{
    bytes::Bytes,
    core::{Capacity, Cycle, ScriptHashType, TransactionBuilder, TransactionView},
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use mixer_core::action::Withdraw;
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes};
use mixer_core::merkle;
use mixer_core::smt::NullifierSet;
use mixer_core::{Bytes32, Fr};
use mixer_core_v2::{self as v2, IntentArgs, IntentError, PoolConfig, PoolState, ScriptId};

use crate::harness::{
    assert_exit_code, capacity_of, plain_cell, prove_note, script_hash, Env, Note, WithdrawProof,
    CKB, TX_FEE,
};

/// Root history of the test pools: small, so tests can run past it.
pub const ROOT_HISTORY: u16 = 8;
/// What the test intents hold on top of the denomination.
pub const TIP: u64 = CKB;
/// Blocks the owner of a test intent waits before taking it back.
pub const RECLAIM_AFTER: u64 = 100;
/// What the aggregator of an honest batch brings to pay the miner from.
const AGGREGATOR_FUNDS: u64 = 100 * CKB;

/// Assert that the intent lock rejected the transaction for `reason`.
pub fn assert_intent_rejected(result: Result<Cycle, ckb_testtool::ckb_error::Error>, reason: IntentError) {
    assert_exit_code(result, reason.code());
}

/// The choices made when a pool is created.
#[derive(Debug, Clone)]
pub struct PoolSetup {
    pub denomination: u64,
    pub root_history: u16,
    pub pool_lock_hash_type: ScriptHashType,
    pub intent_lock_hash_type: ScriptHashType,
}

impl PoolSetup {
    /// Both locks referenced by data hash, so nobody can swap their code.
    pub fn new(denomination: u64) -> Self {
        Self {
            denomination,
            root_history: ROOT_HISTORY,
            pool_lock_hash_type: ScriptHashType::Data2,
            intent_lock_hash_type: ScriptHashType::Data2,
        }
    }
}

/// Everything a pool creation transaction is made of.
#[derive(Debug, Clone)]
pub struct CreateParams {
    /// The transaction's first input.
    pub funding: OutPoint,
    /// The pool type script's args.
    pub config: PoolConfig,
    /// Hash type the pool cell's lock is referenced by.
    pub pool_lock_hash_type: ScriptHashType,
    pub data: Vec<u8>,
    /// Number of plain outputs placed before the pool cell.
    pub outputs_before: usize,
}

impl Env {
    fn script_id(&mut self, code: &OutPoint, hash_type: ScriptHashType) -> ScriptId {
        let script = self
            .context
            .build_script_with_hash_type(code, hash_type, Bytes::new())
            .expect("the script is deployed");
        ScriptId {
            code_hash: script.code_hash().as_slice().try_into().expect("32 bytes"),
            hash_type: hash_type.into(),
        }
    }

    /// The intent lock, referenced through `hash_type`, with arbitrary args.
    pub fn intent_lock_with(&mut self, hash_type: ScriptHashType, args: Vec<u8>) -> Script {
        let code = self.intent_lock_code.clone();
        self.context
            .build_script_with_hash_type(&code, hash_type, args.into())
            .expect("intent-lock is deployed")
    }

    /// An honest creation: the pool cell is output 0 and its Type ID comes from
    /// a freshly funded first input.
    pub fn v2_create_params(&mut self, setup: &PoolSetup) -> CreateParams {
        let data = PoolState::empty(setup.root_history).to_bytes();
        let creator = self.user_lock(0xc0);
        // Enough for any pool cell these tests create, plus change.
        let funding = self.fund(&creator, 20_000 * CKB);
        let first_input = CellInput::new(funding.clone(), 0);

        let pool_lock_code = self.pool_lock_code.clone();
        let intent_lock_code = self.intent_lock_code.clone();
        let config = PoolConfig {
            type_id: v2::type_id(first_input.as_slice(), 0),
            denomination: setup.denomination,
            vk_hash: CellOutput::calc_data_hash(&self.vk).as_slice().try_into().expect("32 bytes"),
            pool_lock: self.script_id(&pool_lock_code, setup.pool_lock_hash_type),
            intent_lock: self.script_id(&intent_lock_code, setup.intent_lock_hash_type),
            root_history: setup.root_history,
        };
        CreateParams {
            funding,
            config,
            pool_lock_hash_type: setup.pool_lock_hash_type,
            data,
            outputs_before: 0,
        }
    }

    /// Type and lock script of the pool with these args.
    pub fn v2_scripts(&mut self, config_args: Vec<u8>, pool_lock_hash_type: ScriptHashType) -> (Script, Script) {
        let type_code = self.pool_type_v2_code.clone();
        let lock_code = self.pool_lock_code.clone();
        let type_script = self
            .context
            .build_script_with_hash_type(&type_code, ScriptHashType::Data2, config_args.into())
            .expect("pool-type-v2 is deployed");
        let lock_script = self
            .context
            .build_script_with_hash_type(&lock_code, pool_lock_hash_type, type_script.calc_script_hash().as_bytes())
            .expect("pool-lock is deployed");
        (type_script, lock_script)
    }

    pub fn v2_create_tx(&mut self, params: &CreateParams) -> TransactionView {
        let (type_script, lock_script) = self.v2_scripts(params.config.to_bytes(), params.pool_lock_hash_type);
        self.v2_create_tx_with(params, &type_script, &lock_script)
    }

    /// Like [`Env::v2_create_tx`], with the pool cell's scripts given explicitly.
    pub fn v2_create_tx_with(
        &mut self,
        params: &CreateParams,
        type_script: &Script,
        lock_script: &Script,
    ) -> TransactionView {
        let pool = CellOutput::new_builder()
            .lock(lock_script.clone())
            .type_(Some(type_script.clone()).pack())
            .build_exact_capacity(Capacity::bytes(params.data.len()).expect("capacity"))
            .expect("capacity");
        let creator = self.user_lock(0xc0);
        let (funding_cell, _) = self.context.get_cell(&params.funding).expect("funding cell");
        let filler = 100 * CKB;
        let change = capacity_of(&funding_cell)
            - capacity_of(&pool)
            - filler * params.outputs_before as u64
            - TX_FEE;

        let mut builder = TransactionBuilder::default().input(CellInput::new(params.funding.clone(), 0));
        for _ in 0..params.outputs_before {
            builder = builder.output(plain_cell(&creator, filler)).output_data(Bytes::new().pack());
        }
        let tx = builder
            .output(pool)
            .output_data(Bytes::from(params.data.clone()).pack())
            .output(plain_cell(&creator, change))
            .output_data(Bytes::new().pack())
            .build();
        self.context.complete_tx(tx)
    }
}

/// One withdrawal of a batch, as it goes into the witness.
#[derive(Debug, Clone)]
pub struct WithdrawEntry {
    pub fee: u64,
    pub root: Bytes32,
    pub nullifier_hash: Bytes32,
    pub proof: Vec<u8>,
    pub nullifier_proof: Vec<u8>,
}

/// Everything a batch transaction is made of. The pool cell is input 0 and
/// output 0.
#[derive(Debug, Clone)]
pub struct BatchParams {
    /// Inputs after the pool cell: the intents, then the aggregator's own.
    pub inputs: Vec<CellInput>,
    pub withdrawals: Vec<WithdrawEntry>,
    /// Lock, data and capacity of the pool cell the transaction creates.
    pub pool_lock: Script,
    pub pool_data: Vec<u8>,
    pub pool_capacity: u64,
    /// Outputs after the pool cell: one recipient per withdrawal, in order,
    /// then the aggregator's own.
    pub outputs: Vec<(CellOutput, Bytes)>,
    pub vk_dep: Option<CellDep>,
    /// The commitments the batch adds to the tree, in order.
    pub deposits: Vec<Bytes32>,
}

impl BatchParams {
    /// The batch action.
    pub fn action(&self) -> Vec<u8> {
        let withdrawals: Vec<Withdraw<'_>> = self
            .withdrawals
            .iter()
            .map(|entry| Withdraw {
                fee: entry.fee,
                root: entry.root,
                nullifier_hash: entry.nullifier_hash,
                proof: &entry.proof,
                nullifier_proof: &entry.nullifier_proof,
            })
            .collect();
        v2::encode_batch(&withdrawals).expect("the batch fits the encoding")
    }

    /// The witness of the pool input: the batch action in `input_type`.
    pub fn witness(&self) -> Bytes {
        action_witness(self.action())
    }
}

pub fn action_witness(action: Vec<u8>) -> Bytes {
    WitnessArgs::new_builder().input_type(Some(Bytes::from(action)).pack()).build().as_bytes()
}

/// Off-chain mirror of one v2 pool.
pub struct PoolV2 {
    pub config: PoolConfig,
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

impl PoolV2 {
    /// Create a pool on chain. Returns the pool and the cycles of the creation.
    pub fn create(env: &mut Env, setup: &PoolSetup) -> (Self, Cycle) {
        let params = env.v2_create_params(setup);
        let (type_script, lock_script) = env.v2_scripts(params.config.to_bytes(), params.pool_lock_hash_type);
        let tx = env.v2_create_tx(&params);
        let cycles = env.verify(&tx).expect("pool creation is accepted");
        env.commit(&tx);

        let pool = Self {
            capacity: capacity_of(&tx.output(0).expect("pool output")),
            state: PoolState::empty(params.config.root_history),
            config: params.config,
            type_script,
            lock_script,
            cell: OutPoint::new(tx.hash(), 0),
            leaves: Vec::new(),
            nullifiers: NullifierSet::new(),
        };
        (pool, cycles)
    }

    pub fn denomination(&self) -> u64 {
        self.config.denomination
    }

    pub fn type_hash(&self) -> Bytes32 {
        script_hash(&self.type_script)
    }

    /// A cell with this pool's type script.
    pub fn pool_output(&self, lock: &Script, capacity: u64) -> CellOutput {
        CellOutput::new_builder()
            .capacity(capacity)
            .lock(lock.clone())
            .type_(Some(self.type_script.clone()).pack())
            .build()
    }

    // ---- intents

    /// Lock args of an intent to deposit `commitment` into this pool.
    pub fn intent_args(&self, commitment: &Fr, owner: &Script) -> IntentArgs {
        IntentArgs {
            pool_type_hash: self.type_hash(),
            commitment: fr_to_le_bytes(commitment),
            owner_lock_hash: script_hash(owner),
            reclaim_since: RECLAIM_AFTER,
        }
    }

    /// The intent lock as this pool's args name it.
    pub fn intent_lock(&self, env: &mut Env, args: &IntentArgs) -> Script {
        let hash_type = ScriptHashType::try_from(self.config.intent_lock.hash_type).expect("hash type");
        env.intent_lock_with(hash_type, args.to_bytes())
    }

    /// Put an intent cell on chain, as its owner would: the denomination plus a
    /// tip, under the intent lock.
    pub fn place_intent(&self, env: &mut Env, commitment: &Fr, owner: &Script) -> OutPoint {
        let lock = self.intent_lock(env, &self.intent_args(commitment, owner));
        env.fund(&lock, self.denomination() + TIP)
    }

    // ---- batches

    /// An honest batch that deposits `intents` and pays out `proofs`.
    ///
    /// The aggregator adds one input of its own and collects the tips and fees
    /// in one output, after the recipients. Where the pool state does not allow
    /// a step (spent note, unknown root, bad commitment), the step is skipped
    /// in the expected state; the chain rejects such a transaction before it
    /// looks at the output.
    pub fn batch_params(&self, env: &mut Env, intents: &[OutPoint], proofs: &[WithdrawProof]) -> BatchParams {
        let denomination = self.denomination();
        let mut state = self.state.clone();
        let mut nullifiers = self.nullifiers.clone();
        let mut collected = AGGREGATOR_FUNDS - TX_FEE;

        let mut withdrawals = Vec::new();
        let mut outputs = Vec::new();
        for proof in proofs {
            let nullifier_proof = nullifiers.proof(&proof.nullifier_hash);
            if state.withdraw(&proof.root, &proof.nullifier_hash, &nullifier_proof).is_ok() {
                nullifiers.insert(proof.nullifier_hash);
            }
            withdrawals.push(WithdrawEntry {
                fee: proof.fee,
                root: proof.root,
                nullifier_hash: proof.nullifier_hash,
                proof: proof.proof.clone(),
                nullifier_proof,
            });
            outputs.push((plain_cell(&proof.recipient_lock, denomination - proof.fee), Bytes::new()));
            collected += proof.fee;
        }

        let mut inputs = Vec::new();
        let mut deposits = Vec::new();
        for intent in intents {
            let (cell, _) = env.context.get_cell(intent).expect("the intent cell is live");
            let args = IntentArgs::parse(&cell.lock().args().raw_data()).expect("intent lock args");
            deposits.push(args.commitment);
            inputs.push(CellInput::new(intent.clone(), 0));
            collected = collected + capacity_of(&cell) - denomination;
        }
        let _ = state.deposit(&deposits);

        let aggregator = env.user_lock(0xa9);
        inputs.push(CellInput::new(env.fund(&aggregator, AGGREGATOR_FUNDS), 0));
        outputs.push((plain_cell(&aggregator, collected), Bytes::new()));

        BatchParams {
            inputs,
            withdrawals,
            pool_lock: self.lock_script.clone(),
            pool_data: state.to_bytes(),
            pool_capacity: self.capacity + deposits.len() as u64 * denomination
                - proofs.len() as u64 * denomination,
            outputs,
            vk_dep: Some(env.vk_dep()),
            deposits,
        }
    }

    pub fn batch_tx(&self, env: &mut Env, params: &BatchParams) -> TransactionView {
        let mut builder = TransactionBuilder::default()
            .input(CellInput::new(self.cell.clone(), 0))
            .inputs(params.inputs.clone())
            .output(self.pool_output(&params.pool_lock, params.pool_capacity))
            .output_data(Bytes::from(params.pool_data.clone()).pack())
            .witness(params.witness().pack());
        for (cell, data) in &params.outputs {
            builder = builder.output(cell.clone()).output_data(data.pack());
        }
        if let Some(dep) = &params.vk_dep {
            builder = builder.cell_dep(dep.clone());
        }
        env.context.complete_tx(builder.build())
    }

    /// Record a batch that the chain accepted, and check the new pool cell
    /// against a tree and a nullifier set computed independently of the
    /// incremental state.
    pub fn apply_batch(&mut self, env: &mut Env, tx: &TransactionView, params: &BatchParams) {
        env.commit(tx);
        self.cell = OutPoint::new(tx.hash(), 0);
        self.capacity = params.pool_capacity;
        self.state = PoolState::from_bytes(&params.pool_data, self.config.root_history)
            .expect("accepted state is well formed");

        for commitment in &params.deposits {
            self.leaves.push(fr_from_le_bytes(commitment).expect("accepted commitment is canonical"));
        }
        assert_eq!(self.state.next_index as usize, self.leaves.len());
        if !params.deposits.is_empty() {
            let (_, root) = merkle::merkle_path(&self.leaves, 0).expect("the tree has leaves");
            assert_eq!(*self.state.current_root(), fr_to_le_bytes(&root));
        }
        for entry in &params.withdrawals {
            assert!(self.nullifiers.insert(entry.nullifier_hash), "the chain accepted a spent nullifier");
        }
        assert_eq!(self.nullifiers.root(), self.state.nullifier_root);
    }

    /// Settle an honest batch. Returns the transaction and its cycles.
    pub fn settle(
        &mut self,
        env: &mut Env,
        intents: &[OutPoint],
        proofs: &[WithdrawProof],
    ) -> (TransactionView, Cycle) {
        let params = self.batch_params(env, intents, proofs);
        let tx = self.batch_tx(env, &params);
        let cycles = env.verify(&tx).expect("the batch is accepted");
        self.apply_batch(env, &tx, &params);
        (tx, cycles)
    }

    /// Deposit `notes` through intents, all in one batch.
    pub fn deposit_notes(&mut self, env: &mut Env, notes: &[Note]) -> (TransactionView, Cycle) {
        let owner = env.user_lock(0xd0);
        let intents: Vec<OutPoint> = notes
            .iter()
            .map(|note| self.place_intent(env, &note.commitment(), &owner))
            .collect();
        self.settle(env, &intents, &[])
    }

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
}
