// The pool's transactions, as tools/testnet/src/v2.rs assembles them.

import { ccc } from "@ckb-ccc/core";
import { JsonRpcTransformers } from "@ckb-ccc/core/advancedBarrel";
import { intent_args, reclaim_since } from "../wasm/mixer_wasm.js";
import { DEPS, FEE_RATE, LONGEST_WAIT, PLAIN_CELL_BASE, POOL_TYPE, SHANNONS, type OutPointJson, type ScriptJson } from "./config";
import { concat, fromHex, toHex, u64le } from "./hex";
import { intentLock, poolConfig, poolLock, poolTypeHash, scriptHash, type Pool } from "./pool";
import type { LiveCell, TxJson } from "./rpc";

/** A withdrawal as it is handed to an aggregator (docs/intent-aggregation.md §4.2). No secrets in it. */
export interface Message {
  pool: string;
  root: string;
  nullifier_hash: string;
  recipient: ScriptJson;
  fee: number;
  proof: string;
}

const HEX32 = /^0x[0-9a-f]{64}$/;
const HASH_TYPE_NAMES = ["data", "type", "data1", "data2"];

/** `value` as a well-formed message, or an error saying what is wrong with it. */
export function parseMessage(value: any): Message {
  const bad = (what: string) => new Error(`取款消息不合法: ${what}`);
  if (typeof value !== "object" || value === null) throw bad("不是对象");
  for (const key of ["pool", "root", "nullifier_hash"]) {
    if (typeof value[key] !== "string" || !HEX32.test(value[key])) throw bad(`${key} 不是 32 字节的十六进制`);
  }
  if (typeof value.proof !== "string" || !/^0x[0-9a-f]{256}$/.test(value.proof)) throw bad("proof 不是 128 字节");
  if (!Number.isSafeInteger(value.fee) || value.fee < 0) throw bad("fee 不是非负整数");
  const r = value.recipient;
  if (typeof r !== "object" || r === null) throw bad("没有 recipient");
  if (typeof r.code_hash !== "string" || !HEX32.test(r.code_hash)) throw bad("recipient.code_hash");
  if (!HASH_TYPE_NAMES.includes(r.hash_type)) throw bad("recipient.hash_type");
  if (typeof r.args !== "string" || !/^0x([0-9a-f]{2}){0,200}$/.test(r.args)) throw bad("recipient.args");
  return {
    pool: value.pool,
    root: value.root,
    nullifier_hash: value.nullifier_hash,
    recipient: { code_hash: r.code_hash, hash_type: r.hash_type, args: r.args },
    fee: value.fee,
    proof: value.proof,
  };
}

const dep = (out_point: OutPointJson) => ({ outPoint: { txHash: out_point.tx_hash, index: out_point.index }, depType: "code" as const });
const script = (json: ScriptJson) => JsonRpcTransformers.scriptTo(json as any);
const input = (cell: LiveCell, since = 0n) => ({
  previousOutput: { txHash: cell.out_point.tx_hash, index: cell.out_point.index },
  since,
  cellOutput: { capacity: cell.output.capacity, lock: script(cell.output.lock), type: cell.output.type ? script(cell.output.type) : undefined },
  outputData: cell.data,
});

/** The least a cell under `lock` with no type and no data can hold, in shannons. */
export function plainCellCapacity(lock: ScriptJson): bigint {
  return (PLAIN_CELL_BASE + BigInt(fromHex(lock.args).length)) * SHANNONS;
}

/** Why the pool cannot pay `message` as things stand, if it cannot. Proofs are not verified here. */
export function refusal(pool: Pool, message: Message): string | undefined {
  if (message.pool !== pool.typeHash) return "这条消息是给另一个池子的";
  const fee = BigInt(message.fee);
  if (fee > pool.config.denomination) return "手续费超过面额";
  if (pool.config.denomination - fee < plainCellCapacity(message.recipient)) return "扣掉手续费后不够收款 cell 的最低容量";
  if (pool.ledger.is_spent(fromHex(message.nullifier_hash))) return "这张票据已经取过款";
  if (!pool.ledger.is_known_root(fromHex(message.root))) return "证明所依据的根已经不在池子的历史里，需要重新生成证明";
  return undefined;
}

