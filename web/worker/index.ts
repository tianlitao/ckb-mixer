// The aggregator, as a Cloudflare Worker (docs/intent-aggregation.md §4.2).
//
//   POST /withdrawals              take a withdrawal message
//   GET  /withdrawals/<nullifier>  what became of it
//   GET  /info                     who this aggregator is and what it asks
//   cron                           batch the waiting intents and withdrawals
//
// It holds no user secrets and nobody's funds: a message it sits on can be
// sent by anyone else, and an intent it ignores can be reclaimed. Its own key
// only pays fees and collects tips.

import { ccc } from "@ckb-ccc/core";
import wasmModule from "../wasm/mixer_wasm_bg.wasm";
import { initSync } from "../wasm/mixer_wasm.js";
import { CHAIN, FEE_RATE, RPC_URL } from "../shared/config";
import { liveIntents, syncPool, type Checkpoint, type Pool } from "../shared/pool";
import { Rpc, RpcError, type LiveCell } from "../shared/rpc";
import { batchDraft, feeFor, parseMessage, refusal, txJson, txSize, txWeight, type Message } from "../shared/tx";

initSync({ module: wasmModule });

export interface Env {
  STATE: KVNamespace;
  /** The aggregator's secp256k1 private key, hex. A secret: `wrangler secret put AGGREGATOR_KEY`. */
  AGGREGATOR_KEY: string;
  RPC_URL?: string;
  /** Least fee a withdrawal has to offer, in shannons. */
  MIN_WITHDRAW_FEE?: string;
  /** Least tip an intent has to carry, in shannons. */
  MIN_TIP?: string;
  /** Intents this close to being reclaimable are left to their owners, in blocks. */
  MIN_BLOCKS_LEFT?: string;
  MAX_WITHDRAWALS?: string;
  MAX_INTENTS?: string;
}

interface Settings {
  rpcUrl: string;
  minFee: bigint;
  minTip: bigint;
  minBlocksLeft: bigint;
  maxWithdrawals: number;
  maxIntents: number;
}

const settings = (env: Env): Settings => ({
  rpcUrl: env.RPC_URL || RPC_URL,
  // A withdrawal alone weighs about 17,700 bytes (it is all cycles): 88,500
  // shannons at this code's fee rate. Below that it is not worth carrying.
  minFee: BigInt(env.MIN_WITHDRAW_FEE || 200_000),
  minTip: BigInt(env.MIN_TIP || 1_000_000),
  minBlocksLeft: BigInt(env.MIN_BLOCKS_LEFT || 10),
  maxWithdrawals: Number(env.MAX_WITHDRAWALS || 4),
  maxIntents: Number(env.MAX_INTENTS || 16),
});

/** A withdrawal this aggregator was handed. */
interface Entry {
  message: Message;
  received: number;
  status: "queued" | "sent" | "settled" | "dropped";
  /** The batch it went into. */
  tx?: string;
  reason?: string;
}

/** The batch this aggregator sent last, until the chain has it. */
interface Pending {
  hash: string;
  withdrawals: string[];
  intents: number;
  sent: number;
}

const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "content-type",
  "access-control-max-age": "86400",
};

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...CORS } });

const key = (nullifierHash: string) => `w:${nullifierHash}`;

function signerOf(env: Env, url: string) {
  if (!/^(0x)?[0-9a-fA-F]{64}$/.test(env.AGGREGATOR_KEY ?? "")) throw new Error("AGGREGATOR_KEY is not set");
  const client = new ccc.ClientPublicTestnet({ url });
  const privateKey = env.AGGREGATOR_KEY.startsWith("0x") ? env.AGGREGATOR_KEY : `0x${env.AGGREGATOR_KEY}`;
  return new ccc.SignerCkbPrivateKey(client, privateKey);
}

async function loadPool(env: Env, rpc: Rpc): Promise<Pool> {
  const saved = await env.STATE.get<Checkpoint>("checkpoint", "json");
  const { pool, checkpoint } = await syncPool(rpc, saved ?? undefined);
  if (checkpoint.tx !== saved?.tx) await env.STATE.put("checkpoint", JSON.stringify(checkpoint));
  return pool;
}

/** A script said no, as opposed to the node not knowing a cell or being busy. */
const scriptRefused = (error: unknown) => error instanceof RpcError && /ScriptError|ValidationFailure|error code/i.test(error.message);

/** Whether the pool would pay `message` now, by a dry run of the batch holding only it. */
async function verify(rpc: Rpc, pool: Pool, message: Message): Promise<string | undefined> {
  try {
    await rpc.dryRun(txJson(batchDraft(pool, [], [message])));
    return undefined;
  } catch (error) {
    if (scriptRefused(error)) return (error as Error).message;
    throw error;
  }
}

