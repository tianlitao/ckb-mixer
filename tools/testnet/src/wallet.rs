//! The funding account: a secp256k1-blake160 key on testnet, with coin
//! selection and the sighash-all signature of ckb-system-scripts.

use std::io::Read;
use std::path::Path;

use ckb_hash::{blake2b_256, new_blake2b};
use ckb_types::{
    bytes::Bytes,
    core::{tx_pool::get_transaction_weight, Capacity, DepType, ScriptHashType, TransactionView},
    h256,
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
    H256,
};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

use crate::rpc::{LiveCell, Rpc};

pub const SHANNONS: u64 = 100_000_000;
/// Fee rate of the transactions this account pays for, in shannons per 1,000
/// bytes: five times the nodes' default `min_fee_rate`.
pub const FEE_RATE: u64 = 5_000;

/// secp256k1-blake160-sighash-all on testnet: its type hash, and the
/// transaction whose output 0 is the dep group with the code and its data.
const SECP_CODE_HASH: H256 = h256!("0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8");
const SECP_DEP_GROUP_TX: H256 = h256!("0xf8de3bb47d055cdf460d93a2a6e1b05f7432f9777c8c474abf4eec1d4aee5d37");
const SIGNATURE_LEN: usize = 65;

pub fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("/dev/urandom: {error}"))?;
    Ok(bytes)
}

pub struct Key {
    secret: SecretKey,
}

impl Key {
    /// Read a key file: 32 bytes, hex.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let bytes = unhex(text.trim())?;
        let secret = SecretKey::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self { secret })
    }

    /// Create a key file with a fresh key. Never overwrites one.
    pub fn generate(path: &Path) -> Result<Self, String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let secret = loop {
            if let Ok(secret) = SecretKey::from_slice(&random_bytes::<32>()?) {
                break secret;
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        writeln!(file, "{}", &hex(&secret.secret_bytes())[2..]).map_err(|error| error.to_string())?;
        Ok(Self { secret })
    }

    pub fn lock(&self) -> Script {
        let public_key = PublicKey::from_secret_key(SECP256K1, &self.secret).serialize();
        secp_lock(&blake2b_256(public_key)[..20])
    }

    pub fn address(&self) -> String {
        address(&self.lock())
    }

    /// Recoverable signature in the layout the lock expects: r, s, recovery id.
    fn sign(&self, digest: &[u8; 32]) -> [u8; SIGNATURE_LEN] {
        let signature = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(*digest), &self.secret);
        let (recovery, compact) = signature.serialize_compact();
        let mut out = [0u8; SIGNATURE_LEN];
        out[..64].copy_from_slice(&compact);
        out[64] = i32::from(recovery) as u8;
        out
    }
}

/// The secp256k1-blake160 lock of the key whose public key hashes to `args`.
pub fn secp_lock(args: &[u8]) -> Script {
    Script::new_builder()
        .code_hash(SECP_CODE_HASH.pack())
        .hash_type(ScriptHashType::Type)
        .args(Bytes::copy_from_slice(args).pack())
        .build()
}

/// Testnet address of a lock script (full format, bech32m).
pub fn address(lock: &Script) -> String {
    let mut payload = vec![0x00];
    payload.extend_from_slice(lock.code_hash().as_slice());
    payload.push(lock.hash_type().into());
    payload.extend_from_slice(&lock.args().raw_data());
    bech32m("ckt", &payload)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let digits = text.trim().trim_start_matches("0x");
    if digits.len() % 2 != 0 || !digits.is_ascii() {
        return Err(format!("not hex: {text}"));
    }
    (0..digits.len() / 2)
        .map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).map_err(|_| format!("not hex: {text}")))
        .collect()
}

/// A cell with nothing but capacity.
pub fn plain_cell(lock: &Script, capacity: u64) -> CellOutput {
    CellOutput::new_builder().capacity(capacity).lock(lock.clone()).build()
}

