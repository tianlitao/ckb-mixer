// Replays the testnet pool from the chain and prints what it adds up to.
// A check that the wasm build and the sync code agree with the live cell, and
// a count of what a sync costs: a Worker is allowed only so many requests and
// so much CPU per run.
//
//   npm run sync
//   npm run sync -- --checkpoint file    also writes the checkpoint, to seed a Worker's KV with

import { readFileSync, writeFileSync } from "node:fs";
import { initSync, ckb_hash, vk_from_snarkjs } from "../wasm/mixer_wasm.js";
import { CHAIN, RPC_URL } from "../shared/config";
import { ckb, toHex } from "../shared/hex";
import { liveIntents, syncPool } from "../shared/pool";
import { Rpc } from "../shared/rpc";

initSync({ module: readFileSync(new URL("../wasm/mixer_wasm_bg.wasm", import.meta.url)) });

let requests = 0;
const plainFetch = globalThis.fetch;
globalThis.fetch = (...args) => (requests++, plainFetch(...args));

/** Runs `work` and says what it took: wall time, requests to the node, CPU. */
async function measured<T>(work: () => Promise<T>): Promise<{ value: T; cost: string }> {
  const [wall, cpu, before] = [Date.now(), process.cpuUsage(), requests];
  const value = await work();
  const used = process.cpuUsage(cpu);
  const ms = Math.round((used.user + used.system) / 1000);
  return { value, cost: `${Date.now() - wall} ms, ${requests - before} requests, ${ms} ms of CPU` };
}

const rpc = new Rpc(RPC_URL);
if ((await rpc.chain()) !== CHAIN) throw new Error("not the testnet");

const first = await measured(() => syncPool(rpc));
const { pool, checkpoint } = first.value;
console.log(`replayed ${pool.replayed} pool transactions: ${first.cost}`);
console.log(`pool cell     ${pool.cell.out_point.tx_hash}:${Number(pool.cell.out_point.index)}`);
console.log(`type hash     ${pool.typeHash}`);
console.log(`capacity      ${ckb(pool.ledger.capacity)} CKB, denomination ${ckb(pool.config.denomination)} CKB`);
console.log(`deposits      ${pool.ledger.leaf_count}, withdrawals ${pool.ledger.nullifier_count}`);
console.log(`root          ${toHex(pool.ledger.current_root())}`);

const again = await measured(() => syncPool(rpc, checkpoint));
console.log(`from the checkpoint: ${again.value.pool.replayed} transactions: ${again.cost}; ${checkpoint.snapshot.length} bytes of snapshot`);

const vk = readFileSync(new URL("../public/circuit/verification_key.json", import.meta.url), "utf8");
const vkHash = toHex(ckb_hash(vk_from_snarkjs(vk)));
console.log(`vk            ${vkHash === pool.config.vk_hash ? "the bundled circuit is the pool's" : `MISMATCH: ${vkHash} vs ${pool.config.vk_hash}`}`);

for (const { cell, args } of await liveIntents(rpc)) {
  console.log(`intent        ${cell.out_point.tx_hash}:${Number(cell.out_point.index)} ${ckb(BigInt(cell.output.capacity))} CKB, reclaim after ${args.reclaim_after}`);
}

const at = process.argv.indexOf("--checkpoint");
if (at > 0) {
  const file = process.argv[at + 1];
  if (!file) throw new Error("--checkpoint needs a file to write");
  writeFileSync(file, JSON.stringify(checkpoint));
  console.log(`checkpoint    written to ${file}`);
}
