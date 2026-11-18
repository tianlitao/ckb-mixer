//! `mixer2 <command>`: pool v2 of the mixer PoC on CKB testnet.
//!
//! The chain is the source of truth. `state-v2.json` next to this crate keeps
//! only what the chain cannot give back: where the code is, which pool is
//! ours, the secrets of the notes made here, and when each transaction was
//! sent. The commitment tree, the nullifier set and the list of waiting
//! intents are read from the chain on every run.
//!
//! Three roles use it, and here they all hold the same key: a depositor makes
//! intents and may take them back, a withdrawer writes withdrawal messages to
//! `requests/`, and an aggregator settles whatever it finds.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use ckb_types::{
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{Byte32, CellOutput, OutPoint, Script},
    prelude::*,
};
use mixer_core::field::fr_to_le_bytes;
use mixer_core::poseidon::hash2;
use mixer_core::{Bytes32, Error, Fr};
use mixer_core_v2::{self as core, IntentError};
use mixer_testnet::cli::{
    byte32, bytes32, ckb, contracts_dir, grouped, load_state, now, print_sent, save_state, tool_dir, Note, NoteRecord,
    Options, SentRecord, WAIT,
};
use mixer_testnet::pool::DataCell;
use mixer_testnet::proof;
use mixer_testnet::rpc::{hash_hex, Rpc};
use mixer_testnet::v2::{reclaim_draft, store_draft, Deployment, Event, Intent, Message, Payout, Pool, Scripts, HASH_TYPE};
use mixer_testnet::wallet::{self, hex, occupied, plain_cell, Key, FEE_RATE, SHANNONS};
use mixer_testnet::TESTNET_RPC;
use serde::{Deserialize, Serialize};

const USAGE: &str = "usage: mixer2 <command> [--option value]...

  address  [--key <file>]                the funding account and what it holds
  deploy   [--variant size]              store pool-type-v2 and intent-lock; the verifying key and
                                         pool-lock too, unless the cells of the v1 deployment serve
  create   [--denomination 150]          create a pool; denomination in CKB
           [--root-history 32]
  intent   [--count 1] [--tip 1]         make deposit intents; tip in CKB
           [--reclaim-after 100]         ... that their owner can take back after this many blocks
           [--hash-type data2]           ... under another hash type, which the pool does not count
  request  --note <n> [--fee 1000000]    prove the withdrawal of note n and write the message an
           [--to self]                   aggregator needs to requests/; fee in shannons
  aggregate [--deposits <max>]           settle one batch: waiting intents and requested withdrawals
           [--withdrawals <max>]
           [--key <file>]                ... as another aggregator
           [--fee-rate 5000]             ... paying this many shannons per KB of weight
           [--wait yes]                  ... `no` returns as soon as the node has the transaction
  race     --key <file> --rates <a,b,..> two aggregators go for the same intents: ours sends its batch,
           [--deposits 2]                then the other offers its own at each fee rate in turn
  reclaim  --note <n>                    take the intent of note n back
  attack                                 dry-run transactions the scripts have to refuse
  pay      --key <file> --ckb <n>        send CKB to another key, creating it if need be
  info                                   the pool as the chain has it, and what was sent";

/// What a withdrawal leaves to the aggregator unless told otherwise: 0.01 CKB.
const WITHDRAW_FEE: u64 = 1_000_000;
/// The longest wait a relative `since` can express, in blocks. The intent lock
/// takes any number in its args, and no script runs when a cell is created: a
/// cell made with a longer wait than this can be deposited, but never taken back.
const LONGEST_WAIT: u64 = (1 << 56) - 1;

#[derive(Serialize, Deserialize, Default)]
struct State {
    deployment: Option<DeploymentRecord>,
    pool: Option<PoolRecord>,
    notes: Vec<NoteRecord>,
    sent: Vec<SentRecord>,
}

/// A cell that scripts name by the hash of its data.
#[derive(Serialize, Deserialize)]
struct CellRecord {
    tx_hash: String,
    index: u32,
    data_hash: String,
}

impl CellRecord {
    fn cell(&self) -> Result<DataCell, String> {
        Ok(DataCell { out_point: OutPoint::new(byte32(&self.tx_hash)?, self.index), data_hash: byte32(&self.data_hash)? })
    }
}

#[derive(Serialize, Deserialize)]
struct DeploymentRecord {
    variant: String,
    vk: CellRecord,
    pool_lock: CellRecord,
    pool_type: CellRecord,
    intent_lock: CellRecord,
}

#[derive(Serialize, Deserialize)]
struct PoolRecord {
    /// Shannons.
    denomination: u64,
    root_history: u16,
    type_id: String,
    creation_tx: String,
}

/// The part of the v1 tool's state file that says where v1's code cells are.
#[derive(Deserialize, Default)]
struct V1State {
    deployment: Option<V1Deployment>,
}

#[derive(Deserialize)]
struct V1Deployment {
    tx_hash: String,
    vk_hash: String,
    pool_lock_hash: String,
}

fn data_hash(data: &[u8]) -> String {
    hash_hex(&CellOutput::calc_data_hash(data))
}

/// Capacity of the outputs of `tx`.
fn outputs(tx: &TransactionView) -> u64 {
    tx.outputs().into_iter().map(|output| Unpack::<u64>::unpack(&output.capacity())).sum()
}

/// Capacity of the inputs of `tx` from `first` on: the cells that pay for it.
fn funding(tx: &TransactionView, first: usize, rpc: &Rpc) -> Result<u64, String> {
    let mut total = 0;
    for input in tx.inputs().into_iter().skip(first) {
        let out_point = input.previous_output();
        let index: u32 = out_point.index().unpack();
        let source = rpc.transaction(&out_point.tx_hash())?.ok_or("a funding cell's transaction is unknown")?;
        let output = source.tx.outputs().get(index as usize).ok_or("a funding cell does not exist")?;
        total += Unpack::<u64>::unpack(&output.capacity());
    }
    Ok(total)
}

fn requests_dir() -> PathBuf {
    tool_dir().join("requests")
}

/// What a script is expected to say about a transaction it has to refuse.
enum Refusal {
    Pool(Error),
    Intent(IntentError),
}

