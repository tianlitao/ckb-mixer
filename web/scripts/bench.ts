// How much CPU the aggregator's own work takes, apart from waiting on the node:
// restoring the ledger from its checkpoint and building a batch.
//
//   npx tsx scripts/bench.ts
import { readFileSync } from "node:fs";
import { initSync, Ledger } from "../wasm/mixer_wasm.js";
import { POOL_TYPE, RPC_URL } from "../shared/config";
import { fromHex } from "../shared/hex";
import { syncPool } from "../shared/pool";
import { Rpc } from "../shared/rpc";

initSync({ module: readFileSync(new URL("../wasm/mixer_wasm_bg.wasm", import.meta.url)) });
const { pool, checkpoint } = await syncPool(new Rpc(RPC_URL));
const args = fromHex(POOL_TYPE.args);
const time = (label: string, run: () => unknown) => {
  const started = performance.now();
  run();
  console.log(`${label}: ${(performance.now() - started).toFixed(1)} ms`);
};
console.log(`${pool.ledger.leaf_count} leaves, ${pool.ledger.nullifier_count} nullifiers`);
time("restore from the checkpoint", () => Ledger.restore(args, checkpoint.snapshot));
const one = (seed: number) => {
  const request = new Uint8Array(200);
  request.set(pool.ledger.current_root(), 8);
  request[40] = seed;
  return request;
};
for (const count of [1, 4]) {
  const requests = new Uint8Array(200 * count);
  for (let i = 0; i < count; i++) requests.set(one(i + 1), 200 * i);
  time(`batch of ${count} withdrawal(s)`, () => pool.ledger.batch(requests, new Uint8Array(0)));
}
time("batch of 16 deposits", () => {
  const commitments = new Uint8Array(32 * 16);
  for (let i = 0; i < 16; i++) commitments[32 * i] = i + 1;
  pool.ledger.batch(new Uint8Array(0), commitments);
});
