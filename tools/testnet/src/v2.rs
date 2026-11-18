//! Pool v2 as the chain has it: its scripts, its history, the intents waiting
//! for it, and the transactions that move it.
//!
//! As with v1, nothing is kept between runs: the commitment tree and the
//! nullifier set are rebuilt from the pool's own transactions, and the result
//! has to reproduce the live pool cell byte for byte.

use std::collections::HashMap;

use ckb_jsonrpc_types as json_types;
use ckb_types::{
    bytes::Bytes,
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{Byte32, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use mixer_core::action::Withdraw;
use mixer_core::field::fr_from_le_bytes;
use mixer_core::smt::NullifierSet;
use mixer_core::{Bytes32, Fr};
use mixer_core_v2::{self as core, IntentArgs, PoolConfig, PoolState, ScriptId};
use serde::{Deserialize, Serialize};

use crate::pool::DataCell;
use crate::proof::Withdrawal;
use crate::rpc::{hash_hex, ChainTx, LiveCell, Role, Rpc};
use crate::wallet::{self, hex, occupied, plain_cell, unhex, Key};

/// How the pool's scripts name their code: by the hash of the binary, run on
/// the current VM.
pub const HASH_TYPE: ScriptHashType = ScriptHashType::Data2;

fn raw(hash: &Byte32) -> Bytes32 {
    hash.as_slice().try_into().expect("32 bytes")
}

/// What has to be on chain before a v2 pool can exist. The verifying key and
/// the pool lock are the same as v1's, and may be the very cells v1 uses.
#[derive(Debug, Clone)]
pub struct Deployment {
    pub vk: DataCell,
    pub pool_lock: DataCell,
    pub pool_type: DataCell,
    pub intent_lock: DataCell,
}

impl Deployment {
    pub fn cells(&self) -> [(&'static str, &DataCell); 4] {
        [
            ("verifying key", &self.vk),
            ("pool-lock", &self.pool_lock),
            ("pool-type-v2", &self.pool_type),
            ("intent-lock", &self.intent_lock),
        ]
    }
}

/// A transaction that stores each of `data` in a cell of `owner`, in order;
/// still to be funded.
pub fn store_draft(owner: &Script, data: &[&[u8]]) -> TransactionView {
    let mut builder = TransactionBuilder::default();
    for data in data {
        let cell = plain_cell(owner, 0);
        let capacity = occupied(&cell, data.len());
        builder = builder
            .output(cell.as_builder().capacity(capacity).build())
            .output_data(Bytes::copy_from_slice(data).pack());
    }
    builder.build()
}

/// A pool's two scripts and what its type script's args say.
#[derive(Debug, Clone)]
pub struct Scripts {
    pub config: PoolConfig,
    pub type_script: Script,
    pub lock_script: Script,
}

impl Scripts {
    pub fn new(deployment: &Deployment, denomination: u64, root_history: u16, type_id: Bytes32) -> Self {
        let id = |cell: &DataCell| ScriptId { code_hash: raw(&cell.data_hash), hash_type: HASH_TYPE.into() };
        let config = PoolConfig {
            type_id,
            denomination,
            vk_hash: raw(&deployment.vk.data_hash),
            pool_lock: id(&deployment.pool_lock),
            intent_lock: id(&deployment.intent_lock),
            root_history,
        };
        let type_script = Script::new_builder()
            .code_hash(deployment.pool_type.data_hash.clone())
            .hash_type(HASH_TYPE)
            .args(Bytes::from(config.to_bytes()).pack())
            .build();
        let lock_script = Script::new_builder()
            .code_hash(deployment.pool_lock.data_hash.clone())
            .hash_type(HASH_TYPE)
            .args(type_script.calc_script_hash().as_bytes().pack())
            .build();
        Self { config, type_script, lock_script }
    }

    /// The hash of the type script. With the type ID in its args it names one
    /// cell, and it is what intents and the pool lock refer to.
    pub fn type_hash(&self) -> Bytes32 {
        raw(&self.type_script.calc_script_hash())
    }

    /// A pool cell.
    fn cell(&self, capacity: u64) -> CellOutput {
        CellOutput::new_builder()
            .capacity(capacity)
            .lock(self.lock_script.clone())
            .type_(Some(self.type_script.clone()).pack())
            .build()
    }

    /// The transaction that creates a pool, funded and signed by `key`, and the
    /// scripts of the pool it creates.
    ///
    /// The type ID is derived from the transaction's first input, so the
    /// scripts are known only once the funding cells are.
    pub fn create(rpc: &Rpc, key: &Key, deployment: &Deployment, denomination: u64, root_history: u16) -> Result<(Self, TransactionView), String> {
        let placeholder = Self::new(deployment, denomination, root_history, [0; 32]);
        let data = PoolState::empty(root_history).to_bytes();
        // The cell gets exactly the capacity it occupies; that amount stays in
        // the pool for good.
        let capacity = occupied(&placeholder.cell(0), data.len());
        let draft = TransactionBuilder::default()
            .cell_dep(deployment.pool_type.dep())
            .output(placeholder.cell(capacity))
            .output_data(Bytes::from(data).pack())
            .build();

        let scripts_of = |tx: &TransactionView| {
            let first_input = tx.inputs().get(0).expect("a funded transaction has an input");
            Self::new(deployment, denomination, root_history, core::type_id(first_input.as_slice(), 0))
        };
        let tx = wallet::fund_with(rpc, key, draft, 0, 0, &|tx| {
            let mut outputs: Vec<CellOutput> = tx.outputs().into_iter().collect();
            outputs[0] = scripts_of(&tx).cell(capacity);
            tx.as_advanced_builder().set_outputs(outputs).build()
        })?;
        Ok((scripts_of(&tx), tx))
    }

    /// The args of an intent for this pool that `owner` can take back after
    /// `reclaim_after` blocks.
    pub fn intent_args(&self, commitment: Bytes32, owner: &Script, reclaim_after: u64) -> IntentArgs {
        IntentArgs {
            pool_type_hash: self.type_hash(),
            commitment,
            owner_lock_hash: raw(&owner.calc_script_hash()),
            reclaim_after,
        }
    }

    /// The intent lock with `args`, naming its code by `hash_type`. The pool
    /// counts only [`HASH_TYPE`]; the others exist to show that.
    pub fn intent_lock(&self, hash_type: ScriptHashType, args: &IntentArgs) -> Script {
        Script::new_builder()
            .code_hash(self.config.intent_lock.code_hash.pack())
            .hash_type(hash_type)
            .args(Bytes::from(args.to_bytes()).pack())
            .build()
    }

    /// The commitment `lock` deposits into this pool when a cell under it is
    /// spent next to the pool cell: the rule of the pool type script.
    fn deposited_by(&self, lock: &Script) -> Option<Bytes32> {
        let intent_lock = &self.config.intent_lock;
        if lock.code_hash().as_slice() != intent_lock.code_hash || lock.hash_type().as_slice() != [intent_lock.hash_type] {
            return None;
        }
        IntentArgs::parse(&lock.args().raw_data())
            .filter(|args| args.pool_type_hash == self.type_hash())
            .map(|args| args.commitment)
    }

    /// Every live cell under the intent lock, named by `hash_type`, whose args
    /// bind it to this pool; whether or not it can be deposited.
    pub fn intents(&self, rpc: &Rpc, hash_type: ScriptHashType) -> Result<Vec<Intent>, String> {
        let prefix = Script::new_builder()
            .code_hash(self.config.intent_lock.code_hash.pack())
            .hash_type(hash_type)
            .args(Bytes::copy_from_slice(&self.type_hash()).pack())
            .build();
        let cells = rpc.cells_with_lock_prefix(&prefix)?;
        Ok(cells
            .into_iter()
            .filter_map(|cell| {
                let args = IntentArgs::parse(&cell.output.lock().args().raw_data())?;
                Some(Intent { cell, args })
            })
            .collect())
    }

    /// Why an aggregator leaves `intent` alone, if it does. Depositing any of
    /// these would make the whole batch fail, or pay less than `least_tip`.
    pub fn refusal(&self, intent: &Intent, least_tip: u64) -> Option<String> {
        if intent.cell.output.type_().is_some() || !intent.cell.data.is_empty() {
            return Some("it has a type script or data".to_string());
        }
        if fr_from_le_bytes(&intent.args.commitment).is_none() {
            return Some("its commitment is not a field element".to_string());
        }
        match intent.cell.capacity().checked_sub(self.config.denomination) {
            Some(tip) if tip >= least_tip => None,
            Some(tip) => Some(format!("its tip is {tip} shannons")),
            None => Some("it holds less than the denomination".to_string()),
        }
    }
}

/// A cell under the intent lock.
#[derive(Debug, Clone)]
pub struct Intent {
    pub cell: LiveCell,
    pub args: IntentArgs,
}

/// A withdrawal and the lock it pays.
#[derive(Debug, Clone)]
pub struct Payout {
    pub withdrawal: Withdrawal,
    pub recipient: Script,
}

/// A withdrawal as it is handed to an aggregator: everything the pool
/// transaction needs, and no secret. The nullifier-set proof is not part of it;
/// it depends on the withdrawals that come before, and the aggregator computes
/// it.
#[derive(Serialize, Deserialize)]
pub struct Message {
    /// The pool's type script hash.
    pub pool: String,
    pub root: String,
    pub nullifier_hash: String,
    pub recipient: json_types::Script,
    /// Shannons.
    pub fee: u64,
    pub proof: String,
}

impl Message {
    pub fn new(pool: &Bytes32, payout: &Payout) -> Self {
        Self {
            pool: hex(pool),
            root: hex(&payout.withdrawal.root),
            nullifier_hash: hex(&payout.withdrawal.nullifier_hash),
            recipient: payout.recipient.clone().into(),
            fee: payout.withdrawal.fee,
            proof: hex(&payout.withdrawal.proof),
        }
    }

    /// The pool the message is for, and the payout it asks for.
    pub fn payout(&self) -> Result<(Bytes32, Payout), String> {
        let bytes32 = |text: &str| -> Result<Bytes32, String> { unhex(text)?.try_into().map_err(|_| format!("not 32 bytes: {text}")) };
        let recipient: Script = self.recipient.clone().into();
        let withdrawal = Withdrawal {
            root: bytes32(&self.root)?,
            nullifier_hash: bytes32(&self.nullifier_hash)?,
            recipient_lock_hash: raw(&recipient.calc_script_hash()),
            fee: self.fee,
            proof: unhex(&self.proof)?,
        };
        Ok((bytes32(&self.pool)?, Payout { withdrawal, recipient }))
    }
}

/// What one transaction did to the pool.
#[derive(Debug, Clone)]
pub enum Event {
    Created,
    Settled { first_leaf: u32, deposits: u32, payouts: Vec<Payout> },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub tx_hash: Byte32,
    pub block_number: Option<u64>,
    pub event: Event,
}

/// A pool transaction as the chain has it: the index of the input that spends
/// the pool cell (the creation has none) and the lock of every input.
struct Link {
    found: ChainTx,
    pool_input: Option<usize>,
    input_locks: Vec<Script>,
}

/// What the pool's transactions so far add up to.
struct Ledger {
    state: PoolState,
    leaves: Vec<Fr>,
    nullifiers: NullifierSet,
    capacity: u64,
}

impl Ledger {
    /// Apply one pool transaction the way the type script does: the
    /// withdrawals of its witness first, then one deposit per intent input.
    fn apply(&mut self, scripts: &Scripts, link: &Link) -> Result<Event, String> {
        let tx = &link.found.tx;
        let rejected = |what: &str| format!("{}: {what}", hash_hex(&tx.hash()));
        let pool_output = tx
            .outputs()
            .into_iter()
            .position(|output| output.type_().to_opt().as_ref() == Some(&scripts.type_script))
            .ok_or_else(|| rejected("no pool output"))?;

        let Some(index) = link.pool_input else {
            self.capacity = tx.outputs().get(pool_output).expect("found above").capacity().unpack();
            return Ok(Event::Created);
        };
        let witness = tx.witnesses().get(index).ok_or_else(|| rejected("no pool witness"))?;
        let args = WitnessArgs::from_slice(&witness.raw_data()).map_err(|_| rejected("bad pool witness"))?;
        let bytes = args.input_type().to_opt().ok_or_else(|| rejected("no action"))?.raw_data();
        let withdrawals = core::decode_batch(&bytes).ok_or_else(|| rejected("bad action"))?;
        let denomination = scripts.config.denomination;

        let mut payouts = Vec::with_capacity(withdrawals.len());
        for (position, withdraw) in withdrawals.iter().enumerate() {
            // Our own siblings, not the transaction's: the tree kept here has
            // to stand on its own.
            let proof = self.nullifiers.proof(&withdraw.nullifier_hash);
            self.state
                .withdraw(&withdraw.root, &withdraw.nullifier_hash, &proof)
                .map_err(|error| rejected(&format!("{error:?}")))?;
            self.nullifiers.insert(withdraw.nullifier_hash);
            self.capacity = self.capacity.checked_sub(denomination).ok_or_else(|| rejected("pool overdrawn"))?;

            let recipient = tx.outputs().get(pool_output + 1 + position).ok_or_else(|| rejected("no recipient"))?.lock();
            payouts.push(Payout {
                withdrawal: Withdrawal {
                    root: withdraw.root,
                    nullifier_hash: withdraw.nullifier_hash,
                    recipient_lock_hash: raw(&recipient.calc_script_hash()),
                    fee: withdraw.fee,
                    proof: withdraw.proof.to_vec(),
                },
                recipient,
            });
        }

        let commitments: Vec<Bytes32> = link.input_locks.iter().filter_map(|lock| scripts.deposited_by(lock)).collect();
        let first_leaf = self.state.next_index;
        self.state.deposit(&commitments).map_err(|error| rejected(&format!("{error:?}")))?;
        for commitment in &commitments {
            self.leaves.push(fr_from_le_bytes(commitment).expect("the deposit checked it"));
        }
        self.capacity += commitments.len() as u64 * denomination;
        Ok(Event::Settled { first_leaf, deposits: commitments.len() as u32, payouts })
    }
}

/// A pool, rebuilt from the chain.
pub struct Pool {
    pub scripts: Scripts,
    /// The live pool cell.
    pub cell: LiveCell,
    pub state: PoolState,
    /// Every commitment deposited so far, in tree order.
    pub leaves: Vec<Fr>,
    pub nullifiers: NullifierSet,
    /// The pool's transactions, oldest first.
    pub steps: Vec<Step>,
}

impl Pool {
    /// The pool with these scripts, as it is now. The type ID in the type
    /// script's args makes it the only cell of that type.
    pub fn load(rpc: &Rpc, scripts: Scripts) -> Result<Self, String> {
        let mut cells = rpc.live_cells(&scripts.type_script, Role::Type)?;
        let cell = match cells.len() {
            1 => cells.remove(0),
            count => return Err(format!("{count} live cells carry the pool's type script; there has to be exactly one")),
        };

        let mut ledger = Ledger {
            state: PoolState::empty(scripts.config.root_history),
            leaves: Vec::new(),
            nullifiers: NullifierSet::new(),
            capacity: 0,
        };
        let mut steps = Vec::new();
        for link in lineage(rpc, &scripts, &cell.out_point)? {
            let event = ledger.apply(&scripts, &link)?;
            steps.push(Step { tx_hash: link.found.tx.hash(), block_number: link.found.block_number, event });
        }
        if ledger.state.to_bytes() != cell.data.as_ref() || ledger.capacity != cell.capacity() {
            return Err(format!(
                "the transactions behind {} do not add up to the state it holds",
                hash_hex(&cell.out_point.tx_hash())
            ));
        }
        let Ledger { state, leaves, nullifiers, .. } = ledger;
        Ok(Self { scripts, cell, state, leaves, nullifiers, steps })
    }

    /// Where `commitment` sits in the tree.
    pub fn leaf_index(&self, commitment: &Fr) -> Option<u32> {
        self.leaves.iter().position(|leaf| leaf == commitment).map(|index| index as u32)
    }

    /// Capacity that the pool cell and `intents` bring into a transaction.
    pub fn input_capacity(&self, intents: &[Intent]) -> u64 {
        self.cell.capacity() + intents.iter().map(|intent| intent.cell.capacity()).sum::<u64>()
    }

    /// The batch that deposits `intents` and pays `payouts`, in these orders.
    ///
    /// Complete except for whoever collects what is left over: every intent's
    /// capacity beyond the denomination and every withdrawal's fee are inputs
    /// that no output claims yet.
    pub fn batch_draft(&self, deployment: &Deployment, intents: &[Intent], payouts: &[Payout]) -> Result<TransactionView, String> {
        let mut state = self.state.clone();
        let mut nullifiers = self.nullifiers.clone();
        let mut nullifier_proofs = Vec::with_capacity(payouts.len());
        for payout in payouts {
            let withdrawal = &payout.withdrawal;
            // Each proof is against the set as the withdrawals before it leave it.
            let proof = nullifiers.proof(&withdrawal.nullifier_hash);
            state
                .withdraw(&withdrawal.root, &withdrawal.nullifier_hash, &proof)
                .map_err(|error| format!("withdrawal {}: {error:?}", hex(&withdrawal.nullifier_hash)))?;
            nullifiers.insert(withdrawal.nullifier_hash);
            nullifier_proofs.push(proof);
        }
        let commitments: Vec<Bytes32> = intents.iter().map(|intent| intent.args.commitment).collect();
        state.deposit(&commitments).map_err(|error| format!("deposit: {error:?}"))?;

        let denomination = self.scripts.config.denomination;
        let capacity = (self.cell.capacity() + intents.len() as u64 * denomination)
            .checked_sub(payouts.len() as u64 * denomination)
            .ok_or("the pool does not hold that many notes")?;
        Ok(self.assemble(deployment, intents, payouts, &nullifier_proofs, capacity, state.to_bytes()))
    }

    /// A pool transaction with everything spelled out, right or wrong: the
    /// pool cell and `intents` go in, the pool cell comes out with `capacity`
    /// and `data`, followed by one output per payout.
    pub fn assemble(
        &self,
        deployment: &Deployment,
        intents: &[Intent],
        payouts: &[Payout],
        nullifier_proofs: &[Vec<u8>],
        capacity: u64,
        data: Vec<u8>,
    ) -> TransactionView {
        let withdrawals: Vec<Withdraw<'_>> = payouts
            .iter()
            .zip(nullifier_proofs)
            .map(|(payout, nullifier_proof)| Withdraw {
                fee: payout.withdrawal.fee,
                root: payout.withdrawal.root,
                nullifier_hash: payout.withdrawal.nullifier_hash,
                proof: &payout.withdrawal.proof,
                nullifier_proof,
            })
            .collect();
        let action = core::encode_batch(&withdrawals).expect("a batch this tool can build fits the encoding");
        let witness = WitnessArgs::new_builder().input_type(Some(Bytes::from(action)).pack()).build();

        let mut builder = TransactionBuilder::default()
            .cell_dep(deployment.pool_type.dep())
            .cell_dep(deployment.pool_lock.dep())
            .input(CellInput::new(self.cell.out_point.clone(), 0))
            .witness(witness.as_bytes().pack())
            .output(self.scripts.cell(capacity))
            .output_data(Bytes::from(data).pack());
        if !intents.is_empty() {
            builder = builder.cell_dep(deployment.intent_lock.dep());
        }
        if !payouts.is_empty() {
            builder = builder.cell_dep(deployment.vk.dep());
        }
        for intent in intents {
            builder = builder.input(CellInput::new(intent.cell.out_point.clone(), 0)).witness(Bytes::new().pack());
        }
        let denomination = self.scripts.config.denomination;
        for payout in payouts {
            builder = builder
                .output(plain_cell(&payout.recipient, denomination.saturating_sub(payout.withdrawal.fee)))
                .output_data(Bytes::new().pack());
        }
        builder.build()
    }
}

/// A transaction that spends `intent` without the pool, with `since` on its
/// input; still to be funded, by its owner if the lock is to accept it. The
/// funding account gets the intent's capacity as change.
pub fn reclaim_draft(deployment: &Deployment, intent: &Intent, since: u64) -> TransactionView {
    TransactionBuilder::default()
        .cell_dep(deployment.intent_lock.dep())
        .input(CellInput::new(intent.cell.out_point.clone(), since))
        .witness(Bytes::new().pack())
        .build()
}

/// The transactions behind the pool cell at `out_point`, oldest first.
fn lineage(rpc: &Rpc, scripts: &Scripts, out_point: &OutPoint) -> Result<Vec<Link>, String> {
    let mut cache: HashMap<Byte32, ChainTx> = HashMap::new();
    let mut fetch = |hash: &Byte32| -> Result<ChainTx, String> {
        if let Some(found) = cache.get(hash) {
            return Ok(found.clone());
        }
        let found = rpc.transaction(hash)?.ok_or_else(|| format!("the node does not know {}", hash_hex(hash)))?;
        cache.insert(hash.clone(), found.clone());
        Ok(found)
    };

    let mut links = Vec::new();
    let mut tx_hash = out_point.tx_hash();
    loop {
        let found = fetch(&tx_hash)?;
        let mut pool_input = None;
        let mut input_locks = Vec::new();
        for (index, input) in found.tx.inputs().into_iter().enumerate() {
            let previous = input.previous_output();
            let output_index: u32 = previous.index().unpack();
            let spent = fetch(&previous.tx_hash())?
                .tx
                .outputs()
                .get(output_index as usize)
                .ok_or_else(|| format!("{} spends an output that does not exist", hash_hex(&tx_hash)))?;
            if spent.type_().to_opt().as_ref() == Some(&scripts.type_script) {
                pool_input = Some((index, previous.tx_hash()));
            }
            input_locks.push(spent.lock());
        }
        let previous = pool_input.as_ref().map(|(_, hash)| hash.clone());
        links.push(Link { found, pool_input: pool_input.map(|(index, _)| index), input_locks });
        match previous {
            Some(hash) => tx_hash = hash,
            None => break,
        }
    }
    links.reverse();
    Ok(links)
}
