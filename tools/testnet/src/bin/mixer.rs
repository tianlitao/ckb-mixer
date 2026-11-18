//! `mixer <command>`: the mixer PoC on CKB testnet.
//!
//! The chain is the source of truth. `state.json` next to this crate keeps only
//! what the chain cannot give back: which transaction deployed the code, which
//! one created the pool, the secrets of the notes deposited here, and when each
//! transaction was sent. The commitment tree and the nullifier set are rebuilt
//! from the pool's transactions on every run.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ckb_types::{
    core::TransactionView,
    packed::{Byte32, CellOutput, Script},
    prelude::*,
};
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes};
use mixer_core::poseidon::hash2;
use mixer_core::{Bytes32, Error, Fr};
use mixer_testnet::pool::{Deployment, Event, Pool, Scripts};
use mixer_testnet::proof::{self, Withdrawal};
use mixer_testnet::rpc::{hash_hex, Rpc};
use mixer_testnet::wallet::{self, hex, unhex, Key, SHANNONS};
use mixer_testnet::{EXPLORER_TX, TESTNET_RPC};
use serde::{Deserialize, Serialize};

const USAGE: &str = "usage: mixer <command> [--option value]...

  address                               the funding account and what it holds
  deploy   [--variant size]             store the verifying key and both scripts
  create   [--denomination 100]         create a pool; denomination in CKB
  deposit  [--count 1]                  deposit fresh notes in one transaction
  withdraw --note <n> [--fee 1000000]   withdraw note n to a fresh address; fee in shannons
           [--after-deposits <k>]       ... and send it only after k more deposits have landed
  attack   --note <n>                   dry-run transactions the pool has to refuse
  chain    --note <n>                   a deposit and two withdrawals, each built on the one before,
                                        sent without waiting for a block in between
  info                                  the pool as the chain has it, and what was sent";

/// What a withdrawal leaves to the miner unless told otherwise: 0.01 CKB.
const WITHDRAW_FEE: u64 = 1_000_000;
/// How long to wait for a transaction to be committed.
const WAIT: Duration = Duration::from_secs(540);

#[derive(Serialize, Deserialize, Default)]
struct State {
    deployment: Option<DeploymentRecord>,
    pool: Option<PoolRecord>,
    notes: Vec<NoteRecord>,
    sent: Vec<SentRecord>,
}

#[derive(Serialize, Deserialize)]
struct DeploymentRecord {
    variant: String,
    tx_hash: String,
    vk_hash: String,
    pool_lock_hash: String,
    pool_type_hash: String,
}

#[derive(Serialize, Deserialize)]
struct PoolRecord {
    /// Shannons.
    denomination: u64,
    creation_tx: String,
}

/// A note's secrets, as 32-byte little-endian field elements.
#[derive(Serialize, Deserialize)]
struct NoteRecord {
    nullifier: String,
    secret: String,
    commitment: String,
}

/// A transaction this tool sent, with what the node's dry run said about it.
#[derive(Serialize, Deserialize)]
struct SentRecord {
    step: String,
    tx_hash: String,
    /// Seconds since the epoch.
    sent_at: u64,
    bytes: u64,
    cycles: u64,
    fee: u64,
}

struct Note {
    nullifier: Fr,
    secret: Fr,
}

impl Note {
    fn random() -> Result<Self, String> {
        // 31 random bytes are always below the field's modulus.
        let element = || -> Result<Fr, String> {
            let mut bytes = [0u8; 32];
            bytes[..31].copy_from_slice(&wallet::random_bytes::<31>()?);
            Ok(fr_from_le_bytes(&bytes).expect("below the modulus"))
        };
        Ok(Self { nullifier: element()?, secret: element()? })
    }

    fn commitment(&self) -> Fr {
        hash2(&self.nullifier, &self.secret)
    }

    fn record(&self) -> NoteRecord {
        NoteRecord {
            nullifier: hex(&fr_to_le_bytes(&self.nullifier)),
            secret: hex(&fr_to_le_bytes(&self.secret)),
            commitment: hex(&fr_to_le_bytes(&self.commitment())),
        }
    }

    fn from_record(record: &NoteRecord) -> Result<Self, String> {
        let element = |text: &str| fr_from_le_bytes(&bytes32(text)?).ok_or_else(|| format!("not a field element: {text}"));
        let note = Self { nullifier: element(&record.nullifier)?, secret: element(&record.secret)? };
        if hex(&fr_to_le_bytes(&note.commitment())) != record.commitment {
            return Err(format!("the secrets of note {} do not give its commitment", record.commitment));
        }
        Ok(note)
    }
}

