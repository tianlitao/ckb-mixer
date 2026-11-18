//! Cycle and size measurements of pool v2, for every build variant.
//!
//!     scripts/build-contracts.sh all
//!     cargo test -p mixer-tests --test cycles_v2 -- --nocapture --ignored
//!
//! Variants that have not been built are skipped. These are measurements, not
//! pass/fail tests: the assertions are that the batches are accepted, and that
//! the largest batch that should fit a block does while one more does not.

use ckb_testtool::ckb_types::{core::Capacity, packed::OutPoint};
use mixer_tests::harness::{
    contracts_dir, plain_cell, tx_size, tx_weight, Env, Note, CKB, DENOMINATION, MAX_CYCLES,
};
use mixer_tests::harness_v2::{PoolSetup, PoolV2};

/// The root history the design recommends, see docs/intent-aggregation.md.
const ROOT_HISTORY: u16 = 128;
const DEPOSIT_BATCHES: [u32; 6] = [1, 2, 4, 8, 16, 64];
const WITHDRAW_BATCHES: [u32; 5] = [1, 2, 4, 8, 16];
const FEE: u64 = CKB / 100;

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

fn setup() -> PoolSetup {
    let mut setup = PoolSetup::new(DENOMINATION);
    setup.root_history = ROOT_HISTORY;
    setup
}

struct Batch {
    deposits: u32,
    withdrawals: u32,
    cycles: u64,
    bytes: usize,
    weight: u64,
    stages: Vec<(String, u64)>,
}

/// Settle one batch of `deposits` intents and `withdrawals` payouts on a fresh
/// pool. `None` if it does not fit a block.
fn batch(variant: &str, deposits: u32, withdrawals: u32) -> Option<Batch> {
    let mut env = Env::with_contracts(&contracts_dir(variant));
    let (mut pool, _) = PoolV2::create(&mut env, &setup());

    let notes: Vec<Note> = (0..withdrawals).map(|i| Note::from_seed(1_000 + u64::from(i))).collect();
    if !notes.is_empty() {
        pool.deposit_notes(&mut env, &notes);
    }
    let recipient = env.user_lock(0xaa);
    let proofs: Vec<_> = notes
        .iter()
        .enumerate()
        .map(|(index, note)| pool.prove_withdraw(&env, note, index as u32, &recipient, FEE))
        .collect();
    let owner = env.user_lock(0xd0);
    let intents: Vec<OutPoint> = (0..deposits)
        .map(|i| pool.place_intent(&mut env, &Note::from_seed(u64::from(i)).commitment(), &owner))
        .collect();

    let params = pool.batch_params(&mut env, &intents, &proofs);
    let tx = pool.batch_tx(&mut env, &params);
    let (cycles, stages) = env.try_verify_profiled(&tx).ok()?;
    Some(Batch { deposits, withdrawals, cycles, bytes: tx_size(&tx), weight: tx_weight(&tx, cycles), stages })
}

fn print_batch(batch: &Batch) {
    println!(
        "  {:>2} deposits {:>2} withdrawals {:>14} cycles {:>7} tx bytes   {:>5.2}% of a block",
        batch.deposits,
        batch.withdrawals,
        thousands(batch.cycles),
        thousands(batch.bytes as u64),
        batch.cycles as f64 * 100.0 / MAX_CYCLES as f64,
    );
}