async function accept(request: Request, env: Env): Promise<Response> {
  const config = settings(env);
  let message: Message;
  try {
    message = parseMessage(await request.json());
  } catch (error) {
    return json({ error: (error as Error).message }, 400);
  }
  if (BigInt(message.fee) < config.minFee) return json({ error: `手续费低于这个聚合者的下限 ${config.minFee} shannons` }, 400);

  const rpc = new Rpc(config.rpcUrl);
  const pool = await loadPool(env, rpc);
  const refused = refusal(pool, message);
  if (refused) return json({ error: refused }, 400);
  // The proof is checked by the scripts themselves, on the node.
  const failed = await verify(rpc, pool, message);
  if (failed) return json({ error: `池子的脚本拒绝了这笔取款: ${failed}` }, 400);

  const entry: Entry = { message, received: Date.now(), status: "queued" };
  await env.STATE.put(key(message.nullifier_hash), JSON.stringify(entry));
  return json({ status: entry.status, nullifier_hash: message.nullifier_hash });
}

async function lookup(nullifierHash: string, env: Env): Promise<Response> {
  const entry = await env.STATE.get<Entry>(key(nullifierHash), "json");
  if (!entry) return json({ error: "这个聚合者没有收到过这笔取款" }, 404);
  return json({ status: entry.status, tx: entry.tx, reason: entry.reason, received: entry.received });
}

async function info(env: Env): Promise<Response> {
  const config = settings(env);
  const signer = signerOf(env, config.rpcUrl);
  return json({
    chain: CHAIN,
    address: await signer.getRecommendedAddress(),
    min_withdraw_fee: config.minFee.toString(),
    min_tip: config.minTip.toString(),
    min_blocks_left: config.minBlocksLeft.toString(),
    pending: await env.STATE.get<Pending>("pending", "json"),
    last: await env.STATE.get("last", "json"),
  });
}

/** The transaction `draft` becomes once this aggregator collects what it leaves over and pays the miner. */
async function finish(draft: ccc.Transaction, signer: ccc.Signer, rpc: Rpc): Promise<{ tx: ccc.Transaction; fee: bigint; cycles: bigint }> {
  const fund = async (rate: bigint) => {
    const tx = draft.clone();
    await tx.completeInputsByCapacity(signer);
    await tx.completeFeeBy(signer, rate, undefined, { maxFeeRate: 1_000_000_000n });
    return await signer.signTransaction(tx);
  };
  // By size first; the dry run then says what it weighs.
  let tx = await fund(FEE_RATE);
  const { cycles } = await rpc.dryRun(txJson(tx));
  const size = txSize(tx);
  const weight = txWeight(size, cycles);
  if (weight > size) {
    tx = await fund((FEE_RATE * weight) / size + 1n);
    await rpc.dryRun(txJson(tx));
  }
  return { tx, fee: await tx.getFee(signer.client), cycles };
}

