//! The pool as the chain has it: its scripts, its history, and the
//! transactions that move it.
//!
//! Nothing is kept between runs. The commitment tree and the nullifier set that
//! a withdrawal needs are rebuilt from the pool's own transactions, and the
//! result has to reproduce the live pool cell byte for byte.

use std::collections::HashMap;

use ckb_types::{
    bytes::Bytes,
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{Byte32, CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use mixer_core::action::{self, Action};
use mixer_core::field::fr_from_le_bytes;
use mixer_core::smt::NullifierSet;
use mixer_core::state::PoolState;
use mixer_core::{Bytes32, Fr};

use crate::proof::Withdrawal;
use crate::rpc::{hash_hex, ChainTx, LiveCell, Role, Rpc};
use crate::wallet::{occupied, plain_cell};

/// A cell that scripts refer to by the hash of its data: code, or the
/// verifying key.
#[derive(Debug, Clone)]
pub struct DataCell {
    pub out_point: OutPoint,
    pub data_hash: Byte32,
}

impl DataCell {
    fn dep(&self) -> CellDep {
        CellDep::new_builder().out_point(self.out_point.clone()).build()
    }
}

/// What has to be on chain before a pool can exist.
#[derive(Debug, Clone)]
pub struct Deployment {
    pub vk: DataCell,
    pub pool_lock: DataCell,
    pub pool_type: DataCell,
}

impl Deployment {
    /// A transaction that stores the verifying key and the two scripts, in this
    /// order, in cells of `owner`; still to be funded.
    pub fn draft(owner: &Script, vk: &[u8], pool_lock: &[u8], pool_type: &[u8]) -> TransactionView {
        let mut builder = TransactionBuilder::default();
        for data in [vk, pool_lock, pool_type] {
            let cell = plain_cell(owner, 0);
            let capacity = occupied(&cell, data.len());
            builder = builder
                .output(cell.as_builder().capacity(capacity).build())
                .output_data(Bytes::copy_from_slice(data).pack());
        }
        builder.build()
    }

    /// The cells that [`Deployment::draft`] made in transaction `tx_hash`.
    pub fn new(tx_hash: &Byte32, vk_hash: Byte32, pool_lock_hash: Byte32, pool_type_hash: Byte32) -> Self {
        let cell = |index: u32, data_hash: Byte32| DataCell { out_point: OutPoint::new(tx_hash.clone(), index), data_hash };
        Self { vk: cell(0, vk_hash), pool_lock: cell(1, pool_lock_hash), pool_type: cell(2, pool_type_hash) }
    }

    pub fn cells(&self) -> [(&'static str, &DataCell); 3] {
        [("verifying key", &self.vk), ("pool-lock", &self.pool_lock), ("pool-type", &self.pool_type)]
    }
}

/// The two scripts of a pool. Both name their code by its data hash, so a pool
/// is tied to these exact binaries and this exact verifying key, wherever they
/// are stored.
#[derive(Debug, Clone)]
pub struct Scripts {
    pub denomination: u64,
    pub type_script: Script,
    pub lock_script: Script,
}

impl Scripts {
    pub fn new(deployment: &Deployment, denomination: u64) -> Self {
        let hash_type = ScriptHashType::Data2;
        let mut args = Vec::with_capacity(73);
        args.extend_from_slice(&denomination.to_le_bytes());
        args.extend_from_slice(deployment.vk.data_hash.as_slice());
        args.extend_from_slice(deployment.pool_lock.data_hash.as_slice());
        args.push(hash_type.into());

        let type_script = Script::new_builder()
            .code_hash(deployment.pool_type.data_hash.clone())
            .hash_type(hash_type)
            .args(Bytes::from(args).pack())
            .build();
        let lock_script = Script::new_builder()
            .code_hash(deployment.pool_lock.data_hash.clone())
            .hash_type(hash_type)
            .args(type_script.calc_script_hash().as_bytes().pack())
            .build();
        Self { denomination, type_script, lock_script }
    }

    /// A pool cell.
    fn cell(&self, capacity: u64) -> CellOutput {
        CellOutput::new_builder()
            .capacity(capacity)
            .lock(self.lock_script.clone())
            .type_(Some(self.type_script.clone()).pack())
            .build()
    }

    /// A transaction that creates a pool, still to be funded. The cell gets
    /// exactly the capacity it occupies; that amount stays in the pool for good.
    pub fn create_draft(&self, deployment: &Deployment) -> TransactionView {
        let data = PoolState::empty().to_bytes();
        let capacity = occupied(&self.cell(0), data.len());
        TransactionBuilder::default()
            .cell_dep(deployment.pool_type.dep())
            .output(self.cell(capacity))
            .output_data(Bytes::from(data).pack())
            .build()
    }
}

/// What one transaction did to the pool.
#[derive(Debug, Clone)]
pub enum Event {
    Created,
    Deposited { first_leaf: u32, count: u32 },
    Withdrew { withdrawal: Withdrawal, recipient: Script },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub tx_hash: Byte32,
    pub block_number: Option<u64>,
    pub event: Event,
}

/// What the pool's transactions so far add up to.
struct Ledger {
    state: PoolState,
    leaves: Vec<Fr>,
    nullifiers: NullifierSet,
    capacity: u64,
}

impl Ledger {
    /// Apply one pool transaction: the creation, or the one whose input
    /// `pool_input` spends the pool cell. Returns the index of its pool output
    /// and what it did.
    fn apply(&mut self, scripts: &Scripts, tx: &TransactionView, pool_input: Option<usize>) -> Result<(usize, Event), String> {
        let rejected = |what: &str| format!("{}: {what}", hash_hex(&tx.hash()));
        let pool_output = tx
            .outputs()
            .into_iter()
            .position(|output| output.type_().to_opt().as_ref() == Some(&scripts.type_script))
            .ok_or_else(|| rejected("no pool output"))?;

        let Some(index) = pool_input else {
            self.capacity = tx.outputs().get(pool_output).expect("found above").capacity().unpack();
            return Ok((pool_output, Event::Created));
        };
        let witness = tx.witnesses().get(index).ok_or_else(|| rejected("no pool witness"))?;
        let args = WitnessArgs::from_slice(&witness.raw_data()).map_err(|_| rejected("bad pool witness"))?;
        let bytes = args.input_type().to_opt().ok_or_else(|| rejected("no action"))?.raw_data();
        let event = match action::decode(&bytes).ok_or_else(|| rejected("bad action"))? {
            Action::Deposit { commitments } => {
                let first_leaf = self.state.next_index;
                let count = self.state.deposit(commitments).map_err(|error| rejected(&format!("{error:?}")))?;
                for chunk in commitments.chunks_exact(32) {
                    let commitment: Bytes32 = chunk.try_into().expect("32 bytes");
                    self.leaves.push(fr_from_le_bytes(&commitment).expect("the deposit checked it"));
                }
                self.capacity += u64::from(count) * scripts.denomination;
                Event::Deposited { first_leaf, count }
            }
            Action::Withdraw(withdraw) => {
                // Our own siblings, not the transaction's: the tree kept here
                // has to stand on its own.
                let proof = self.nullifiers.proof(&withdraw.nullifier_hash);
                self.state
                    .withdraw(&withdraw.root, &withdraw.nullifier_hash, &proof)
                    .map_err(|error| rejected(&format!("{error:?}")))?;
                self.nullifiers.insert(withdraw.nullifier_hash);
                self.capacity = self.capacity.checked_sub(scripts.denomination).ok_or_else(|| rejected("pool overdrawn"))?;

                let recipient = tx.outputs().get(pool_output + 1).ok_or_else(|| rejected("no recipient"))?.lock();
                Event::Withdrew {
                    withdrawal: Withdrawal {
                        root: withdraw.root,
                        nullifier_hash: withdraw.nullifier_hash,
                        recipient_lock_hash: recipient.calc_script_hash().as_slice().try_into().expect("32 bytes"),
                        fee: withdraw.fee,
                        proof: withdraw.proof.to_vec(),
                    },
                    recipient,
                }
            }
        };
        Ok((pool_output, event))
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
    /// The pool that transaction `creation` created, as it is now.
    ///
    /// The type script alone does not identify a pool: it has no unique id in
    /// its args, so anyone can create another cell with the same scripts. A
    /// pool is the chain of cells that starts at its creation.
    pub fn load(rpc: &Rpc, scripts: Scripts, creation: &Byte32) -> Result<Self, String> {
        let mut cache = HashMap::new();
        for cell in rpc.live_cells(&scripts.type_script, Role::Type)? {
            let lineage = lineage(rpc, &scripts, &cell.out_point, &mut cache)?;
            if lineage.first().map(|(found, _)| found.tx.hash()).as_ref() == Some(creation) {
                return Self::replay(scripts, cell, lineage);
            }
        }
        Err(format!("no live pool cell descends from {}", hash_hex(creation)))
    }

    /// Apply the pool's transactions to the empty state, the way the type
    /// script did when it accepted them.
    fn replay(scripts: Scripts, cell: LiveCell, lineage: Vec<(ChainTx, Option<usize>)>) -> Result<Self, String> {
        let mut ledger = Ledger {
            state: PoolState::empty(),
            leaves: Vec::new(),
            nullifiers: NullifierSet::new(),
            capacity: 0,
        };
        let mut steps = Vec::new();
        for (found, pool_input) in lineage {
            let (_, event) = ledger.apply(&scripts, &found.tx, pool_input)?;
            steps.push(Step { tx_hash: found.tx.hash(), block_number: found.block_number, event });
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

    /// The pool as it will be once `tx`, which spends this pool's cell, is
    /// committed. The next transaction can be built on the result before the
    /// chain has seen `tx`.
    pub fn after(&self, tx: &TransactionView) -> Result<Self, String> {
        let pool_input = tx
            .inputs()
            .into_iter()
            .position(|input| input.previous_output() == self.cell.out_point)
            .ok_or_else(|| format!("{} does not spend the pool cell", hash_hex(&tx.hash())))?;
        let mut ledger = Ledger {
            state: self.state.clone(),
            leaves: self.leaves.clone(),
            nullifiers: self.nullifiers.clone(),
            capacity: self.cell.capacity(),
        };
        let (pool_output, event) = ledger.apply(&self.scripts, tx, Some(pool_input))?;

        let output = tx.outputs().get(pool_output).expect("apply found it");
        let data = tx.outputs_data().get(pool_output).map(|data| data.raw_data()).unwrap_or_default();
        if ledger.state.to_bytes() != data.as_ref() || ledger.capacity != Unpack::<u64>::unpack(&output.capacity()) {
            return Err(format!("{} does not produce the state its action leads to", hash_hex(&tx.hash())));
        }
        let mut steps = self.steps.clone();
        steps.push(Step { tx_hash: tx.hash(), block_number: None, event });
        let Ledger { state, leaves, nullifiers, .. } = ledger;
        Ok(Self {
            scripts: self.scripts.clone(),
            cell: LiveCell { out_point: OutPoint::new(tx.hash(), pool_output as u32), output, data },
            state,
            leaves,
            nullifiers,
            steps,
        })
    }

    /// Where `commitment` sits in the tree.
    pub fn leaf_index(&self, commitment: &Fr) -> Option<u32> {
        self.leaves.iter().position(|leaf| leaf == commitment).map(|index| index as u32)
    }

    /// The part every pool transaction shares: the pool cell goes in with the
    /// action as its witness, and comes out with `capacity` and `data`.
    fn update(&self, deployment: &Deployment, action: Vec<u8>, capacity: u64, data: Vec<u8>) -> TransactionBuilder {
        let witness = WitnessArgs::new_builder().input_type(Some(Bytes::from(action)).pack()).build();
        TransactionBuilder::default()
            .cell_dep(deployment.pool_type.dep())
            .cell_dep(deployment.pool_lock.dep())
            .input(CellInput::new(self.cell.out_point.clone(), 0))
            .witness(witness.as_bytes().pack())
            .output(self.scripts.cell(capacity))
            .output_data(Bytes::from(data).pack())
    }

    /// A deposit of `commitments`, still to be funded.
    pub fn deposit_draft(&self, deployment: &Deployment, commitments: &[Bytes32]) -> Result<TransactionView, String> {
        let mut state = self.state.clone();
        state.deposit(&commitments.concat()).map_err(|error| format!("deposit: {error:?}"))?;
        let capacity = self.cell.capacity() + commitments.len() as u64 * self.scripts.denomination;
        Ok(self.update(deployment, action::encode_deposit(commitments), capacity, state.to_bytes()).build())
    }

    /// The withdrawal `withdrawal` proves, paying `recipient`. Complete as it
    /// is: the pool cell is the only input, nobody signs, and the fee is the
    /// part of the denomination that the recipient does not get.
    ///
    /// For a withdrawal the pool refuses (a spent note, a forgotten root) the
    /// pool data is left as it is; the type script rejects the transaction
    /// before it looks at the output.
    pub fn withdraw_tx(&self, deployment: &Deployment, withdrawal: &Withdrawal, recipient: &Script) -> TransactionView {
        let nullifier_proof = self.nullifiers.proof(&withdrawal.nullifier_hash);
        let mut state = self.state.clone();
        let _ = state.withdraw(&withdrawal.root, &withdrawal.nullifier_hash, &nullifier_proof);

        let denomination = self.scripts.denomination;
        let capacity = self.cell.capacity().saturating_sub(denomination);
        self.update(deployment, withdrawal.action(&nullifier_proof), capacity, state.to_bytes())
            .cell_dep(deployment.vk.dep())
            .output(plain_cell(recipient, denomination.saturating_sub(withdrawal.fee)))
            .output_data(Bytes::new().pack())
            .build()
    }

    /// Not a pool transaction at all: the pool cell goes in and its capacity
    /// comes out in a plain cell of `thief`.
    pub fn theft_tx(&self, deployment: &Deployment, thief: &Script) -> TransactionView {
        let witness = WitnessArgs::new_builder()
            .input_type(Some(Bytes::from(action::encode_deposit(&[[0u8; 32]]))).pack())
            .build();
        TransactionBuilder::default()
            .cell_dep(deployment.pool_type.dep())
            .cell_dep(deployment.pool_lock.dep())
            .input(CellInput::new(self.cell.out_point.clone(), 0))
            .witness(witness.as_bytes().pack())
            .output(plain_cell(thief, self.cell.capacity() - 1_000_000))
            .output_data(Bytes::new().pack())
            .build()
    }
}

/// The transactions behind the pool cell at `out_point`, oldest first, each
/// with the index of the input that spent the pool cell before it; the
/// creation has none.
fn lineage(
    rpc: &Rpc,
    scripts: &Scripts,
    out_point: &OutPoint,
    cache: &mut HashMap<Byte32, ChainTx>,
) -> Result<Vec<(ChainTx, Option<usize>)>, String> {
    let mut fetch = |hash: &Byte32| -> Result<ChainTx, String> {
        if let Some(found) = cache.get(hash) {
            return Ok(found.clone());
        }
        let found = rpc.transaction(hash)?.ok_or_else(|| format!("the node does not know {}", hash_hex(hash)))?;
        cache.insert(hash.clone(), found.clone());
        Ok(found)
    };

    let mut steps = Vec::new();
    let mut tx_hash = out_point.tx_hash();
    loop {
        let found = fetch(&tx_hash)?;
        let mut pool_input = None;
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
                break;
            }
        }
        match pool_input {
            Some((index, previous)) => {
                steps.push((found, Some(index)));
                tx_hash = previous;
            }
            None => {
                steps.push((found, None));
                break;
            }
        }
    }
    steps.reverse();
    Ok(steps)
}
