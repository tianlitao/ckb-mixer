// The pool, rebuilt from the chain: what tools/testnet/src/v2.rs does on the
// host, with the replay itself done by the wasm build of mixer-core-v2.
//
// Nothing here is trusted from a cache or a server: whatever the replay starts
// from, it has to end at the state and capacity of the live pool cell, byte
// for byte, or the sync fails.

import { ccc } from "@ckb-ccc/core";
import { Ledger, parse_intent_args, pool_config } from "../wasm/mixer_wasm.js";
import { POOL_TYPE, type ScriptJson } from "./config";
import { fromHex, toHex, concat } from "./hex";
import type { ChainTx, LiveCell, Rpc } from "./rpc";

export interface PoolConfig {
  type_id: string;
  denomination: bigint;
  vk_hash: string;
  pool_lock: ScriptId;
  intent_lock: ScriptId;
  root_history: number;
}

interface ScriptId {
  code_hash: string;
  hash_type: string;
}

/** A ledger as of the pool cell created by `tx`, to start the next sync from. */
export interface Checkpoint {
  tx: string;
  snapshot: string;
}

export interface Pool {
  config: PoolConfig;
  /** Hash of the pool's type script: what intents and the pool lock name. */
  typeHash: string;
  /** The live pool cell. */
  cell: LiveCell;
  ledger: Ledger;
  /** Pool transactions replayed by this sync. */
  replayed: number;
}

const HASH_TYPES: Record<number, string> = { 0: "data", 1: "type", 2: "data1", 4: "data2" };

const POOL_ARGS = fromHex(POOL_TYPE.args);

export const sameScript = (a: ScriptJson | null | undefined, b: ScriptJson): boolean =>
  !!a && a.code_hash.toLowerCase() === b.code_hash.toLowerCase() && a.hash_type === b.hash_type && a.args.toLowerCase() === b.args.toLowerCase();

export function scriptHash(script: ScriptJson): string {
  return ccc.Script.from({ codeHash: script.code_hash, hashType: script.hash_type as ccc.HashTypeLike, args: script.args }).hash();
}

let config: PoolConfig | undefined;

/** The pool's parameters, read from the args of its type script. */
export function poolConfig(): PoolConfig {
  if (!config) {
    const raw = JSON.parse(pool_config(POOL_ARGS));
    const id = (value: { code_hash: string; hash_type: number }): ScriptId => {
      const hash_type = HASH_TYPES[value.hash_type];
      if (!hash_type) throw new Error(`unknown hash type ${value.hash_type}`);
      return { code_hash: value.code_hash, hash_type };
    };
    config = {
      type_id: raw.type_id,
      denomination: BigInt(raw.denomination),
      vk_hash: raw.vk_hash,
      pool_lock: id(raw.pool_lock),
      intent_lock: id(raw.intent_lock),
      root_history: raw.root_history,
    };
  }
  return config;
}

let typeHash: string | undefined;

export function poolTypeHash(): string {
  return (typeHash ??= scriptHash(POOL_TYPE));
}

/** The pool cell's lock: the pool lock, bound to the pool's type script. */
export function poolLock(): ScriptJson {
  return { ...poolConfig().pool_lock, args: poolTypeHash() };
}

/**
 * The intent lock for these args, named exactly as the pool's args name it.
 * A cell under the same code by another hash type is not a deposit.
 */
export function intentLock(args: string): ScriptJson {
  return { ...poolConfig().intent_lock, args };
}

export interface IntentArgs {
  pool_type_hash: string;
  commitment: string;
  owner_lock_hash: string;
  reclaim_after: bigint;
  /** Whether any `since` can express the wait. */
  reclaimable: boolean;
  /** Whether the commitment is a field element; the pool takes no other. */
  canonical: boolean;
}

export function intentArgs(args: string): IntentArgs | undefined {
  const json = parse_intent_args(fromHex(args));
  if (!json) return undefined;
  const raw = JSON.parse(json);
  return { ...raw, reclaim_after: BigInt(raw.reclaim_after) };
}

/** The commitment `lock` deposits when a cell under it is spent next to the pool cell. */
function depositedBy(lock: ScriptJson): string | undefined {
  const id = poolConfig().intent_lock;
  if (lock.code_hash.toLowerCase() !== id.code_hash || lock.hash_type !== id.hash_type) return undefined;
  const args = intentArgs(lock.args);
  return args && args.pool_type_hash === poolTypeHash() ? args.commitment : undefined;
}

/** A pool transaction: the input that spends the pool cell, and every input's lock. */
interface Link {
  found: ChainTx;
  poolInput?: { index: number; tx: string };
  locks: ScriptJson[];
}

/** Puts the transactions `hashes` name into `known`: those it lacks, in one batch. */
async function load(rpc: Rpc, known: Map<string, ChainTx>, hashes: string[]) {
  const wanted = [...new Set(hashes)].filter((h) => !known.has(h));
  const found = await rpc.transactions(wanted);
  wanted.forEach((h, i) => {
    const tx = found[i];
    if (!tx) throw new Error(`节点没有交易 ${h}`);
    known.set(h, tx);
  });
}