async function tick(env: Env): Promise<Record<string, unknown>> {
  const config = settings(env);
  const rpc = new Rpc(config.rpcUrl);
  const state = env.STATE;

  // One batch at a time: a second one would spend the same pool cell.
  const pending = await state.get<Pending>("pending", "json");
  if (pending) {
    const { status } = await rpc.status(pending.hash);
    if (status === "pending" || status === "proposed") return { waiting: pending.hash, status };
    for (const hash of pending.withdrawals) {
      const entry = await state.get<Entry>(key(hash), "json");
      if (!entry) continue;
      // Not committed: it goes back in the queue and is looked at afresh below.
      const next: Entry = status === "committed" ? { ...entry, status: "settled", tx: pending.hash } : { ...entry, status: "queued", tx: undefined };
      await state.put(key(hash), JSON.stringify(next));
    }
    await state.delete("pending");
  }

  const pool = await loadPool(env, rpc);
  const [tip, indexed, live] = await Promise.all([
    rpc.tip(),
    rpc.indexerTip(),
    rpc.call("get_live_cell", [pool.cell.out_point, false]),
  ]);
  // The indexer runs a little behind the chain: the pool cell it reports may be spent already.
  if (live.status !== "live") return { skipped: "the indexer is behind the pool", tip: tip.toString(), indexed: indexed.toString() };

  const denomination = pool.config.denomination;
  const intents: LiveCell[] = (await liveIntents(rpc))
    .filter(({ cell, args }) => {
      if (cell.output.type || cell.data !== "0x" || !args.canonical) return false;
      if (BigInt(cell.output.capacity) < denomination + config.minTip) return false;
      // Its owner may be about to take it back; a batch spending it would then fail whole.
      return (cell.block_number ?? 0n) + args.reclaim_after >= tip + config.minBlocksLeft;
    })
    .map(({ cell }) => cell)
    .sort((a, b) => (BigInt(b.output.capacity) > BigInt(a.output.capacity) ? 1 : -1))
    .slice(0, config.maxIntents);

  const queued: Entry[] = [];
  const listed = await state.list({ prefix: "w:", limit: 100 });
  for (const { name } of listed.keys) {
    const entry = await state.get<Entry>(name, "json");
    if (!entry || entry.status !== "queued") continue;
    const refused = refusal(pool, entry.message);
    if (!refused) {
      queued.push(entry);
    } else if (pool.ledger.is_spent(ccc.bytesFrom(entry.message.nullifier_hash))) {
      // Someone else sent it, the user by the fallback path for one.
      await state.put(name, JSON.stringify({ ...entry, status: "settled" } satisfies Entry));
    } else {
      await state.put(name, JSON.stringify({ ...entry, status: "dropped", reason: refused } satisfies Entry));
    }
  }
  queued.sort((a, b) => b.message.fee - a.message.fee || a.received - b.received);
  const payouts = queued.slice(0, config.maxWithdrawals).map((entry) => entry.message);
  if (intents.length === 0 && payouts.length === 0) return { idle: true, deposits: pool.ledger.leaf_count, withdrawals: pool.ledger.nullifier_count };

  const signer = signerOf(env, config.rpcUrl);
  let result: Awaited<ReturnType<typeof finish>>;
  try {
    result = await finish(batchDraft(pool, intents, payouts), signer, rpc);
  } catch (error) {
    if (!scriptRefused(error)) throw error;
    // Something in the batch is refused. Find which withdrawals, drop those, and try again next time.
    let dropped = 0;
    for (const message of payouts) {
      const failed = await verify(rpc, pool, message);
      if (!failed) continue;
      const entry = queued.find((e) => e.message.nullifier_hash === message.nullifier_hash)!;
      await state.put(key(message.nullifier_hash), JSON.stringify({ ...entry, status: "dropped", reason: failed } satisfies Entry));
      dropped++;
    }
    return { failed: (error as Error).message, dropped };
  }

  const income =
    intents.reduce((sum, cell) => sum + BigInt(cell.output.capacity) - denomination, 0n) + payouts.reduce((sum, m) => sum + BigInt(m.fee), 0n);
  if (income < result.fee) return { skipped: "the batch would cost more than it brings", income: income.toString(), fee: result.fee.toString() };

  // The dry run does not apply the replace-by-fee rule: if someone's batch on
  // the same pool cell is in the node's pool already, this throws, and the
  // next tick starts over from whatever the chain has by then.
  const hash = await rpc.send(txJson(result.tx));
  const sent: Pending = { hash, withdrawals: payouts.map((m) => m.nullifier_hash), intents: intents.length, sent: Date.now() };
  await state.put("pending", JSON.stringify(sent));
  for (const message of payouts) {
    const entry = queued.find((e) => e.message.nullifier_hash === message.nullifier_hash)!;
    await state.put(key(message.nullifier_hash), JSON.stringify({ ...entry, status: "sent", tx: hash } satisfies Entry));
  }
  return {
    sent: hash,
    deposits: intents.length,
    withdrawals: payouts.length,
    cycles: result.cycles.toString(),
    fee: result.fee.toString(),
    income: income.toString(),
  };
}

async function runTick(env: Env): Promise<Record<string, unknown>> {
  let outcome: Record<string, unknown>;
  try {
    outcome = await tick(env);
  } catch (error) {
    outcome = { error: error instanceof Error ? error.message : String(error) };
  }
  console.log("tick", JSON.stringify(outcome));
  // KV writes are limited: an idle tick leaves no trace.
  if (outcome.sent || outcome.error || outcome.failed) await env.STATE.put("last", JSON.stringify({ at: Date.now(), ...outcome }));
  return outcome;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    try {
      if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });
      if (request.method === "POST" && url.pathname === "/withdrawals") return await accept(request, env);
      const match = /^\/withdrawals\/(0x[0-9a-f]{64})$/.exec(url.pathname);
      if (request.method === "GET" && match) return await lookup(match[1], env);
      if (request.method === "GET" && url.pathname === "/info") return await info(env);
      return json({ error: "not found" }, 404);
    } catch (error) {
      return json({ error: error instanceof Error ? error.message : String(error) }, 500);
    }
  },

  async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
    await runTick(env);
  },
} satisfies ExportedHandler<Env>;