/// The least capacity `cell` can have with `data_len` bytes of data.
pub fn occupied(cell: &CellOutput, data_len: usize) -> u64 {
    let data = Capacity::bytes(data_len).expect("data length fits");
    cell.occupied_capacity(data).expect("capacity fits").as_u64()
}

fn secp_dep() -> CellDep {
    CellDep::new_builder()
        .out_point(OutPoint::new(SECP_DEP_GROUP_TX.pack(), 0))
        .dep_type(DepType::DepGroup)
        .build()
}

/// The weight the transaction pool charges fees on.
pub fn weight(bytes: u64, cycles: u64) -> u64 {
    get_transaction_weight(bytes as usize, cycles)
}

/// Complete `draft` with cells of `key`: inputs that cover its outputs and the
/// fee, a change output, the lock's cell dep and the signature.
///
/// `draft` may already spend cells of other locks, worth `draft_input_capacity`
/// in total, with one witness each; the pool cell of a deposit is one.
pub fn fund(rpc: &Rpc, key: &Key, draft: TransactionView, draft_input_capacity: u64) -> Result<TransactionView, String> {
    fund_with(rpc, key, draft, draft_input_capacity, 0, &|tx| tx)
}

/// [`fund`], with two things a caller may need to decide.
///
/// `least_fee`: the transaction pays this much even where its size asks for
/// less. Miners rank a transaction by its weight, which for one that verifies
/// many proofs is set by its cycles and not by its bytes.
///
/// `finish`: applied to the funded transaction before it is signed, for
/// outputs that depend on which cells fund it. It must not change the
/// transaction's size, inputs or witnesses.
pub fn fund_with(
    rpc: &Rpc,
    key: &Key,
    draft: TransactionView,
    draft_input_capacity: u64,
    least_fee: u64,
    finish: &dyn Fn(TransactionView) -> TransactionView,
) -> Result<TransactionView, String> {
    assert_eq!(draft.witnesses().len(), draft.inputs().len(), "one witness per input");
    let lock = key.lock();

    // Plain cells only: the code cells this key deployed sit under the same
    // lock, and spending one would take the code off the chain.
    let mut cells: Vec<LiveCell> = rpc.plain_cells(&lock)?;
    cells.sort_by_key(|cell| std::cmp::Reverse(cell.capacity()));

    let outputs: u64 = draft.outputs().into_iter().map(|output| Unpack::<u64>::unpack(&output.capacity())).sum();
    let least_change = occupied(&plain_cell(&lock, 0), 0);

    let mut used = 0;
    let mut total = draft_input_capacity;
    let mut fee = least_fee;
    loop {
        while used == 0 || total < outputs + fee + least_change {
            let cell = cells.get(used).ok_or_else(|| {
                format!(
                    "{} holds {} CKB in plain cells; the transaction needs {} CKB",
                    key.address(),
                    (total - draft_input_capacity) / SHANNONS,
                    (outputs + fee + least_change - draft_input_capacity).div_ceil(SHANNONS),
                )
            })?;
            total += cell.capacity();
            used += 1;
        }
        let change = plain_cell(&lock, total - outputs - fee);
        let tx = assemble(&draft, &cells[..used], change);
        // The size does not depend on the amounts, so this settles as soon as
        // the set of inputs does.
        let needed = (tx.data().serialized_size_in_block() as u64 * FEE_RATE).div_ceil(1000);
        if needed <= fee {
            return Ok(sign(finish(tx), key, draft.inputs().len(), used));
        }
        fee = needed;
    }
}

/// `draft` plus the funding inputs and the change output, not yet signed: the
/// first funding input's witness reserves room for the signature.
fn assemble(draft: &TransactionView, cells: &[LiveCell], change: CellOutput) -> TransactionView {
    let placeholder = WitnessArgs::new_builder()
        .lock(Some(Bytes::from(vec![0u8; SIGNATURE_LEN])).pack())
        .build();
    let mut builder = draft.as_advanced_builder().cell_dep(secp_dep());
    for (index, cell) in cells.iter().enumerate() {
        let witness = if index == 0 { placeholder.as_bytes() } else { Bytes::new() };
        builder = builder.input(CellInput::new(cell.out_point.clone(), 0)).witness(witness.pack());
    }
    builder.output(change).output_data(Bytes::new().pack()).build()
}