/**
 * The batch that deposits `intents` and pays `payouts`, in these orders.
 *
 * Complete except for whoever collects what is left over: every intent's
 * capacity beyond the denomination and every withdrawal's fee are inputs that
 * no output claims yet. With one withdrawal and nothing else it is a valid
 * transaction as it stands, needing no signature: the fee goes to the miner.
 */
export function batchDraft(pool: Pool, intents: LiveCell[], payouts: Message[]): ccc.Transaction {
  const requests = concat(
    ...payouts.map((m) => concat(u64le(BigInt(m.fee)), fromHex(m.root), fromHex(m.nullifier_hash), fromHex(m.proof))),
  );
  const commitments = concat(...intents.map((cell) => fromHex(cell.output.lock.args).slice(32, 64)));
  const batch = JSON.parse(pool.ledger.batch(requests, commitments));

  const cellDeps = [dep(DEPS.poolType), dep(DEPS.poolLock)];
  if (intents.length) cellDeps.push(dep(DEPS.intentLock));
  if (payouts.length) cellDeps.push(dep(DEPS.vk));

  const denomination = pool.config.denomination;
  return ccc.Transaction.from({
    cellDeps,
    inputs: [input(pool.cell), ...intents.map((cell) => input(cell))],
    outputs: [
      { capacity: BigInt(batch.capacity), lock: script(poolLock()), type: script(POOL_TYPE) },
      ...payouts.map((m) => ({ capacity: denomination - BigInt(m.fee), lock: script(m.recipient) })),
    ],
    outputsData: [batch.state, ...payouts.map(() => "0x")],
    witnesses: [toHex(ccc.WitnessArgs.from({ inputType: batch.action }).toBytes()), ...intents.map(() => "0x")],
  });
}

/**
 * The lock of an intent for this pool that `owner` can take back after
 * `reclaimAfter` blocks. `reclaimAfter` is a number of blocks: 2^56 or more is
 * refused, since no script would refuse the cell and it could never be reclaimed.
 */
export function newIntentLock(commitment: Uint8Array, owner: ScriptJson, reclaimAfter: bigint): ScriptJson {
  if (reclaimAfter < 0n || reclaimAfter > LONGEST_WAIT) throw new Error("等待的区块数必须小于 2^56");
  const args = intent_args(fromHex(poolTypeHash()), commitment, fromHex(scriptHash(owner)), reclaimAfter);
  return intentLock(toHex(args));
}

/** The capacity an intent holds: the denomination and the aggregator's tip. */
export function intentCapacity(tip: bigint): bigint {
  return poolConfig().denomination + tip;
}

/**
 * A transaction that takes `intent` back without the pool. Still needs an
 * input under the owner's lock (the intent lock demands one), and an output.
 */
export function reclaimDraft(intent: LiveCell, reclaimAfter: bigint): ccc.Transaction {
  return ccc.Transaction.from({
    cellDeps: [dep(DEPS.intentLock)],
    inputs: [input(intent, reclaim_since(reclaimAfter))],
    witnesses: ["0x"],
  });
}

export const txJson = (tx: ccc.Transaction): TxJson => JsonRpcTransformers.transactionFrom(tx) as unknown as TxJson;

/** Bytes a transaction takes in a block. */
export const txSize = (tx: ccc.Transaction): bigint => BigInt(tx.toBytes().length + 4);

/**
 * What a miner weighs a transaction by: its size, or its cycles scaled by
 * block bytes over block cycles, whichever is more. A withdrawal's weight is
 * all cycles.
 */
export function txWeight(size: bigint, cycles: bigint): bigint {
  const byCycles = (cycles * 597_000n + 3_499_999_999n) / 3_500_000_000n;
  return byCycles > size ? byCycles : size;
}

/** The fee for `weight` at `rate` shannons per 1000 bytes, rounded up. */
export const feeFor = (weight: bigint, rate: bigint = FEE_RATE): bigint => (weight * rate + 999n) / 1000n;
