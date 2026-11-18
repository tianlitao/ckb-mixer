// Replays the testnet pool from the chain and prints what it adds up to.
// A check that the wasm build and the sync code agree with the live cell.
//
//   npm run sync

import { readFileSync } from "node:fs";
import { initSync, ckb_hash, vk_from_snarkjs } from "../wasm/mixer_wasm.js";
import { CHAIN, RPC_URL } from "../shared/config";
import { ckb, toHex } from "../shared/hex";
import { liveIntents, syncPool } from "../shared/pool";
import { Rpc } from "../shared/rpc";

initSync({ module: readFileSync(new URL("../wasm/mixer_wasm_bg.wasm", import.meta.url)) });

const rpc = new Rpc(RPC_URL);
if ((await rpc.chain()) !== CHAIN) throw new Error("not the testnet");

let started = Date.now();
const { pool, checkpoint } = await syncPool(rpc);
console.log(`replayed ${pool.replayed} pool transactions in ${Date.now() - started} ms`);
console.log(`pool cell     ${pool.cell.out_point.tx_hash}:${Number(pool.cell.out_point.index)}`);
console.log(`type hash     ${pool.typeHash}`);
console.log(`capacity      ${ckb(pool.ledger.capacity)} CKB, denomination ${ckb(pool.config.denomination)} CKB`);
console.log(`deposits      ${pool.ledger.leaf_count}, withdrawals ${pool.ledger.nullifier_count}`);
console.log(`root          ${toHex(pool.ledger.current_root())}`);

started = Date.now();
const again = await syncPool(rpc, checkpoint);
console.log(`from the checkpoint: ${again.pool.replayed} transactions, ${Date.now() - started} ms, ${checkpoint.snapshot.length} bytes of snapshot`);

const vk = readFileSync(new URL("../public/circuit/verification_key.json", import.meta.url), "utf8");
const vkHash = toHex(ckb_hash(vk_from_snarkjs(vk)));
console.log(`vk            ${vkHash === pool.config.vk_hash ? "the bundled circuit is the pool's" : `MISMATCH: ${vkHash} vs ${pool.config.vk_hash}`}`);

for (const { cell, args } of await liveIntents(rpc)) {
  console.log(`intent        ${cell.out_point.tx_hash}:${Number(cell.out_point.index)} ${ckb(BigInt(cell.output.capacity))} CKB, reclaim after ${args.reclaim_after}`);
}
