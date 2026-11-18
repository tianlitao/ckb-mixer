//! Cycle and size measurements, for every build variant of the contracts.
//!
//!     scripts/build-contracts.sh all
//!     cargo test -p mixer-tests --test cycles -- --nocapture
//!
//! Variants that have not been built are skipped. These are measurements, not
//! pass/fail tests: the only assertions are that the transactions are accepted.

use mixer_core::merkle::TREE_DEPTH;
use mixer_tests::harness::{contracts_dir, tx_size, Env, Note, Pool, CKB, DENOMINATION, MAX_CYCLES};

/// Batch sizes of the measured deposits.
const BATCHES: [u32; 5] = [1, 2, 4, 8, 16];
/// Withdrawals measured per variant; the nullifier set grows by one each time.
const WITHDRAWALS: u32 = 4;

/// Poseidon hashes the pool performs to append `n` leaves to an empty tree: the
/// last leaf is hashed to the root, an earlier leaf only while its node is a
/// right child.
fn poseidon_hashes(n: u32) -> u64 {
    TREE_DEPTH as u64 + (0..n - 1).map(|index| u64::from(index.trailing_ones())).sum::<u64>()
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn size(variant: &str, name: &str) -> Option<u64> {
    std::fs::metadata(contracts_dir(variant).join(name)).ok().map(|meta| meta.len())
}

struct Measurement {
    create: u64,
    /// `(batch size, cycles, transaction bytes)`
    deposits: Vec<(u32, u64, usize)>,
    /// `(spent nullifiers before, cycles, transaction bytes)`
    withdrawals: Vec<(usize, u64, usize)>,
    deposit_stages: Vec<(String, u64)>,
    withdraw_stages: Vec<(String, u64)>,
}

fn measure(variant: &str) -> Measurement {
    let dir = contracts_dir(variant);

    let mut deposits = Vec::new();
    let mut deposit_stages = Vec::new();
    let mut create = 0;
    for batch in BATCHES {
        let mut env = Env::with_contracts(&dir);
        let (mut pool, create_cycles) = Pool::create(&mut env, DENOMINATION);
        create = create_cycles;
        let commitments: Vec<_> = (0..batch).map(|i| Note::from_seed(u64::from(i)).commitment()).collect();
        let params = pool.deposit_params(&commitments);
        let tx = pool.deposit_tx(&mut env, &params);
        let (cycles, stages) = env.verify_profiled(&tx);
        pool.apply_deposit(&mut env, &tx, &params);
        deposits.push((batch, cycles, tx_size(&tx)));
        if batch == 1 {
            deposit_stages = stages;
        }
    }

    let mut env = Env::with_contracts(&dir);
    let (mut pool, _) = Pool::create(&mut env, DENOMINATION);
    let notes: Vec<Note> = (0..WITHDRAWALS).map(|i| Note::from_seed(100 + u64::from(i))).collect();
    let commitments: Vec<_> = notes.iter().map(Note::commitment).collect();
    pool.deposit(&mut env, &commitments);

    let recipient = env.user_lock(0xaa);
    let mut withdrawals = Vec::new();
    let mut withdraw_stages = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        let proof = pool.prove_withdraw(&env, note, index as u32, &recipient, CKB / 100);
        let params = pool.withdraw_params(&env, &proof);
        let tx = pool.withdraw_tx(&mut env, &params);
        let (cycles, stages) = env.verify_profiled(&tx);
        pool.apply_withdraw(&mut env, &tx, &params);
        withdrawals.push((index, cycles, tx_size(&tx)));
        if index == 0 {
            withdraw_stages = stages;
        }
    }

    Measurement { create, deposits, withdrawals, deposit_stages, withdraw_stages }
}

fn print_stages(title: &str, total: u64, stages: &[(String, u64)], labels: &[(&str, &str)]) {
    println!("\n{title}: {} cycles in total", thousands(total));
    let mut previous = 0;
    for (stage, cycles) in stages {
        let label = labels
            .iter()
            .find(|(name, _)| name == stage)
            .map_or(stage.as_str(), |(_, label)| label);
        println!("  {:>12}  {label}", thousands(cycles - previous));
        previous = *cycles;
    }
    println!("  {:>12}  pool lock, other locks", thousands(total - previous));
}

#[test]
fn report() {
    println!("\nblock limit (mainnet max_block_cycles): {} cycles", thousands(MAX_CYCLES));

    let mut size_default = None;
    for variant in ["default", "size", "bext"] {
        let (Some(type_size), Some(lock_size)) = (size(variant, "pool-type"), size(variant, "pool-lock")) else {
            println!("\n== {variant}: not built, skipped (scripts/build-contracts.sh {variant})");
            continue;
        };
        let m = measure(variant);
        println!("\n== {variant}: pool-type {} bytes, pool-lock {} bytes", thousands(type_size), thousands(lock_size));
        println!("  create pool          {:>12} cycles", thousands(m.create));
        for (batch, cycles, bytes) in &m.deposits {
            println!(
                "  deposit x{batch:<2} ({:>2} Poseidon) {:>12} cycles   {:>6} tx bytes",
                poseidon_hashes(*batch),
                thousands(*cycles),
                thousands(*bytes as u64),
            );
        }
        let (first, last) = (m.deposits[0], m.deposits[m.deposits.len() - 1]);
        let per_hash = (last.1 - first.1) / (poseidon_hashes(last.0) - poseidon_hashes(first.0));
        println!("  per extra Poseidon hash   ~{} cycles", thousands(per_hash));
        for (spent, cycles, bytes) in &m.withdrawals {
            println!(
                "  withdraw ({spent} spent before) {:>12} cycles   {:>6} tx bytes   {:.2}% of a block",
                thousands(*cycles),
                thousands(*bytes as u64),
                *cycles as f64 * 100.0 / MAX_CYCLES as f64,
            );
        }
        if variant == "default" {
            size_default = Some(type_size);
        }
    }

    if size("profile", "pool-type").is_none() {
        println!("\n== profile: not built, no stage breakdown (scripts/build-contracts.sh profile)");
        return;
    }
    let m = measure("profile");
    println!("\n== stage breakdown (profile build: the default build plus cycle checkpoints)");
    if let Some(bytes) = size_default {
        println!("  loading the pool-type binary costs 1 cycle per 4 bytes: ~{} cycles", thousands(bytes / 4));
    }
    print_stages(
        "deposit x1",
        m.deposits[0].1,
        &m.deposit_stages,
        &[
            ("start", "VM start, program load"),
            ("loaded", "load script, pool state and witness"),
            ("tree", "append to the commitment tree (20 Poseidon hashes)"),
            ("end", "compare output state and capacity"),
        ],
    );
    print_stages(
        "withdraw",
        m.withdrawals[0].1,
        &m.withdraw_stages,
        &[
            ("start", "VM start, program load"),
            ("loaded", "load script, pool state and witness"),
            ("nullifier", "root lookup, nullifier set update (256 levels of blake2b)"),
            ("recipient", "recipient checks, load verifying key"),
            ("groth16", "Groth16 verification"),
            ("end", "compare output state and capacity"),
        ],
    );
}
