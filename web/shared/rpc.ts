// Just enough JSON-RPC for the mixer, over fetch: the same calls
// tools/testnet/src/rpc.rs makes. The node is the only source of truth.

import type { OutPointJson, ScriptJson } from "./config";
import { hexNum } from "./hex";

export interface CellOutputJson {
  capacity: string;
  lock: ScriptJson;
  type?: ScriptJson | null;
}

export interface TxJson {
  version: string;
  cell_deps: { out_point: OutPointJson; dep_type: string }[];
  header_deps: string[];
  inputs: { previous_output: OutPointJson; since: string }[];
  outputs: CellOutputJson[];
  outputs_data: string[];
  witnesses: string[];
  hash?: string;
}

export interface LiveCell {
  out_point: OutPointJson;
  output: CellOutputJson;
  data: string;
  block_number?: bigint;
}

export interface ChainTx {
  tx: TxJson;
  status: string;
  block_number?: bigint;
  reason?: string;
}

export class RpcError extends Error {}

const PAGE = 100;
/** Calls put in one batch request; more than this goes in several. */
const BATCH = 50;

export class Rpc {
  constructor(readonly url: string) {}

  private async post(body: unknown): Promise<any> {
    let last: unknown;
    for (let attempt = 0; attempt < 4; attempt++) {
      if (attempt > 0) await new Promise((done) => setTimeout(done, 1000 * attempt));
      try {
        const response = await fetch(this.url, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return await response.json();
      } catch (error) {
        // Transport errors are retried; an error the node returns is final.
        last = error;
      }
    }
    throw new Error(`节点请求失败: ${last instanceof Error ? last.message : last}`);
  }

  async call(method: string, params: unknown[]): Promise<any> {
    const value = await this.post({ id: 1, jsonrpc: "2.0", method, params });
    if (value.error) throw new RpcError(`${method}: ${value.error.message ?? JSON.stringify(value.error)}`);
    return value.result;
  }

  /** Several calls in one request, results in the order asked. */
  async batch(calls: { method: string; params: unknown[] }[]): Promise<any[]> {
    if (calls.length === 0) return [];
    const body = calls.map((call, id) => ({ id, jsonrpc: "2.0", ...call }));
    const values: any[] = await this.post(body);
    if (!Array.isArray(values) || values.length !== calls.length) throw new Error("节点的批量响应不完整");
    const out = new Array(calls.length);
    for (const value of values) {
      if (value.error) throw new RpcError(`${calls[value.id]?.method}: ${value.error.message}`);
      out[value.id] = value.result;
    }
    return out;
  }

  async chain(): Promise<string> {
    return (await this.call("get_blockchain_info", [])).chain;
  }

  async tip(): Promise<bigint> {
    return BigInt(await this.call("get_tip_block_number", []));
  }

  async indexerTip(): Promise<bigint> {
    return BigInt((await this.call("get_indexer_tip", [])).block_number);
  }

  /** The live cells an indexer search key matches, all pages of them. */
  async cells(key: Record<string, unknown>): Promise<LiveCell[]> {
    const cells: LiveCell[] = [];
    let cursor: string | undefined;
    for (;;) {
      const params: unknown[] = [key, "asc", hexNum(PAGE)];
      if (cursor) params.push(cursor);
      const page = await this.call("get_cells", params);
      for (const object of page.objects) {
        cells.push({
          out_point: object.out_point,
          output: object.output,
          data: object.output_data ?? "0x",
          block_number: BigInt(object.block_number),
        });
      }
      if (page.objects.length < PAGE) return cells;
      cursor = page.last_cursor;
    }
  }

  cellsByType(script: ScriptJson): Promise<LiveCell[]> {
    return this.cells({ script, script_type: "type", script_search_mode: "exact" });
  }

  cellsByLockPrefix(script: ScriptJson): Promise<LiveCell[]> {
    return this.cells({ script, script_type: "lock", script_search_mode: "prefix" });
  }

  /** The live cells of `lock` that hold nothing but capacity. */
  async plainCells(lock: ScriptJson): Promise<LiveCell[]> {
    const cells = await this.cells({
      script: lock,
      script_type: "lock",
      script_search_mode: "exact",
      filter: { script_len_range: ["0x0", "0x1"], output_data_len_range: ["0x0", "0x1"] },
    });
    return cells.filter((cell) => !cell.output.type && cell.data === "0x");
  }

  private static chainTx(value: any): ChainTx | undefined {
    if (!value || !value.transaction) return undefined;
    const status = value.tx_status ?? {};
    return {
      tx: value.transaction,
      status: status.status ?? "unknown",
      block_number: status.block_number ? BigInt(status.block_number) : undefined,
      reason: status.reason ?? undefined,
    };
  }

  async transaction(hash: string): Promise<ChainTx | undefined> {
    return Rpc.chainTx(await this.call("get_transaction", [hash]));
  }

  async transactions(hashes: string[]): Promise<(ChainTx | undefined)[]> {
    const out: (ChainTx | undefined)[] = [];
    for (let at = 0; at < hashes.length; at += BATCH) {
      const calls = hashes.slice(at, at + BATCH).map((hash) => ({ method: "get_transaction", params: [hash] }));
      out.push(...(await this.batch(calls)).map(Rpc.chainTx));
    }
    return out;
  }

  /**
   * The transactions that had a cell with this type script among their inputs
   * or outputs, newest first, down to `until` (left out) if it is one of them.
   */
  async typeHistory(script: ScriptJson, until?: string): Promise<string[]> {
    const key = { script, script_type: "type", script_search_mode: "exact", group_by_transaction: true };
    const hashes: string[] = [];
    let cursor: string | undefined;
    for (;;) {
      const params: unknown[] = [key, "desc", hexNum(PAGE)];
      if (cursor) params.push(cursor);
      const page = await this.call("get_transactions", params);
      for (const object of page.objects) {
        if (object.tx_hash === until) return hashes;
        hashes.push(object.tx_hash);
      }
      if (page.objects.length < PAGE) return hashes;
      cursor = page.last_cursor;
    }
  }

  /** Only the status of a transaction: `unknown` if the node has none. */
  async status(hash: string): Promise<{ status: string; reason?: string; block_number?: bigint }> {
    const value = await this.call("get_transaction", [hash]);
    const status = value?.tx_status ?? {};
    return {
      status: status.status ?? "unknown",
      reason: status.reason ?? undefined,
      block_number: status.block_number ? BigInt(status.block_number) : undefined,
    };
  }

  /**
   * Ask the node whether its pool would take `tx`, without submitting it. Runs
   * every script. It does not apply the replace-by-fee rule, so a transaction
   * that passes can still be refused when sent.
   */
  async dryRun(tx: TxJson): Promise<{ cycles: bigint; fee: bigint }> {
    const value = await this.call("test_tx_pool_accept", [tx, "passthrough"]);
    return { cycles: BigInt(value.cycles), fee: BigInt(value.fee) };
  }

  async send(tx: TxJson): Promise<string> {
    return await this.call("send_transaction", [tx, "passthrough"]);
  }
}