impl Refusal {
    fn code(&self) -> i8 {
        match self {
            Refusal::Pool(error) => error.code(),
            Refusal::Intent(error) => error.code(),
        }
    }

    fn name(&self) -> String {
        match self {
            Refusal::Pool(error) => format!("pool type script, {error:?}"),
            Refusal::Intent(error) => format!("intent lock, {error:?}"),
        }
    }
}

struct Mixer {
    rpc: Rpc,
    key: Key,
    state: State,
    state_path: PathBuf,
}

impl Mixer {
    fn open() -> Result<Self, String> {
        let state_path = tool_dir().join("state-v2.json");
        let state: State = load_state(&state_path)?;
        let rpc = Rpc::new(TESTNET_RPC);
        let chain = rpc.chain()?;
        if chain != "ckb_testnet" {
            return Err(format!("{TESTNET_RPC} serves {chain}, not the testnet"));
        }
        Ok(Self { rpc, key: Key::load(&tool_dir().join("dev.key"))?, state, state_path })
    }

    fn save(&self) -> Result<(), String> {
        save_state(&self.state_path, &self.state)
    }

    fn deployment(&self) -> Result<Deployment, String> {
        let record = self.state.deployment.as_ref().ok_or("nothing is deployed yet: run `mixer2 deploy`")?;
        Ok(Deployment {
            vk: record.vk.cell()?,
            pool_lock: record.pool_lock.cell()?,
            pool_type: record.pool_type.cell()?,
            intent_lock: record.intent_lock.cell()?,
        })
    }

    /// The node's copy of each code cell has to hash to what the scripts name.
    fn check_deployment(&self, deployment: &Deployment) -> Result<(), String> {
        for (name, cell) in deployment.cells() {
            let live = self.rpc.live_cell(&cell.out_point)?.ok_or_else(|| format!("the {name} cell is not live"))?;
            if CellOutput::calc_data_hash(&live.data) != cell.data_hash {
                return Err(format!("the {name} cell does not hold the data that was deployed"));
            }
            println!("  {name}: {} bytes on chain, data hash {}", grouped(live.data.len() as u64), hash_hex(&cell.data_hash));
        }
        Ok(())
    }

    fn scripts(&self) -> Result<(Deployment, Scripts), String> {
        let deployment = self.deployment()?;
        let record = self.state.pool.as_ref().ok_or("there is no pool yet: run `mixer2 create`")?;
        let scripts = Scripts::new(&deployment, record.denomination, record.root_history, bytes32(&record.type_id)?);
        Ok((deployment, scripts))
    }

    fn pool(&self) -> Result<(Deployment, Pool), String> {
        let (deployment, scripts) = self.scripts()?;
        let pool = Pool::load(&self.rpc, scripts)?;
        Ok((deployment, pool))
    }

    /// Dry-run `tx` on the node, send it, and note it in the state file.
    fn send(&mut self, step: &str, tx: &TransactionView) -> Result<Byte32, String> {
        let bytes = tx.data().serialized_size_in_block() as u64;
        let accepted = self.rpc.dry_run(tx)?;
        let weight = wallet::weight(bytes, accepted.cycles);
        println!(
            "{step}: {} bytes, {} cycles, weight {} bytes, fee {} CKB ({} shannons per KB of weight)",
            grouped(bytes),
            grouped(accepted.cycles),
            grouped(weight),
            ckb(accepted.fee),
            grouped(accepted.fee * 1000 / weight),
        );
        let hash = self.rpc.send(tx)?;
        println!("  sent {}", hash_hex(&hash));
        self.state.sent.push(SentRecord {
            step: step.to_string(),
            tx_hash: hash_hex(&hash),
            sent_at: now(),
            bytes,
            cycles: accepted.cycles,
            fee: accepted.fee,
        });
        self.save()?;
        Ok(hash)
    }

    /// Wait until the node has committed `hash` and its indexer has caught up.
    fn settle(&self, hash: &Byte32) -> Result<(), String> {
        let committed = self.rpc.wait_committed(hash, WAIT)?;
        self.rpc.wait_indexed(committed.block_number)?;
        println!("  committed in block {} after {}s", committed.block_number, committed.seconds);
        if let Some(cycles) = committed.cycles {
            println!("  the chain recorded {} cycles", grouped(cycles));
        }
        Ok(())
    }

    fn address(&self, key: &Key) -> Result<(), String> {
        let lock = key.lock();
        let plain = self.rpc.plain_cells(&lock)?;
        let spendable: u64 = plain.iter().map(|cell| cell.capacity()).sum();
        let total = self.rpc.total_capacity(&lock)?;
        println!("address    {}", key.address());
        println!("lock args  {}", hex(&lock.args().raw_data()));
        println!("spendable  {} CKB in {} plain cells", ckb(spendable), plain.len());
        println!("other      {} CKB in cells that hold data", ckb(total.saturating_sub(spendable)));
        Ok(())
    }

    /// The v1 deployment's cell at `index`, if it is live and holds `data`.
    fn v1_cell(&self, v1: &V1State, index: u32, recorded_hash: impl Fn(&V1Deployment) -> &str, data: &[u8]) -> Option<CellRecord> {
        let deployment = v1.deployment.as_ref()?;
        if recorded_hash(deployment) != data_hash(data) {
            return None;
        }
        let out_point = OutPoint::new(byte32(&deployment.tx_hash).ok()?, index);
        let live = self.rpc.live_cell(&out_point).ok()??;
        (live.data.as_ref() == data).then(|| CellRecord { tx_hash: deployment.tx_hash.clone(), index, data_hash: data_hash(data) })
    }

    fn deploy(&mut self, variant: &str) -> Result<(), String> {
        if self.state.deployment.is_some() {
            return Err("already deployed; the record is in state-v2.json".into());
        }
        let dir = contracts_dir(variant);
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
        };
        let (pool_type, intent_lock, pool_lock, vk) =
            (read("pool-type-v2")?, read("intent-lock")?, read("pool-lock")?, proof::verifying_key());