const spentBy = ({ tx }: ChainTx) => tx.inputs.map((input) => input.previous_output.tx_hash);

async function link(rpc: Rpc, hash: string, known: Map<string, ChainTx>): Promise<Link> {
  await load(rpc, known, [hash]);
  const found = known.get(hash)!;
  if (found.status !== "committed") throw new Error(`池子交易 ${hash} 还没有上链`);
  await load(rpc, known, spentBy(found));

  const out: Link = { found, locks: [] };
  found.tx.inputs.forEach((input, index) => {
    const previous = input.previous_output;
    const spent = known.get(previous.tx_hash)!.tx.outputs[Number(previous.index)];
    if (!spent) throw new Error(`${hash} 花费了不存在的输出`);
    if (sameScript(spent.type, POOL_TYPE)) out.poolInput = { index, tx: previous.tx_hash };
    out.locks.push(spent.lock);
  });
  return out;
}

function apply(ledger: Ledger, { found, poolInput, locks }: Link) {
  const witness = found.tx.witnesses[poolInput!.index];
  const action = witness && witness !== "0x" ? ccc.WitnessArgs.fromBytes(witness).inputType : undefined;
  if (!action) throw new Error(`${found.tx.hash}: 池子输入没有动作`);
  const commitments = locks.map(depositedBy).filter((c): c is string => !!c);
  ledger.apply(fromHex(action), concat(...commitments.map(fromHex)));
}

async function replay(rpc: Rpc, cell: LiveCell, checkpoint?: Checkpoint): Promise<{ ledger: Ledger; replayed: number }> {
  if (checkpoint?.tx === cell.out_point.tx_hash) {
    return { ledger: Ledger.restore(POOL_ARGS, checkpoint.snapshot), replayed: 0 };
  }
  // The walk below asks for one pool transaction at a time, then for what it
  // spends: two requests each. The indexer can say which ones it will be, so
  // they are fetched ahead in a few batches. Only a shortcut: the walk still
  // follows the pool cell itself, and asks for whatever is missing here.
  const known = new Map<string, ChainTx>();
  const history = await rpc.typeHistory(POOL_TYPE, checkpoint?.tx);
  await load(rpc, known, history);
  await load(rpc, known, history.flatMap((hash) => spentBy(known.get(hash)!)));

  // Walk back from the live cell to the checkpoint, or to the creation.
  const links: Link[] = [];
  let ledger: Ledger | undefined;
  for (let hash = cell.out_point.tx_hash; !ledger; ) {
    const here = await link(rpc, hash, known);
    if (!here.poolInput) {
      const output = here.found.tx.outputs.find((o) => sameScript(o.type, POOL_TYPE));
      if (!output) throw new Error(`${hash} 不是池子的交易`);
      ledger = new Ledger(POOL_ARGS, BigInt(output.capacity));
      break;
    }
    links.push(here);
    if (checkpoint?.tx === here.poolInput.tx) ledger = Ledger.restore(POOL_ARGS, checkpoint.snapshot);
    else hash = here.poolInput.tx;
  }
  for (const here of links.reverse()) apply(ledger, here);
  return { ledger, replayed: links.length };
}

const matches = (ledger: Ledger, cell: LiveCell) =>
  toHex(ledger.state_bytes()) === cell.data.toLowerCase() && ledger.capacity === BigInt(cell.output.capacity);

/** The pool as it is now. The Type ID in the type script's args makes it the only cell of that type. */
export async function syncPool(rpc: Rpc, checkpoint?: Checkpoint): Promise<{ pool: Pool; checkpoint: Checkpoint }> {
  const cells = await rpc.cellsByType(POOL_TYPE);
  if (cells.length !== 1) throw new Error(`有 ${cells.length} 个带池子 type script 的活 cell，应当恰好一个`);
  const cell = cells[0];
  if (!sameScript(cell.output.lock, poolLock())) throw new Error("池子 cell 的 lock 不是池子的 pool lock");

  let result: { ledger: Ledger; replayed: number } | undefined;
  if (checkpoint) {
    try {
      result = await replay(rpc, cell, checkpoint);
      if (!matches(result.ledger, cell)) result = undefined;
    } catch {
      result = undefined; // a stale or damaged cache: start over from the chain
    }
  }
  result ??= await replay(rpc, cell);
  if (!matches(result.ledger, cell)) {
    throw new Error(`${cell.out_point.tx_hash} 之前的交易重放出的状态和池子 cell 里的不一致`);
  }
  const pool: Pool = { config: poolConfig(), typeHash: poolTypeHash(), cell, ledger: result.ledger, replayed: result.replayed };
  return { pool, checkpoint: { tx: cell.out_point.tx_hash, snapshot: result.ledger.snapshot() } };
}

/** Every live cell under the pool's intent lock whose args bind it to this pool. */
export async function liveIntents(rpc: Rpc): Promise<{ cell: LiveCell; args: IntentArgs }[]> {
  const cells = await rpc.cellsByLockPrefix(intentLock(poolTypeHash()));
  const out = [];
  for (const cell of cells) {
    const args = intentArgs(cell.output.lock.args);
    if (args && args.pool_type_hash === poolTypeHash()) out.push({ cell, args });
  }
  return out;
}