fn tool_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where `scripts/build-contracts.sh` leaves the binaries of a build variant.
fn contracts_dir(variant: &str) -> PathBuf {
    let target = match variant {
        "default" => "target".to_string(),
        other => format!("target-{other}"),
    };
    mixer_prover::repo_root().join("contracts").join(target).join("riscv64imac-unknown-none-elf/release")
}

fn bytes32(text: &str) -> Result<Bytes32, String> {
    unhex(text)?.try_into().map_err(|_| format!("not 32 bytes: {text}"))
}

fn byte32(text: &str) -> Result<Byte32, String> {
    Ok(bytes32(text)?.pack())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// `1234567` as `1,234,567`.
fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Shannons as CKB, without trailing zeros.
fn ckb(shannons: u64) -> String {
    let whole = grouped(shannons / SHANNONS);
    match shannons % SHANNONS {
        0 => whole,
        fraction => format!("{whole}.{}", format!("{fraction:08}").trim_end_matches('0')),
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
        let state_path = tool_dir().join("state.json");
        let state = match std::fs::read_to_string(&state_path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| format!("{}: {error}", state_path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(format!("{}: {error}", state_path.display())),
        };
        let rpc = Rpc::new(TESTNET_RPC);
        let chain = rpc.chain()?;
        if chain != "ckb_testnet" {
            return Err(format!("{TESTNET_RPC} serves {chain}, not the testnet"));
        }
        Ok(Self { rpc, key: Key::load(&tool_dir().join("dev.key"))?, state, state_path })
    }

    /// The file holds note secrets: private, and replaced whole or not at all.
    fn save(&self) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let text = serde_json::to_string_pretty(&self.state).map_err(|error| error.to_string())?;
        let partial = self.state_path.with_extension("json.partial");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)
            .and_then(|mut file| writeln!(file, "{text}"))
            .and_then(|_| std::fs::rename(&partial, &self.state_path))
            .map_err(|error| format!("{}: {error}", self.state_path.display()))
    }

    fn deployment(&self) -> Result<Deployment, String> {
        let record = self.state.deployment.as_ref().ok_or("nothing is deployed yet: run `mixer deploy`")?;
        Ok(Deployment::new(
            &byte32(&record.tx_hash)?,
            byte32(&record.vk_hash)?,
            byte32(&record.pool_lock_hash)?,
            byte32(&record.pool_type_hash)?,
        ))
    }

    /// The node's copy of each deployed cell has to hash to what the pool
    /// scripts name.
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

    fn pool(&self) -> Result<(Deployment, Pool), String> {
        let deployment = self.deployment()?;
        let record = self.state.pool.as_ref().ok_or("there is no pool yet: run `mixer create`")?;
        let scripts = Scripts::new(&deployment, record.denomination);
        let pool = Pool::load(&self.rpc, scripts, &byte32(&record.creation_tx)?)?;
        Ok((deployment, pool))
    }

    /// Dry-run `tx` on the node, send it, and note it in the state file.
    fn send(&mut self, step: &str, tx: &TransactionView) -> Result<Byte32, String> {
        let bytes = tx.data().serialized_size_in_block() as u64;
        let accepted = self.rpc.dry_run(tx)?;
        println!(
            "{step}: {} bytes, {} cycles, fee {} CKB ({} shannons per KB of weight)",
            grouped(bytes),
            grouped(accepted.cycles),
            ckb(accepted.fee),
            grouped(accepted.fee * 1000 / wallet::weight(bytes, accepted.cycles)),
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

    fn address(&self) -> Result<(), String> {
        let lock = self.key.lock();
        let plain = self.rpc.plain_cells(&lock)?;
        let spendable: u64 = plain.iter().map(|cell| cell.capacity()).sum();
        let total = self.rpc.total_capacity(&lock)?;
        println!("address    {}", self.key.address());
        println!("lock args  {}", hex(&lock.args().raw_data()));
        println!("spendable  {} CKB in {} plain cells", ckb(spendable), plain.len());
        println!("other      {} CKB in cells that hold data", ckb(total.saturating_sub(spendable)));
        Ok(())
    }

    fn deploy(&mut self, variant: &str) -> Result<(), String> {
        if self.state.deployment.is_some() {
            return Err("already deployed; the record is in state.json".into());
        }
        let dir = contracts_dir(variant);
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
        };
        let (pool_type, pool_lock, vk) = (read("pool-type")?, read("pool-lock")?, proof::verifying_key());

        let draft = Deployment::draft(&self.key.lock(), &vk, &pool_lock, &pool_type);
        let tx = wallet::fund(&self.rpc, &self.key, draft, 0)?;
        let hash = self.send(&format!("deploy ({variant} build)"), &tx)?;

        let data_hash = |data: &[u8]| hash_hex(&CellOutput::calc_data_hash(data));
        self.state.deployment = Some(DeploymentRecord {
            variant: variant.to_string(),
            tx_hash: hash_hex(&hash),
            vk_hash: data_hash(&vk),
            pool_lock_hash: data_hash(&pool_lock),
            pool_type_hash: data_hash(&pool_type),
        });
        self.save()?;
        self.settle(&hash)?;
        self.check_deployment(&self.deployment()?)
    }

    fn create(&mut self, denomination: u64) -> Result<(), String> {
        if self.state.pool.is_some() {
            return Err("there is a pool already; the record is in state.json".into());
        }
        let deployment = self.deployment()?;
        self.check_deployment(&deployment)?;
        let scripts = Scripts::new(&deployment, denomination);
        println!("  pool type script hash {}", hash_hex(&scripts.type_script.calc_script_hash()));
        println!("  pool lock script hash {}", hash_hex(&scripts.lock_script.calc_script_hash()));

        let tx = wallet::fund(&self.rpc, &self.key, scripts.create_draft(&deployment), 0)?;
        let hash = self.send(&format!("create pool ({} CKB)", ckb(denomination)), &tx)?;
        self.state.pool = Some(PoolRecord { denomination, creation_tx: hash_hex(&hash) });
        self.save()?;
        self.settle(&hash)
    }

    fn deposit(&mut self, count: usize) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let first_note = self.state.notes.len();
        let mut commitments = Vec::with_capacity(count);
        for _ in 0..count {
            let note = Note::random()?;
            commitments.push(fr_to_le_bytes(&note.commitment()));
            self.state.notes.push(note.record());
        }
        // The secrets first: a deposit without them cannot be withdrawn.
        self.save()?;

        let draft = pool.deposit_draft(&deployment, &commitments)?;
        let tx = wallet::fund(&self.rpc, &self.key, draft, pool.cell.capacity())?;
        let step = match count {
            1 => format!("deposit note {first_note}"),
            _ => format!("deposit notes {first_note}–{}", first_note + count - 1),
        };
        let hash = self.send(&step, &tx)?;
        self.settle(&hash)?;

        let (_, pool) = self.pool()?;
        println!("  the pool holds {} CKB and {} notes", ckb(pool.cell.capacity()), pool.leaves.len());
        Ok(())
    }

    /// Prove that note `index` of the state file is in the pool, for a payout
    /// to `recipient`.
    fn prove(&self, deployment: &Deployment, pool: &Pool, index: usize, recipient: &Script, fee: u64) -> Result<Withdrawal, String> {
        let record = self.state.notes.get(index).ok_or_else(|| format!("there is no note {index}"))?;
        let note = Note::from_record(record)?;
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
        Ok(withdrawal)
    }

    /// The key note `index` is paid out to: one that has never been on chain.
    fn recipient(&self, index: usize) -> Result<Key, String> {
        let path = tool_dir().join(format!("recipient-{index}.key"));
        let key = if path.exists() { Key::load(&path)? } else { Key::generate(&path)? };
        println!("  note {index} pays {}", key.address());
        Ok(key)
    }

    /// Three pool transactions sent back to back, each spending the pool cell
    /// that the one before it creates, without waiting for a block in between:
    /// a deposit, the withdrawal of note `index`, and the withdrawal of the note
    /// just deposited, whose proof uses a root the chain has not seen yet.
    ///
    /// Also asks the node what it makes of a withdrawal built on the cell the
    /// first transaction has already spent: the transaction of a user who did
    /// not know about the others.
    fn chain(&mut self, index: usize) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;

        let note = Note::random()?;
        let fresh = self.state.notes.len();
        self.state.notes.push(note.record());
        self.save()?;
        let draft = pool.deposit_draft(&deployment, &[fr_to_le_bytes(&note.commitment())])?;
        let deposit = wallet::fund(&self.rpc, &self.key, draft, pool.cell.capacity())?;
        let after_deposit = pool.after(&deposit)?;

        let first_recipient = self.recipient(index)?.lock();
        let first = self.prove(&deployment, &pool, index, &first_recipient, WITHDRAW_FEE)?;
        let unaware = pool.withdraw_tx(&deployment, &first, &first_recipient);
        let first_tx = after_deposit.withdraw_tx(&deployment, &first, &first_recipient);
        let after_first = after_deposit.after(&first_tx)?;

        let second_recipient = self.recipient(fresh)?.lock();
        let second = self.prove(&deployment, &after_deposit, fresh, &second_recipient, WITHDRAW_FEE)?;
        let second_tx = after_first.withdraw_tx(&deployment, &second, &second_recipient);

        let mut hashes = vec![self.send(&format!("chain 1/3: deposit note {fresh}"), &deposit)?];
        match self.rpc.dry_run(&unaware) {
            Ok(accepted) => println!(
                "  a withdrawal built on the cell that deposit spends: the node would take it ({} cycles, fee {} CKB)",
                grouped(accepted.cycles),
                ckb(accepted.fee)
            ),
            Err(message) => println!("  a withdrawal built on the cell that deposit spends: refused, {message}"),
        }
        hashes.push(self.send(&format!("chain 2/3: withdraw note {index}"), &first_tx)?);
        hashes.push(self.send(&format!("chain 3/3: withdraw note {fresh}"), &second_tx)?);

        let mut blocks = Vec::new();
        for hash in &hashes {
            let committed = self.rpc.wait_committed(hash, WAIT)?;
            self.rpc.wait_indexed(committed.block_number)?;
            blocks.push(committed.block_number);
        }
        println!("  committed in blocks {blocks:?}");

        let (_, pool) = self.pool()?;
        println!(
            "  the pool holds {} CKB: {} deposits, {} withdrawals",
            ckb(pool.cell.capacity()),
            pool.leaves.len(),
            pool.nullifiers.len()
        );
        Ok(())
    }

    fn withdraw(&mut self, index: usize, fee: u64, after_deposits: usize) -> Result<(), String> {
        let (deployment, mut pool) = self.pool()?;
        let recipient = self.recipient(index)?;

        let withdrawal = self.prove(&deployment, &pool, index, &recipient.lock(), fee)?;
        let mut step = format!("withdraw note {index}");
        if after_deposits > 0 {
            // Other people's deposits land between proving and sending. The
            // proof's root is then no longer the newest one, and the pool has
            // to find it in its root history.
            for _ in 0..after_deposits {
                self.deposit(1)?;
            }
            pool = self.pool()?.1;
            if withdrawal.root == *pool.state.current_root() || !pool.state.is_known_root(&withdrawal.root) {
                return Err("the proof's root is not an older root of the pool's history".into());
            }
            let age = match after_deposits {
                1 => "1 deposit".to_string(),
                count => format!("{count} deposits"),
            };
            println!("  the proof's root is {age} old now");
            step += &format!(", root {age} old");
        }
        let tx = pool.withdraw_tx(&deployment, &withdrawal, &recipient.lock());
        let hash = self.send(&step, &tx)?;
        self.settle(&hash)?;

        let received: u64 = self.rpc.plain_cells(&recipient.lock())?.iter().map(|cell| cell.capacity()).sum();
        println!("  the recipient now holds {} CKB", ckb(received));
        Ok(())
    }

    /// Transactions an attacker would try, dry-run on the node: each has to be
    /// refused by the pool type script, for the expected reason. Nothing is sent.
    fn attack(&mut self, index: usize) -> Result<(), String> {
        let (deployment, pool) = self.pool()?;
        let thief = self.key.lock();
        let denomination = pool.scripts.denomination;
        let mut cases: Vec<(&str, TransactionView, Error)> = Vec::new();

        // A withdrawal that is on chain already, sent once more. Everything in
        // it is public, so this needs no secret at all.
        let committed = pool.steps.iter().rev().find_map(|step| match &step.event {
            Event::Withdrew { withdrawal, recipient } => Some((withdrawal, recipient)),
            _ => None,
        });
        match committed {
            Some((withdrawal, recipient)) => cases.push((
                "a committed withdrawal, replayed",
                pool.withdraw_tx(&deployment, withdrawal, recipient),
                Error::NullifierSpent,
            )),
            None => println!("  (no withdrawal on chain yet, so nothing to replay)"),
        }

        // A fresh proof that somebody else got hold of before it was mined.
        let victim = wallet::secp_lock(&wallet::random_bytes::<20>()?);
        let withdrawal = self.prove(&deployment, &pool, index, &victim, WITHDRAW_FEE)?;
        let honest = pool.withdraw_tx(&deployment, &withdrawal, &victim);
        let accepted = self.rpc.dry_run(&honest)?;
        println!("  ok    the withdrawal as proved: accepted, {} cycles", grouped(accepted.cycles));

        cases.push(("the same proof, paid to another lock", pool.withdraw_tx(&deployment, &withdrawal, &thief), Error::ProofRejected));
        let greedy = Withdrawal { fee: withdrawal.fee + SHANNONS, ..withdrawal.clone() };
        cases.push(("the same proof, with a higher fee", pool.withdraw_tx(&deployment, &greedy, &victim), Error::ProofRejected));
        let mut outputs: Vec<CellOutput> = honest.outputs().into_iter().collect();
        outputs[1] = outputs[1].clone().as_builder().capacity(denomination - withdrawal.fee - SHANNONS).build();
        cases.push((
            "the same proof, the recipient short-changed",
            honest.as_advanced_builder().set_outputs(outputs).build(),
            Error::RecipientCapacity,
        ));

        cases.push(("the pool cell spent into a plain cell", pool.theft_tx(&deployment, &thief), Error::GroupShape));

        let commitments = [fr_to_le_bytes(&Note::random()?.commitment()), fr_to_le_bytes(&Note::random()?.commitment())];
        let draft = pool.deposit_draft(&deployment, &commitments)?;
        let mut outputs: Vec<CellOutput> = draft.outputs().into_iter().collect();
        outputs[0] = outputs[0].clone().as_builder().capacity(pool.cell.capacity() + denomination).build();
        let draft = draft.as_advanced_builder().set_outputs(outputs).build();
        let short = wallet::fund(&self.rpc, &self.key, draft, pool.cell.capacity())?;
        cases.push(("two notes deposited for the price of one", short, Error::OutputCapacity));

        let mut failed = 0;
        for (name, tx, error) in &cases {
            match self.rpc.dry_run(tx) {
                Err(message) if message.contains(&format!("error code {} ", error.code())) => {
                    println!("  ok    {name}: refused, {error:?} (exit code {})", error.code());
                }
                Err(message) => {
                    failed += 1;
                    println!("  FAIL  {name}: expected {error:?} (exit code {}), got: {message}", error.code());
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

    fn info(&self) -> Result<(), String> {
        if let Some(record) = &self.state.deployment {
            println!("deployment ({} build): {}", record.variant, record.tx_hash);
            println!("  verifying key  {}", record.vk_hash);
            println!("  pool-lock      {}", record.pool_lock_hash);
            println!("  pool-type      {}", record.pool_type_hash);
        }
        if self.state.pool.is_some() {
            let (_, pool) = self.pool()?;
            let out_point = &pool.cell.out_point;
            let index: u32 = out_point.index().unpack();
            println!("pool cell {}:{index}", hash_hex(&out_point.tx_hash()));
            println!("  type script hash {}", hash_hex(&pool.scripts.type_script.calc_script_hash()));
            println!("  capacity {} CKB, denomination {} CKB", ckb(pool.cell.capacity()), ckb(pool.scripts.denomination));
            println!("  {} deposits, {} withdrawals", pool.leaves.len(), pool.nullifiers.len());
            println!("  commitment root {}", hex(pool.state.current_root()));
            println!("  nullifier root  {}", hex(&pool.state.nullifier_root));
            println!("history, rebuilt from the chain:");
            for step in &pool.steps {
                let what = match &step.event {
                    Event::Created => "created".to_string(),
                    Event::Deposited { first_leaf, count: 1 } => format!("deposit, leaf {first_leaf}"),
                    Event::Deposited { first_leaf, count } => format!("deposit, leaves {first_leaf}–{}", first_leaf + count - 1),
                    Event::Withdrew { withdrawal, recipient } => {
                        format!("withdrawal to {}, fee {} CKB", wallet::address(recipient), ckb(withdrawal.fee))
                    }
                };
                println!("  block {:>8}  {}  {what}", step.block_number.unwrap_or(0), hash_hex(&step.tx_hash));
            }
            for (index, record) in self.state.notes.iter().enumerate() {
                let note = Note::from_record(record)?;
                let status = match pool.leaf_index(&note.commitment()) {
                    None => "not in the pool".to_string(),
                    Some(leaf) => {
                        let nullifier_hash = fr_to_le_bytes(&hash2(&note.nullifier, &Fr::from(leaf)));
                        let spent = if pool.nullifiers.contains(&nullifier_hash) { "withdrawn" } else { "unspent" };
                        format!("leaf {leaf}, {spent}")
                    }
                };
                println!("  note {index}: {status}");
            }
        }

        println!("\n| step | transaction | block | bytes | cycles | fee (CKB) | block time − sent |");
        println!("|---|---|---:|---:|---:|---:|---:|");
        for record in &self.state.sent {
            let link = format!("[`{}…`]({EXPLORER_TX}{})", &record.tx_hash[..10], record.tx_hash);
            match self.rpc.transaction(&byte32(&record.tx_hash)?)? {
                Some(found) if found.status == "committed" => {
                    let block = found.block_number.unwrap_or(0);
                    let waited = (self.rpc.block_timestamp(block)? / 1000).saturating_sub(record.sent_at);
                    // The chain's own numbers, where the node still has them.
                    let cycles = found.cycles.unwrap_or(record.cycles);
                    let fee = found.fee.unwrap_or(record.fee);
                    if cycles != record.cycles || fee != record.fee {
                        println!("(the dry run of the next row said {} cycles, fee {})", record.cycles, record.fee);
                    }
                    println!(
                        "| {} | {link} | {} | {} | {} | {} | {waited} s |",
                        record.step,
                        grouped(block),
                        grouped(record.bytes),
                        grouped(cycles),
                        ckb(fee),
                    );
                }
                found => {
                    let status = found.map(|found| found.status).unwrap_or_else(|| "unknown".to_string());
                    println!("| {} | {link} | {status} | {} | {} | {} | |", record.step, grouped(record.bytes), grouped(record.cycles), ckb(record.fee));
                }
            }
        }
        Ok(())
    }
}

/// `--name value` pairs.
struct Options(HashMap<String, String>);

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = HashMap::new();
        let mut args = args;
        while let Some(arg) = args.next() {
            let name = arg.strip_prefix("--").ok_or_else(|| format!("unexpected argument: {arg}\n\n{USAGE}"))?;
            let value = args.next().ok_or_else(|| format!("--{name} needs a value"))?;
            options.insert(name.to_string(), value);
        }
        Ok(Self(options))
    }

    fn take<T: FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        match self.0.remove(name) {
            Some(value) => value.parse().map(Some).map_err(|_| format!("--{name}: cannot read {value}")),
            None => Ok(None),
        }
    }

    fn done(self) -> Result<(), String> {
        match self.0.keys().next() {
            Some(name) => Err(format!("unknown option --{name}\n\n{USAGE}")),
            None => Ok(()),
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or(USAGE)?;
    let mut options = Options::parse(args)?;
    let note = |options: &mut Options| options.take::<usize>("note")?.ok_or("--note <n> is required".to_string());

    match command.as_str() {
        "address" => {
            options.done()?;
            Mixer::open()?.address()
        }
        "deploy" => {
            let variant = options.take::<String>("variant")?.unwrap_or_else(|| "size".to_string());
            options.done()?;
            Mixer::open()?.deploy(&variant)
        }
        "create" => {
            let denomination = options.take::<u64>("denomination")?.unwrap_or(100);
            options.done()?;
            Mixer::open()?.create(denomination * SHANNONS)
        }
        "deposit" => {
            let count = options.take::<usize>("count")?.unwrap_or(1);
            options.done()?;
            if count == 0 {
                return Err("--count has to be at least 1".into());
            }
            Mixer::open()?.deposit(count)
        }
        "withdraw" => {
            let index = note(&mut options)?;
            let fee = options.take::<u64>("fee")?.unwrap_or(WITHDRAW_FEE);
            let after_deposits = options.take::<usize>("after-deposits")?.unwrap_or(0);
            options.done()?;
            Mixer::open()?.withdraw(index, fee, after_deposits)
        }
        "attack" => {
            let index = note(&mut options)?;
            options.done()?;
            Mixer::open()?.attack(index)
        }
        "chain" => {
            let index = note(&mut options)?;
            options.done()?;
            Mixer::open()?.chain(index)
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
