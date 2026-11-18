//! What the two command-line tools, `mixer` (pool v1) and `mixer2` (pool v2),
//! have in common: notes, the private state file, option parsing, and the
//! table of transactions sent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ckb_types::{packed::Byte32, prelude::*};
use mixer_core::field::{fr_from_le_bytes, fr_to_le_bytes};
use mixer_core::poseidon::hash2;
use mixer_core::{Bytes32, Fr};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::rpc::Rpc;
use crate::wallet::{self, hex, unhex, SHANNONS};
use crate::EXPLORER_TX;

/// How long to wait for a transaction to be committed.
pub const WAIT: Duration = Duration::from_secs(540);

/// A note's secrets, as 32-byte little-endian field elements.
#[derive(Serialize, Deserialize)]
pub struct NoteRecord {
    pub nullifier: String,
    pub secret: String,
    pub commitment: String,
}

/// A transaction a tool sent, with what the node's dry run said about it.
#[derive(Serialize, Deserialize)]
pub struct SentRecord {
    pub step: String,
    pub tx_hash: String,
    /// Seconds since the epoch.
    pub sent_at: u64,
    pub bytes: u64,
    pub cycles: u64,
    pub fee: u64,
}

pub struct Note {
    pub nullifier: Fr,
    pub secret: Fr,
}

impl Note {
    pub fn random() -> Result<Self, String> {
        // 31 random bytes are always below the field's modulus.
        let element = || -> Result<Fr, String> {
            let mut bytes = [0u8; 32];
            bytes[..31].copy_from_slice(&wallet::random_bytes::<31>()?);
            Ok(fr_from_le_bytes(&bytes).expect("below the modulus"))
        };
        Ok(Self { nullifier: element()?, secret: element()? })
    }

    pub fn commitment(&self) -> Fr {
        hash2(&self.nullifier, &self.secret)
    }

    pub fn record(&self) -> NoteRecord {
        NoteRecord {
            nullifier: hex(&fr_to_le_bytes(&self.nullifier)),
            secret: hex(&fr_to_le_bytes(&self.secret)),
            commitment: hex(&fr_to_le_bytes(&self.commitment())),
        }
    }

    pub fn from_record(record: &NoteRecord) -> Result<Self, String> {
        let element = |text: &str| fr_from_le_bytes(&bytes32(text)?).ok_or_else(|| format!("not a field element: {text}"));
        let note = Self { nullifier: element(&record.nullifier)?, secret: element(&record.secret)? };
        if hex(&fr_to_le_bytes(&note.commitment())) != record.commitment {
            return Err(format!("the secrets of note {} do not give its commitment", record.commitment));
        }
        Ok(note)
    }
}

pub fn tool_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where `scripts/build-contracts.sh` leaves the binaries of a build variant.
pub fn contracts_dir(variant: &str) -> PathBuf {
    let target = match variant {
        "default" => "target".to_string(),
        other => format!("target-{other}"),
    };
    mixer_prover::repo_root().join("contracts").join(target).join("riscv64imac-unknown-none-elf/release")
}

pub fn bytes32(text: &str) -> Result<Bytes32, String> {
    unhex(text)?.try_into().map_err(|_| format!("not 32 bytes: {text}"))
}

pub fn byte32(text: &str) -> Result<Byte32, String> {
    Ok(bytes32(text)?.pack())
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// `1234567` as `1,234,567`.
pub fn grouped(number: u64) -> String {
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
pub fn ckb(shannons: u64) -> String {
    let whole = grouped(shannons / SHANNONS);
    match shannons % SHANNONS {
        0 => whole,
        fraction => format!("{whole}.{}", format!("{fraction:08}").trim_end_matches('0')),
    }
}

/// Read a state file; one that does not exist yet is the default state.
pub fn load_state<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Write a state file. It holds note secrets: private, and replaced whole or
/// not at all.
pub fn save_state<T: Serialize>(path: &Path, state: &T) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    let partial = path.with_extension("json.partial");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)
        .and_then(|mut file| writeln!(file, "{text}"))
        .and_then(|_| std::fs::rename(&partial, path))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The transactions a tool sent, as a Markdown table, with what the chain says
/// about each of them now.
pub fn print_sent(rpc: &Rpc, sent: &[SentRecord]) -> Result<(), String> {
    println!("\n| step | transaction | block | bytes | cycles | fee (CKB) | block time − sent |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for record in sent {
        let link = format!("[`{}…`]({EXPLORER_TX}{})", &record.tx_hash[..10], record.tx_hash);
        match rpc.transaction(&byte32(&record.tx_hash)?)? {
            Some(found) if found.status == "committed" => {
                let block = found.block_number.unwrap_or(0);
                let waited = (rpc.block_timestamp(block)? / 1000).saturating_sub(record.sent_at);
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

/// `--name value` pairs.
pub struct Options {
    values: HashMap<String, String>,
    usage: &'static str,
}

impl Options {
    pub fn parse(args: impl Iterator<Item = String>, usage: &'static str) -> Result<Self, String> {
        let mut values = HashMap::new();
        let mut args = args;
        while let Some(arg) = args.next() {
            let name = arg.strip_prefix("--").ok_or_else(|| format!("unexpected argument: {arg}\n\n{usage}"))?;
            let value = args.next().ok_or_else(|| format!("--{name} needs a value"))?;
            values.insert(name.to_string(), value);
        }
        Ok(Self { values, usage })
    }

    pub fn take<T: FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        match self.values.remove(name) {
            Some(value) => value.parse().map(Some).map_err(|_| format!("--{name}: cannot read {value}")),
            None => Ok(None),
        }
    }

    pub fn done(self) -> Result<(), String> {
        match self.values.keys().next() {
            Some(name) => Err(format!("unknown option --{name}\n\n{}", self.usage)),
            None => Ok(()),
        }
    }
}