#[test]
#[ignore = "takes several minutes; run it through `make bench`"]
fn report() {
    println!("\nblock limit (mainnet max_block_cycles): {} cycles", thousands(MAX_CYCLES));
    println!("root history of the measured pools: {ROOT_HISTORY}");

    for variant in ["default", "size", "bext"] {
        let (Some(type_size), Some(intent_size)) = (size(variant, "pool-type-v2"), size(variant, "intent-lock")) else {
            println!("\n== {variant}: not built, skipped (scripts/build-contracts.sh {variant})");
            continue;
        };
        println!(
            "\n== {variant}: pool-type-v2 {} bytes, intent-lock {} bytes",
            thousands(type_size),
            thousands(intent_size)
        );

        let mut env = Env::with_contracts(&contracts_dir(variant));
        let (pool, create) = PoolV2::create(&mut env, &setup());
        let owner = env.user_lock(0xd0);
        let args = pool.intent_args(&Note::from_seed(0).commitment(), &owner);
        let intent_lock = pool.intent_lock(&mut env, &args);
        let intent_cell = plain_cell(&intent_lock, 0).occupied_capacity(Capacity::zero()).expect("capacity").as_u64();
        println!("  create pool {:>12} cycles", thousands(create));
        println!(
            "  pool cell: {} bytes of state, {} CKB; an intent cell occupies {} CKB",
            pool.state.to_bytes().len(),
            pool.capacity / CKB,
            intent_cell / CKB,
        );
        println!(
            "  pool type args {} bytes, intent lock args {} bytes",
            pool.type_script.args().raw_data().len(),
            intent_lock.args().raw_data().len(),
        );

        let deposits: Vec<Batch> = DEPOSIT_BATCHES
            .iter()
            .map(|n| batch(variant, *n, 0).expect("the deposit batch fits a block"))
            .collect();
        deposits.iter().for_each(print_batch);
        let (first, last) = (&deposits[0], &deposits[deposits.len() - 1]);
        let per_deposit = (last.cycles - first.cycles) / u64::from(last.deposits - first.deposits);
        let per_deposit_bytes = (last.bytes - first.bytes) / (last.deposits - first.deposits) as usize;
        println!("  per extra deposit    ~{} cycles, {per_deposit_bytes} tx bytes", thousands(per_deposit));

        let withdrawals: Vec<Batch> = WITHDRAW_BATCHES
            .iter()
            .map(|n| batch(variant, 0, *n).expect("the withdrawal batch fits a block"))
            .collect();
        withdrawals.iter().for_each(print_batch);
        let (first, last) = (&withdrawals[0], &withdrawals[withdrawals.len() - 1]);
        let per_withdrawal = (last.cycles - first.cycles) / u64::from(last.withdrawals - first.withdrawals);
        let per_withdrawal_bytes = (last.bytes - first.bytes) / (last.withdrawals - first.withdrawals) as usize;
        println!("  per extra withdrawal ~{} cycles, {per_withdrawal_bytes} tx bytes", thousands(per_withdrawal));

        print_batch(&batch(variant, 8, 8).expect("the mixed batch fits a block"));

        // The largest all-withdrawal batch that fits one block, predicted from
        // the two measurements above and then tried.
        let most = 1 + (MAX_CYCLES - first.cycles) / per_withdrawal;
        let largest = batch(variant, 0, most as u32).expect("the predicted largest batch fits a block");
        print_batch(&largest);
        println!(
            "    ^ the largest withdrawal batch in one block; as a transaction it weighs {} bytes",
            thousands(largest.weight)
        );
        assert!(batch(variant, 0, most as u32 + 1).is_none(), "one more withdrawal should not fit");
        println!("    {} withdrawals do not fit", most + 1);
    }

    if size("profile", "pool-type-v2").is_none() {
        println!("\n== profile: not built, no stage breakdown (scripts/build-contracts.sh profile)");
        return;
    }
    println!("\n== stage breakdown (profile build: the default build plus cycle checkpoints)");
    for (deposits, withdrawals) in [(4, 0), (0, 2), (4, 2)] {
        let batch = batch("profile", deposits, withdrawals).expect("fits a block");
        println!(
            "\n{deposits} deposits, {withdrawals} withdrawals: {} cycles in total",
            thousands(batch.cycles)
        );
        let mut previous = 0;
        for (stage, cycles) in &batch.stages {
            let label = match stage.as_str() {
                "start" => "VM start, program load",
                "loaded" => "load script, pool state, witness; scan inputs for intents",
                "nullifier" => "root lookup, nullifier set update (256 levels of blake2b)",
                "recipient" => "recipient checks",
                "groth16" => "Groth16 verification",
                "tree" => "append the deposits to the commitment tree",
                "end" => "compare output state and capacity",
                other => other,
            };
            println!("  {:>12}  {label}", thousands(cycles - previous));
            previous = *cycles;
        }
        println!("  {:>12}  pool lock, intent locks, other locks", thousands(batch.cycles - previous));
    }
}