/// Sign the inputs `first..first + count`, which all carry the key's lock.
///
/// The lock hashes the transaction hash, then the witnesses of its own inputs
/// — the first of them with 65 zero bytes where the signature goes, which is
/// exactly the placeholder — and then any witness beyond the inputs, each
/// prefixed with its length.
fn sign(tx: TransactionView, key: &Key, first: usize, count: usize) -> TransactionView {
    let mut witnesses: Vec<Bytes> = tx.witnesses().into_iter().map(|witness| witness.raw_data()).collect();
    let inputs = tx.inputs().len();

    let mut hasher = new_blake2b();
    hasher.update(tx.hash().as_slice());
    for index in (first..first + count).chain(inputs..witnesses.len()) {
        hasher.update(&(witnesses[index].len() as u64).to_le_bytes());
        hasher.update(&witnesses[index]);
    }
    let mut digest = [0u8; 32];
    hasher.finalize(&mut digest);

    witnesses[first] = WitnessArgs::new_builder()
        .lock(Some(Bytes::copy_from_slice(&key.sign(&digest))).pack())
        .build()
        .as_bytes();
    tx.as_advanced_builder()
        .set_witnesses(witnesses.into_iter().map(|witness| witness.pack()).collect())
        .build()
}

// ---- bech32m, for addresses

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const BECH32M: u32 = 0x2bc8_30a3;

fn polymod(values: &[u8]) -> u32 {
    const GENERATOR: [u32; 5] = [0x3b6a_57b2, 0x2650_8e6d, 0x1ea1_19fa, 0x3d42_33dd, 0x2a14_62b3];
    let mut checksum: u32 = 1;
    for value in values {
        let top = checksum >> 25;
        checksum = ((checksum & 0x1ff_ffff) << 5) ^ u32::from(*value);
        for (bit, generator) in GENERATOR.iter().enumerate() {
            if (top >> bit) & 1 == 1 {
                checksum ^= generator;
            }
        }
    }
    checksum
}

fn bech32m(hrp: &str, payload: &[u8]) -> String {
    // The payload as 5-bit groups, the last one padded with zeros.
    let mut groups = Vec::with_capacity(payload.len() * 8 / 5 + 7);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in payload {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            groups.push(((buffer >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        groups.push(((buffer << (5 - bits)) & 31) as u8);
    }

    let mut checked: Vec<u8> = hrp.bytes().map(|byte| byte >> 5).collect();
    checked.push(0);
    checked.extend(hrp.bytes().map(|byte| byte & 31));
    checked.extend_from_slice(&groups);
    checked.extend_from_slice(&[0; 6]);
    let checksum = polymod(&checked) ^ BECH32M;
    groups.extend((0..6).map(|i| ((checksum >> (5 * (5 - i))) & 31) as u8));

    let mut out = format!("{hrp}1");
    out.extend(groups.into_iter().map(|group| CHARSET[usize::from(group)] as char));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The well-known test key of the CKB docs and its testnet address.
    #[test]
    fn address_of_a_known_key() {
        let secret = unhex("d00c06bfd800d27397002dca6fb0993d5ba6399b4238b2f29ee9deb97593d2bc").unwrap();
        let key = Key { secret: SecretKey::from_slice(&secret).unwrap() };
        assert_eq!(hex(&key.lock().args().raw_data()), "0xc8328aabcd9b9e8e64fbc566c4385c3bdeb219d7");
        assert_eq!(
            key.address(),
            "ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqwgx292hnvmn68xf779vmzrshpmm6epn4c0cgwga"
        );
    }
}