        // The verifying key and the pool lock are v1's. Scripts name code by
        // its hash, so a cell that already holds the same bytes serves as well.
        let v1: V1State = load_state(&tool_dir().join("state.json"))?;
        let mut vk_record = self.v1_cell(&v1, 0, |deployment| deployment.vk_hash.as_str(), &vk);
        let mut pool_lock_record = self.v1_cell(&v1, 1, |deployment| deployment.pool_lock_hash.as_str(), &pool_lock);
        for (name, record) in [("verifying key", &vk_record), ("pool-lock", &pool_lock_record)] {
            if let Some(record) = record {
                println!("  {name}: the cell {}:{} of the v1 deployment holds it already", record.tx_hash, record.index);
            }
        }

        let mut stored: Vec<&[u8]> = vec![&pool_type[..], &intent_lock[..]];
        if vk_record.is_none() {
            stored.push(&vk);
        }
        if pool_lock_record.is_none() {
            stored.push(&pool_lock);
        }
        let tx = wallet::fund(&self.rpc, &self.key, store_draft(&self.key.lock(), &stored), 0)?;
        let hash = self.send(&format!("deploy v2 ({variant} build)"), &tx)?;

        let mut next = 0u32;
        let mut record = |data: &[u8]| {
            let record = CellRecord { tx_hash: hash_hex(&hash), index: next, data_hash: data_hash(data) };
            next += 1;
            record
        };
        let (pool_type_record, intent_lock_record) = (record(&pool_type), record(&intent_lock));
        if vk_record.is_none() {
            vk_record = Some(record(&vk));
        }
        if pool_lock_record.is_none() {
            pool_lock_record = Some(record(&pool_lock));
        }
        self.state.deployment = Some(DeploymentRecord {
            variant: variant.to_string(),
            vk: vk_record.expect("set above"),
            pool_lock: pool_lock_record.expect("set above"),
            pool_type: pool_type_record,
            intent_lock: intent_lock_record,
        });
        self.save()?;
        self.settle(&hash)?;
        self.check_deployment(&self.deployment()?)
    }

    fn create(&mut self, denomination: u64, root_history: u16) -> Result<(), String> {
        if self.state.pool.is_some() {
            return Err("there is a pool already; the record is in state-v2.json".into());
        }
        let deployment = self.deployment()?;
        self.check_deployment(&deployment)?;
        let (scripts, tx) = Scripts::create(&self.rpc, &self.key, &deployment, denomination, root_history)?;
        println!("  type id               {}", hex(&scripts.config.type_id));
        println!("  pool type script hash {}", hex(&scripts.type_hash()));
        println!("  pool lock script hash {}", hash_hex(&scripts.lock_script.calc_script_hash()));

        let step = format!("create pool v2 ({} CKB, {root_history} roots)", ckb(denomination));
        let hash = self.send(&step, &tx)?;
        self.state.pool = Some(PoolRecord {
            denomination,
            root_history,
            type_id: hex(&scripts.config.type_id),
            creation_tx: hash_hex(&hash),
        });
        self.save()?;
        self.settle(&hash)
    }

    fn intent(&mut self, count: usize, tip: u64, reclaim_after: u64, hash_type: ScriptHashType) -> Result<(), String> {
        let (_, scripts) = self.scripts()?;
        let owner = self.key.lock();
        let capacity = scripts.config.denomination + tip;
        let first_note = self.state.notes.len();

        let mut builder = TransactionBuilder::default();
        for _ in 0..count {
            let note = Note::random()?;
            let args = scripts.intent_args(fr_to_le_bytes(&note.commitment()), &owner, reclaim_after);
            let cell = plain_cell(&scripts.intent_lock(hash_type, &args), capacity);
            let least = occupied(&cell, 0);
            if capacity < least {
                return Err(format!(
                    "an intent cell occupies {} CKB; the denomination and the tip come to {} CKB",
                    ckb(least),
                    ckb(capacity)
                ));
            }
            builder = builder.output(cell).output_data(ckb_types::bytes::Bytes::new().pack());
            self.state.notes.push(note.record());
        }
        // The secrets first: a deposit without them cannot be withdrawn.
        self.save()?;

        let tx = wallet::fund(&self.rpc, &self.key, builder.build(), 0)?;
        let mut step = match count {
            1 => format!("intent for note {first_note}"),
            _ => format!("intents for notes {first_note}–{}", first_note + count - 1),
        };
        if hash_type != HASH_TYPE {
            step += &format!(", lock named by {hash_type:?}");
        }
        let hash = self.send(&step, &tx)?;
        self.settle(&hash)?;
        println!(
            "  {count} intent cell(s) of {} CKB each; the owner can take them back {reclaim_after} blocks from now",
            ckb(capacity)
        );
        Ok(())
    }

    fn note(&self, index: usize) -> Result<Note, String> {
        Note::from_record(self.state.notes.get(index).ok_or_else(|| format!("there is no note {index}"))?)
    }

    /// Prove that note `index` of the state file is in the pool, for a payout
    /// to `recipient`.
    fn prove(&self, deployment: &Deployment, pool: &Pool, index: usize, recipient: &Script, fee: u64) -> Result<Payout, String> {
        let note = self.note(index)?;
        let leaf = pool.leaf_index(&note.commitment()).ok_or_else(|| format!("note {index} is not in the pool"))?;
        let nullifier_hash = fr_to_le_bytes(&hash2(&note.nullifier, &Fr::from(leaf)));
        if pool.nullifiers.contains(&nullifier_hash) {
            return Err(format!("note {index} is withdrawn already"));
        }

        let started = Instant::now();
        let withdrawal = proof::prove(&pool.leaves, leaf, note.nullifier, note.secret, recipient, fee)?;
        // Check it with the verifier that is linked into the pool type script,
        // and with the key the pool names.
        let vk = proof::verifying_key();
        if CellOutput::calc_data_hash(&vk) != deployment.vk.data_hash {
            return Err("the local verifying key is not the one the pool names".into());
        }
        proof::verify(&vk, &withdrawal)?;
        println!(
            "  note {index} is leaf {leaf} of {}; proved in {:.1}s, and the proof verifies off chain",
            pool.leaves.len(),
            started.elapsed().as_secs_f32()
        );
        Ok(Payout { withdrawal, recipient: recipient.clone() })
    }

    /// Notes of the state file that are in the pool and not withdrawn.
    fn unspent(&self, pool: &Pool) -> Vec<usize> {
        (0..self.state.notes.len())
            .filter(|index| {
                let Ok(note) = self.note(*index) else { return false };
                pool.leaf_index(&note.commitment())
                    .is_some_and(|leaf| !pool.nullifiers.contains(&fr_to_le_bytes(&hash2(&note.nullifier, &Fr::from(leaf)))))
            })
            .collect()
    }

    fn request(&mut self, index: usize, fee: u64, to_self: bool) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let recipient = if to_self {
            self.key.lock()
        } else {
            // A key that has never been on chain.
            let path = tool_dir().join(format!("recipient-v2-{index}.key"));
            let key = if path.exists() { Key::load(&path)? } else { Key::generate(&path)? };
            println!("  note {index} pays {}", key.address());
            key.lock()
        };
        let payout = self.prove(&deployment, &pool, index, &recipient, fee)?;

        let dir = requests_dir();
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = dir.join(format!("{}.json", &hex(&payout.withdrawal.nullifier_hash)[2..18]));
        let message = Message::new(&pool.scripts.type_hash(), &payout);
        let text = serde_json::to_string_pretty(&message).map_err(|error| error.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|error| format!("{}: {error}", path.display()))?;
        println!("  wrote {}", path.display());
        Ok(())
    }

    /// The withdrawals asked for in `requests/` that the pool would pay now,
    /// the ones offering the most first.
    fn requested(&self, deployment: &Deployment, pool: &Pool) -> Result<Vec<Payout>, String> {
        let dir = requests_dir();
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries.filter_map(|entry| Some(entry.ok()?.path())).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        };
        paths.retain(|path| path.extension().is_some_and(|extension| extension == "json"));
        paths.sort();

        let vk = proof::verifying_key();
        if CellOutput::calc_data_hash(&vk) != deployment.vk.data_hash {
            return Err("the local verifying key is not the one the pool names".into());
        }
        let denomination = pool.scripts.config.denomination;
        let mut payouts: Vec<Payout> = Vec::new();
        let mut paid = 0;
        for path in &paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let check = |path: &Path| -> Result<Option<Payout>, String> {
                let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
                let message: Message = serde_json::from_str(&text).map_err(|error| error.to_string())?;
                let (pool_hash, payout) = message.payout()?;
                let withdrawal = &payout.withdrawal;
                if pool_hash != pool.scripts.type_hash() {
                    return Err("it is for another pool".into());
                }
                if pool.nullifiers.contains(&withdrawal.nullifier_hash) {
                    return Ok(None);
                }
                if !pool.state.is_known_root(&withdrawal.root) {
                    return Err("its root is not in the pool's history any more".into());
                }
                let recipient = plain_cell(&payout.recipient, denomination.saturating_sub(withdrawal.fee));
                if withdrawal.fee > denomination || Unpack::<u64>::unpack(&recipient.capacity()) < occupied(&recipient, 0) {
                    return Err("the payout is less than the recipient's cell occupies".into());
                }
                proof::verify(&vk, withdrawal).map_err(|error| format!("its proof does not verify: {error}"))?;
                Ok(Some(payout))
            };
            match check(path) {
                Ok(Some(payout)) if payouts.iter().any(|other| other.withdrawal.nullifier_hash == payout.withdrawal.nullifier_hash) => {
                    println!("  skipped {name}: the same note is requested twice");
                }
                Ok(Some(payout)) => payouts.push(payout),
                Ok(None) => paid += 1,
                Err(reason) => println!("  skipped {name}: {reason}"),
            }
        }
        if paid > 0 {
            println!("  {paid} request(s) in {} are paid already", dir.display());
        }
        payouts.sort_by_key(|payout| std::cmp::Reverse(payout.withdrawal.fee));
        Ok(payouts)
    }

    fn aggregate(&mut self, max_deposits: usize, max_withdrawals: usize, key: Option<Key>, fee_rate: u64, wait: bool) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let key = key.as_ref().unwrap_or(&self.key);
        let scripts = &pool.scripts;

        let mut intents = Vec::new();
        for intent in scripts.intents(&self.rpc, HASH_TYPE)? {
            match scripts.refusal(&intent, 0) {
                None => intents.push(intent),
                Some(reason) => println!("  left intent {} alone: {reason}", hash_hex(&intent.cell.out_point.tx_hash())),
            }
        }
        intents.sort_by_key(|intent| std::cmp::Reverse(intent.cell.capacity()));
        intents.truncate(max_deposits);
        let mut payouts = self.requested(&deployment, &pool)?;
        payouts.truncate(max_withdrawals);
        if intents.is_empty() && payouts.is_empty() {
            return Err("there is nothing to settle: no intent is waiting and no withdrawal is requested".into());
        }

        let denomination = scripts.config.denomination;
        let tips: u64 = intents.iter().map(|intent| intent.cell.capacity() - denomination).sum();
        let fees: u64 = payouts.iter().map(|payout| payout.withdrawal.fee).sum();
        println!(
            "  batch: {} deposits tipping {} CKB, {} withdrawals offering {} CKB",
            intents.len(),
            ckb(tips),
            payouts.len(),
            ckb(fees)
        );

        let tx = self.batch_tx(key, &deployment, &pool, &intents, &payouts, fee_rate)?;
        let fee = pool.input_capacity(&intents) + funding(&tx, intents.len() + 1, &self.rpc)? - outputs(&tx);

        let step = format!("batch: {} deposits, {} withdrawals", intents.len(), payouts.len());
        let hash = self.send(&step, &tx)?;
        println!(
            "  the aggregator takes in {} CKB and pays {} CKB to the miner",
            ckb(tips + fees),
            ckb(fee)
        );
        if !wait {
            return Ok(());
        }
        self.settle(&hash)?;

        let (_, pool) = self.pool()?;
        println!(
            "  the pool holds {} CKB: {} deposits, {} withdrawals so far",
            ckb(pool.cell.capacity()),
            pool.leaves.len(),
            pool.nullifiers.len()
        );
        Ok(())
    }

    /// The batch that deposits `intents` and pays `payouts`, completed by `key`:
    /// the aggregator adds a cell of its own and takes everything the pool and
    /// the recipients do not get as change. What the transaction has to pay
    /// depends on its cycles, which only a dry run tells.
    fn batch_tx(
        &self,
        key: &Key,
        deployment: &Deployment,
        pool: &Pool,
        intents: &[Intent],
        payouts: &[Payout],
        fee_rate: u64,
    ) -> Result<TransactionView, String> {
        let draft = pool.batch_draft(deployment, intents, payouts)?;
        let brought = pool.input_capacity(intents);
        let bytes = wallet::fund(&self.rpc, key, draft.clone(), brought)?.data().serialized_size_in_block() as u64;
        let by_size = (bytes * fee_rate).div_ceil(1000);
        let tx = wallet::fund_with(&self.rpc, key, draft.clone(), brought, by_size, &|tx| tx)?;
        let estimate = self.rpc.dry_run(&tx)?;
        let fee = (wallet::weight(bytes, estimate.cycles) * fee_rate).div_ceil(1000).max(by_size);
        if fee == by_size {
            return Ok(tx);
        }
        wallet::fund_with(&self.rpc, key, draft, brought, fee, &|tx| tx)
    }

    /// Two aggregators go for the same intents and the same pool cell. The
    /// first one's batch is sent; the rival's, built before that at each of
    /// `rates`, is then offered to the node, lowest rate first, until one is
    /// taken. Reports what the node says to each, and which batch is mined.
    fn race(&mut self, rival: &Key, max_deposits: usize, rates: &[u64]) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let scripts = &pool.scripts;
        let mut intents: Vec<Intent> =
            scripts.intents(&self.rpc, HASH_TYPE)?.into_iter().filter(|intent| scripts.refusal(intent, 0).is_none()).collect();
        intents.truncate(max_deposits);
        if intents.is_empty() {
            return Err("no intent is waiting: `mixer2 intent`".into());
        }
        let brought = pool.input_capacity(&intents);
        let fee_of = |tx: &TransactionView| -> Result<u64, String> {
            Ok(brought + funding(tx, intents.len() + 1, &self.rpc)? - outputs(tx))
        };

        let first = self.batch_tx(&self.key, &deployment, &pool, &intents, &[], FEE_RATE)?;
        let mut rivals = Vec::new();
        for rate in rates {
            let tx = self.batch_tx(rival, &deployment, &pool, &intents, &[], *rate)?;
            rivals.push((*rate, fee_of(&tx)?, tx));
        }
        let first_fee = fee_of(&first)?;
        let first_hash = self.send(&format!("race: the first aggregator's batch of {} deposits", intents.len()), &first)?;

        // Sent without a dry run of its own: `test_tx_pool_accept` does not
        // apply the replacement rule, `send_transaction` does.
        let mut replacement = None;
        for (rate, fee, tx) in &rivals {
            let bytes = tx.data().serialized_size_in_block() as u64;
            let offer = format!(
                "the rival's batch at {} shannons/KB (fee {} shannons, {} more than the first, {} bytes)",
                grouped(*rate),
                grouped(*fee),
                grouped(fee.saturating_sub(first_fee)),
                grouped(bytes),
            );
            let dry_run = match self.rpc.dry_run(tx) {
                Ok(_) => "accepts it".to_string(),
                Err(message) => format!("refuses it, {message}"),
            };
            println!("  {offer}: a dry run {dry_run}");
            match self.rpc.send(tx) {
                Ok(hash) => {
                    println!("  {offer}: the node takes it, {}", hash_hex(&hash));
                    self.state.sent.push(SentRecord {
                        step: format!("race: the rival's batch, {} shannons/KB", grouped(*rate)),
                        tx_hash: hash_hex(&hash),
                        sent_at: now(),
                        bytes,
                        cycles: 0,
                        fee: *fee,
                    });
                    self.save()?;
                    replacement = Some(hash);
                    break;
                }
                Err(message) => println!("  {offer}: refused, {message}"),
            }
        }

        let mut candidates = vec![("the first aggregator's batch", first_hash)];
        candidates.extend(replacement.map(|hash| ("the rival's batch", hash)));
        let started = Instant::now();
        let winner = 'poll: loop {
            for (name, hash) in &candidates {
                if let Some(found) = self.rpc.transaction(hash)? {
                    if found.status == "committed" {
                        break 'poll (*name, found.block_number.unwrap_or(0));
                    }
                }
            }
            if started.elapsed() > WAIT {
                return Err("neither batch was committed in time".into());
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        };
        self.rpc.wait_indexed(winner.1)?;
        println!("  committed in block {}: {}", winner.1, winner.0);
        for (name, hash) in &candidates {
            let status = match self.rpc.transaction(hash)? {
                Some(found) => found.status,
                None => "the node no longer has it".to_string(),
            };
            println!("  {name} {}: {status}", hash_hex(hash));
        }
        let (_, pool) = self.pool()?;
        println!("  the pool holds {} CKB: {} deposits, {} withdrawals so far", ckb(pool.cell.capacity()), pool.leaves.len(), pool.nullifiers.len());
        Ok(())
    }

    /// The live intent cells of note `index`, under either hash type.
    fn intents_of(&self, scripts: &Scripts, index: usize) -> Result<Vec<Intent>, String> {
        let commitment = fr_to_le_bytes(&self.note(index)?.commitment());
        let mut found = Vec::new();
        for hash_type in [ScriptHashType::Data2, ScriptHashType::Data1] {
            found.extend(scripts.intents(&self.rpc, hash_type)?.into_iter().filter(|intent| intent.args.commitment == commitment));
        }
        Ok(found)
    }

    fn reclaim(&mut self, index: usize) -> Result<(), String> {
        let (deployment, scripts) = self.scripts()?;
        let intent = self
            .intents_of(&scripts, index)?
            .into_iter()
            .next()
            .ok_or_else(|| format!("no intent of note {index} is waiting"))?;
        println!(
            "  the intent holds {} CKB and can be taken back {} blocks after it was made; the tip is block {}",
            ckb(intent.cell.capacity()),
            grouped(intent.args.reclaim_after),
            self.rpc.tip()?
        );
        if intent.args.reclaim_after > LONGEST_WAIT {
            return Err("its wait is longer than any since can express: it can be deposited, but never taken back".into());
        }
        let draft = reclaim_draft(&deployment, &intent, core::reclaim_since(intent.args.reclaim_after));
        let tx = wallet::fund(&self.rpc, &self.key, draft, intent.cell.capacity())?;
        let hash = self.send(&format!("take back the intent of note {index}"), &tx)?;
        self.settle(&hash)
    }

    /// Transactions an attacker would try, dry-run on the node: each has to be
    /// refused by the script named, for the reason named. Nothing is sent.
    fn attack(&mut self) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let scripts = &pool.scripts;
        let denomination = scripts.config.denomination;
        let me = self.key.lock();
        let my_lock_hash: Bytes32 = me.calc_script_hash().as_slice().try_into().expect("32 bytes");
        let fund = |draft: TransactionView, brought: u64| wallet::fund(&self.rpc, &self.key, draft, brought);
        let mut cases: Vec<(String, TransactionView, Refusal)> = Vec::new();

        // ---- the pool on its own
        cases.push((
            "a batch that neither deposits nor withdraws".into(),
            fund(pool.assemble(&deployment, &[], &[], &[], pool.cell.capacity(), pool.state.to_bytes()), pool.cell.capacity())?,
            Refusal::Pool(Error::EmptyBatch),
        ));

        // ---- intents
        let honest: Vec<Intent> =
            scripts.intents(&self.rpc, HASH_TYPE)?.into_iter().filter(|intent| scripts.refusal(intent, 0).is_none()).collect();
        match honest.first() {
            None => println!("  (no intent is waiting, so the intent cases are skipped)"),
            Some(intent) => {
                let one = std::slice::from_ref(intent);
                let brought = pool.input_capacity(one);
                let mut deposited = pool.state.clone();
                deposited.deposit(&[intent.args.commitment]).map_err(|error| format!("{error:?}"))?;

                cases.push((
                    "an intent spent next to the pool, the pool left as it was".into(),
                    fund(pool.assemble(&deployment, one, &[], &[], pool.cell.capacity(), pool.state.to_bytes()), brought)?,
                    Refusal::Pool(Error::OutputState),
                ));
                cases.push((
                    "an intent deposited, its denomination kept out of the pool".into(),
                    fund(pool.assemble(&deployment, one, &[], &[], pool.cell.capacity(), deposited.to_bytes()), brought)?,
                    Refusal::Pool(Error::OutputCapacity),
                ));

                if intent.args.owner_lock_hash == my_lock_hash {
                    let reclaim = |since: u64| fund(reclaim_draft(&deployment, intent, since), intent.cell.capacity());
                    cases.push((
                        "the owner takes an intent back with no since".into(),
                        reclaim(0)?,
                        Refusal::Intent(IntentError::ReclaimTooEarly),
                    ));
                    if intent.args.reclaim_after > 0 {
                        cases.push((
                            "the owner takes an intent back with a since of 0 blocks".into(),
                            reclaim(core::reclaim_since(0))?,
                            Refusal::Intent(IntentError::ReclaimTooEarly),
                        ));
                    }
                    // Not a script's business: the since is right, and whether
                    // the cell is old enough is for the node to say.
                    if intent.args.reclaim_after > LONGEST_WAIT {
                        println!("  note  this intent asks for a wait no since can express; nobody can take it back");
                    } else {
                        match self.rpc.dry_run(&reclaim(core::reclaim_since(intent.args.reclaim_after))?) {
                            Ok(accepted) => println!(
                                "  note  the owner takes it back with the since it asks for: the node would take it ({} cycles); the wait is over",
                                grouped(accepted.cycles)
                            ),
                            Err(message) => println!("  note  the owner takes it back with the since it asks for: the node says {message}"),
                        }
                    }
                }

                // Somebody else, who holds a cell of their own to pay with.
                let stranger_path = tool_dir().join("stranger.key");
                match stranger_path.exists().then(|| Key::load(&stranger_path)).transpose()? {
                    Some(stranger) if !self.rpc.plain_cells(&stranger.lock())?.is_empty() => cases.push((
                        "somebody else takes an intent without the pool".into(),
                        wallet::fund(&self.rpc, &stranger, reclaim_draft(&deployment, intent, 0), intent.cell.capacity())?,
                        Refusal::Intent(IntentError::Unauthorized),
                    )),
                    _ => println!("  (stranger.key holds nothing, so nobody else tries to take an intent: `mixer2 pay --key stranger.key --ckb 100`)"),
                }

                // An intent whose lock is the same code under another hash type
                // is not a deposit to the pool. Spent next to the pool, its
                // capacity would simply be left over for whoever builds the
                // transaction, unless its own lock objects.
                let uncounted = scripts.intents(&self.rpc, ScriptHashType::Data1)?;
                match uncounted.first() {
                    None => println!("  (no intent under another hash type exists: `mixer2 intent --hash-type data1`)"),
                    Some(other) => {
                        let both = [intent.clone(), other.clone()];
                        let draft =
                            pool.assemble(&deployment, &both, &[], &[], pool.cell.capacity() + denomination, deposited.to_bytes());
                        cases.push((
                            "an intent the pool does not count, taken next to the pool".into(),
                            fund(draft, pool.input_capacity(&both))?,
                            Refusal::Intent(IntentError::PoolMismatch),
                        ));
                    }
                }
            }
        }

        // ---- withdrawals
        let replayed = pool.steps.iter().rev().find_map(|step| match &step.event {
            Event::Settled { payouts, .. } => payouts.first().cloned(),
            Event::Created => None,
        });
        let holds = (pool.leaves.len() - pool.nullifiers.len()) as u64;
        match replayed {
            Some(payout) if holds >= 1 => cases.push((
                "a committed withdrawal, replayed".into(),
                pool.assemble(
                    &deployment,
                    &[],
                    std::slice::from_ref(&payout),
                    &[pool.nullifiers.proof(&payout.withdrawal.nullifier_hash)],
                    pool.cell.capacity() - denomination,
                    pool.state.to_bytes(),
                ),
                Refusal::Pool(Error::NullifierSpent),
            )),
            _ => println!("  (no withdrawal on chain yet, or the pool is empty, so nothing is replayed)"),
        }

        let unspent = self.unspent(&pool);
        match unspent.first() {
            None => println!("  (no unspent note of ours is in the pool, so the withdrawal cases are skipped)"),
            Some(&index) => {
                let victim = wallet::secp_lock(&wallet::random_bytes::<20>()?);
                let payout = self.prove(&deployment, &pool, index, &victim, WITHDRAW_FEE)?;
                let one = std::slice::from_ref(&payout);
                let honest = pool.batch_draft(&deployment, &[], one)?;
                let accepted = self.rpc.dry_run(&honest)?;
                println!("  ok    the withdrawal as proved, alone in a batch: accepted, {} cycles", grouped(accepted.cycles));

                let to_me = Payout { recipient: me.clone(), ..payout.clone() };
                cases.push((
                    "the same proof, paid to another lock".into(),
                    pool.batch_draft(&deployment, &[], &[to_me])?,
                    Refusal::Pool(Error::ProofRejected),
                ));
                let mut greedy = payout.clone();
                greedy.withdrawal.fee += SHANNONS;
                cases.push((
                    "the same proof, with a higher fee".into(),
                    pool.batch_draft(&deployment, &[], &[greedy])?,
                    Refusal::Pool(Error::ProofRejected),
                ));
                let mut outputs: Vec<CellOutput> = honest.outputs().into_iter().collect();
                outputs[1] = outputs[1].clone().as_builder().capacity(denomination - WITHDRAW_FEE - SHANNONS).build();
                cases.push((
                    "the same proof, the recipient short-changed".into(),
                    honest.as_advanced_builder().set_outputs(outputs).build(),
                    Refusal::Pool(Error::RecipientCapacity),
                ));

                if holds >= 2 {
                    let mut nullifiers = pool.nullifiers.clone();
                    let first = nullifiers.proof(&payout.withdrawal.nullifier_hash);
                    nullifiers.insert(payout.withdrawal.nullifier_hash);
                    let second = nullifiers.proof(&payout.withdrawal.nullifier_hash);
                    cases.push((
                        "one note withdrawn twice in one batch".into(),
                        pool.assemble(
                            &deployment,
                            &[],
                            &[payout.clone(), payout.clone()],
                            &[first, second],
                            pool.cell.capacity() - 2 * denomination,
                            pool.state.to_bytes(),
                        ),
                        Refusal::Pool(Error::NullifierSpent),
                    ));
                }
                if let Some(&other_index) = unspent.get(1) {
                    let other_victim = wallet::secp_lock(&wallet::random_bytes::<20>()?);
                    let other = self.prove(&deployment, &pool, other_index, &other_victim, WITHDRAW_FEE)?;
                    let swapped = [
                        Payout { recipient: other.recipient.clone(), ..payout.clone() },
                        Payout { recipient: payout.recipient.clone(), ..other.clone() },
                    ];
                    cases.push((
                        "two withdrawals, their recipients swapped".into(),
                        pool.batch_draft(&deployment, &[], &swapped)?,
                        Refusal::Pool(Error::ProofRejected),
                    ));
                }
            }
        }

        let mut failed = 0;
        for (name, tx, refusal) in &cases {
            match self.rpc.dry_run(tx) {
                Err(message) if message.contains(&format!("error code {} ", refusal.code())) => {
                    let source = message.split("source: ").nth(1).and_then(|rest| rest.split(',').next()).unwrap_or("?");
                    println!("  ok    {name}: refused by {source}, exit code {} ({})", refusal.code(), refusal.name());
                }
                Err(message) => {
                    failed += 1;
                    println!("  FAIL  {name}: expected exit code {} ({}), got: {message}", refusal.code(), refusal.name());
                }
                Ok(_) => {
                    failed += 1;
                    println!("  FAIL  {name}: the node would accept it");
                }
            }
        }
        match failed {
            0 => Ok(()),
            _ => Err(format!("{failed} of {} transactions were not refused as expected", cases.len())),
        }
    }

    fn pay(&mut self, path: &Path, amount: u64) -> Result<(), String> {
        let key = if path.exists() { Key::load(path)? } else { Key::generate(path)? };
        println!("  paying {}", key.address());
        let draft = TransactionBuilder::default().output(plain_cell(&key.lock(), amount)).output_data(ckb_types::bytes::Bytes::new().pack()).build();
        let tx = wallet::fund(&self.rpc, &self.key, draft, 0)?;
        let hash = self.send(&format!("pay {} CKB to another key", ckb(amount)), &tx)?;
        self.settle(&hash)
    }

    fn info(&self) -> Result<(), String> {
        if let Some(record) = &self.state.deployment {
            println!("deployment ({} build):", record.variant);
            for (name, cell) in
                [("verifying key", &record.vk), ("pool-lock", &record.pool_lock), ("pool-type-v2", &record.pool_type), ("intent-lock", &record.intent_lock)]
            {
                println!("  {name:<14} {}  in {}:{}", cell.data_hash, cell.tx_hash, cell.index);
            }
        }
        if self.state.pool.is_some() {
            let (_, pool) = self.pool()?;
            let scripts = &pool.scripts;
            let out_point = &pool.cell.out_point;
            let index: u32 = out_point.index().unpack();
            println!("pool cell {}:{index}", hash_hex(&out_point.tx_hash()));
            println!("  type script hash {}", hex(&scripts.type_hash()));
            println!(
                "  capacity {} CKB, denomination {} CKB, {} roots kept, {} bytes of state",
                ckb(pool.cell.capacity()),
                ckb(scripts.config.denomination),
                scripts.config.root_history,
                pool.cell.data.len()
            );
            println!("  {} deposits, {} withdrawals", pool.leaves.len(), pool.nullifiers.len());
            println!("  commitment root {}", hex(pool.state.current_root()));
            println!("  nullifier root  {}", hex(&pool.state.nullifier_root));
            println!("history, rebuilt from the chain:");
            for step in &pool.steps {
                let what = match &step.event {
                    Event::Created => "created".to_string(),
                    Event::Settled { first_leaf, deposits, payouts } => {
                        let leaves = match deposits {
                            0 => String::new(),
                            1 => format!(" (leaf {first_leaf})"),
                            _ => format!(" (leaves {first_leaf}–{})", first_leaf + deposits - 1),
                        };
                        format!("batch: {deposits} deposits{leaves}, {} withdrawals", payouts.len())
                    }
                };
                println!("  block {:>8}  {}  {what}", step.block_number.unwrap_or(0), hash_hex(&step.tx_hash));
            }

            let mut waiting = Vec::new();
            for hash_type in [ScriptHashType::Data2, ScriptHashType::Data1] {
                for intent in scripts.intents(&self.rpc, hash_type)? {
                    waiting.push((hash_type, intent));
                }
            }
            println!("{} intent cell(s) waiting", waiting.len());
            for (index, record) in self.state.notes.iter().enumerate() {
                let note = Note::from_record(record)?;
                let commitment = fr_to_le_bytes(&note.commitment());
                let status = match pool.leaf_index(&note.commitment()) {
                    Some(leaf) => {
                        let nullifier_hash = fr_to_le_bytes(&hash2(&note.nullifier, &Fr::from(leaf)));
                        let spent = if pool.nullifiers.contains(&nullifier_hash) { "withdrawn" } else { "unspent" };
                        format!("leaf {leaf}, {spent}")
                    }
                    None => match waiting.iter().find(|(_, intent)| intent.args.commitment == commitment) {
                        Some((hash_type, intent)) => {
                            format!("an intent of {} CKB is waiting (lock named by {hash_type:?})", ckb(intent.cell.capacity()))
                        }
                        None => "not in the pool, and no intent is waiting".to_string(),
                    },
                };
                println!("  note {index}: {status}");
            }
        }
        print_sent(&self.rpc, &self.state.sent)
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or(USAGE)?;
    let mut options = Options::parse(args, USAGE)?;
    let note = |options: &mut Options| options.take::<usize>("note")?.ok_or("--note <n> is required".to_string());
    // A key file, by name, next to this crate.
    let key_path = |options: &mut Options| -> Result<Option<PathBuf>, String> {
        Ok(options.take::<String>("key")?.map(|name| tool_dir().join(name)))
    };

    match command.as_str() {
        "address" => {
            let path = key_path(&mut options)?;
            options.done()?;
            let mixer = Mixer::open()?;
            match path {
                Some(path) => mixer.address(&Key::load(&path)?),
                None => mixer.address(&mixer.key),
            }
        }
        "deploy" => {
            let variant = options.take::<String>("variant")?.unwrap_or_else(|| "size".to_string());
            options.done()?;
            Mixer::open()?.deploy(&variant)
        }
        "create" => {
            let denomination = options.take::<u64>("denomination")?.unwrap_or(150);
            let root_history = options.take::<u16>("root-history")?.unwrap_or(32);
            options.done()?;
            if root_history == 0 {
                return Err("--root-history has to be at least 1".into());
            }
            Mixer::open()?.create(denomination * SHANNONS, root_history)
        }
        "intent" => {
            let count = options.take::<usize>("count")?.unwrap_or(1);
            let tip = options.take::<u64>("tip")?.unwrap_or(1);
            let reclaim_after = options.take::<u64>("reclaim-after")?.unwrap_or(100);
            let hash_type = match options.take::<String>("hash-type")?.as_deref() {
                None | Some("data2") => ScriptHashType::Data2,
                Some("data1") => ScriptHashType::Data1,
                Some(other) => return Err(format!("--hash-type: data2 or data1, not {other}")),
            };
            options.done()?;
            if count == 0 {
                return Err("--count has to be at least 1".into());
            }
            if reclaim_after > LONGEST_WAIT {
                return Err("--reclaim-after: no since can express that many blocks".into());
            }
            Mixer::open()?.intent(count, tip * SHANNONS, reclaim_after, hash_type)
        }
        "request" => {
            let index = note(&mut options)?;
            let fee = options.take::<u64>("fee")?.unwrap_or(WITHDRAW_FEE);
            let to_self = match options.take::<String>("to")?.as_deref() {
                None => false,
                Some("self") => true,
                Some(other) => return Err(format!("--to: only `self` is known, not {other}")),
            };
            options.done()?;
            Mixer::open()?.request(index, fee, to_self)
        }
        "aggregate" => {
            let max_deposits = options.take::<usize>("deposits")?.unwrap_or(usize::MAX);
            let max_withdrawals = options.take::<usize>("withdrawals")?.unwrap_or(usize::MAX);
            let path = key_path(&mut options)?;
            let fee_rate = options.take::<u64>("fee-rate")?.unwrap_or(FEE_RATE);
            let wait = match options.take::<String>("wait")?.as_deref() {
                None | Some("yes") => true,
                Some("no") => false,
                Some(other) => return Err(format!("--wait: yes or no, not {other}")),
            };
            options.done()?;
            let key = path.map(|path| Key::load(&path)).transpose()?;
            Mixer::open()?.aggregate(max_deposits, max_withdrawals, key, fee_rate, wait)
        }
        "race" => {
            let path = key_path(&mut options)?.ok_or("--key <file> is required")?;
            let max_deposits = options.take::<usize>("deposits")?.unwrap_or(2);
            let rates = options.take::<String>("rates")?.ok_or("--rates <a,b,..> is required")?;
            options.done()?;
            let rates: Vec<u64> =
                rates.split(',').map(|rate| rate.parse().map_err(|_| format!("--rates: cannot read {rate}"))).collect::<Result<_, _>>()?;
            Mixer::open()?.race(&Key::load(&path)?, max_deposits, &rates)
        }
        "reclaim" => {
            let index = note(&mut options)?;
            options.done()?;
            Mixer::open()?.reclaim(index)
        }
        "attack" => {
            options.done()?;
            Mixer::open()?.attack()
        }
        "pay" => {
            let path = key_path(&mut options)?.ok_or("--key <file> is required")?;
            let amount = options.take::<u64>("ckb")?.ok_or("--ckb <n> is required")?;
            options.done()?;
            Mixer::open()?.pay(&path, amount * SHANNONS)
        }
        "info" => {
            options.done()?;
            Mixer::open()?.info()
        }
        _ => Err(format!("unknown command: {command}\n\n{USAGE}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
