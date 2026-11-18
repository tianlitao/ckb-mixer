import { ccc } from "@ckb-ccc/ccc";
import * as snarkjs from "snarkjs";
import initWasm, { ckb_hash, nullifier_hash, proof_from_snarkjs, vk_from_snarkjs } from "../wasm/mixer_wasm.js";
import { CHAIN, DEFAULT_WITHDRAW_FEE, EXPLORER, FEE_RATE, LONGEST_WAIT, RPC_URL, type ScriptJson } from "../shared/config";
import { ckb, fromHex, parseCkb, toHex } from "../shared/hex";
import { intentArgs, liveIntents, scriptHash, syncPool, type Checkpoint, type IntentArgs, type Pool } from "../shared/pool";
import { Rpc, type LiveCell } from "../shared/rpc";
import {
  batchDraft,
  feeFor,
  intentCapacity,
  newIntentLock,
  plainCellCapacity,
  reclaimDraft,
  refusal,
  txJson,
  txSize,
  txWeight,
  type Message,
} from "../shared/tx";
import { loadNotes, newNote, noteString, parseNote, saveNotes, type Note } from "./notes";

const CHECKPOINT = "ckbmix2.testnet.checkpoint";
const AGGREGATOR = "ckbmix2.testnet.aggregator";
const DEFAULT_AGGREGATOR: string = import.meta.env.VITE_AGGREGATOR_URL ?? "http://localhost:8787";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const escape = (text: unknown) =>
  String(text).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const short = (hash: string) => `${hash.slice(0, 10)}…${hash.slice(-6)}`;
const txLink = (hash: string) => `<a href="${EXPLORER}/transaction/${hash}" target="_blank" rel="noreferrer" class="mono">${short(hash)}</a>`;

const rpc = new Rpc(RPC_URL);
const client = new ccc.ClientPublicTestnet({ url: RPC_URL });

let pool: Pool | undefined;
let intents: { cell: LiveCell; args: IntentArgs }[] = [];
let tip = 0n;
let signer: ccc.Signer | undefined;
let owner: { address: string; lock: ScriptJson; hash: string } | undefined;
let notes: Note[] = loadNotes();
/** A note made but not yet deposited, waiting for its backup to be confirmed. */
let fresh: Note | undefined;
/** The note being withdrawn, and the message proved for it. */
let withdrawing: { note: Note; leaf: number; message?: Message } | undefined;
const aggregatorStatus = new Map<string, { status: string; tx?: string; reason?: string }>();

function log(html: string, bad = false) {
  const line = document.createElement("div");
  if (bad) line.className = "bad";
  line.innerHTML = `${new Date().toLocaleTimeString()} · ${html}`;
  $("log").prepend(line);
}

const describe = (error: unknown) => escape(error instanceof Error ? error.message : String(error));

/** Run a button's action, with the button disabled meanwhile and failures logged. */
function action(button: HTMLButtonElement, run: () => Promise<void>) {
  button.addEventListener("click", async () => {
    const label = button.textContent;
    button.disabled = true;
    try {
      await run();
    } catch (error) {
      log(`${escape(label)}失败: ${describe(error)}`, true);
    } finally {
      button.textContent = label;
      button.disabled = false;
      render();
    }
  });
}

const scriptJson = (script: ccc.Script): ScriptJson => ({ code_hash: script.codeHash, hash_type: script.hashType, args: script.args });

// ---- the chain

async function refresh() {
  const saved = localStorage.getItem(CHECKPOINT);
  let checkpoint: Checkpoint | undefined;
  try {
    checkpoint = saved ? JSON.parse(saved) : undefined;
  } catch {
    checkpoint = undefined;
  }
  const synced = await syncPool(rpc, checkpoint);
  pool = synced.pool;
  if (synced.checkpoint.tx !== checkpoint?.tx) localStorage.setItem(CHECKPOINT, JSON.stringify(synced.checkpoint));
  [intents, tip] = await Promise.all([liveIntents(rpc), rpc.tip()]);
  await pollAggregator();
  render();
}

async function pollAggregator() {
  for (const note of notes) {
    const sent = note.withdrawal;
    if (sent?.via !== "aggregator" || aggregatorStatus.get(sent.message.nullifier_hash)?.status === "settled") continue;
    try {
      const response = await fetch(`${aggregatorUrl()}/withdrawals/${sent.message.nullifier_hash}`);
      if (response.ok) aggregatorStatus.set(sent.message.nullifier_hash, await response.json());
    } catch {
      // The aggregator being away changes nothing about what the chain says.
    }
  }
}

// ---- notes and what the chain says about them

type Status =
  | { kind: "new" }
  | { kind: "sent"; tx: string }
  | { kind: "waiting"; cell: LiveCell; args: IntentArgs; left: bigint }
  | { kind: "deposited"; leaf: number }
  | { kind: "withdrawn"; leaf: number }
  | { kind: "reclaimed" };

function statusOf(note: Note): Status {
  if (!pool) return { kind: "new" };
  const leaf = pool.ledger.leaf_index(fromHex(note.commitment));
  if (leaf !== undefined) {
    const spent = pool.ledger.is_spent(nullifier_hash(fromHex(note.nullifier), leaf));
    return { kind: spent ? "withdrawn" : "deposited", leaf };
  }
  const intent = intents.find(({ args }) => args.commitment === note.commitment);
  if (intent) {
    const matures = (intent.cell.block_number ?? tip) + intent.args.reclaim_after;
    return { kind: "waiting", ...intent, left: matures > tip ? matures - tip : 0n };
  }
  if (note.reclaimed) return { kind: "reclaimed" };
  if (note.intent) return { kind: "sent", tx: note.intent.tx };
  return { kind: "new" };
}

function update(note: Note, change: Partial<Note>) {
  Object.assign(note, change);
  if (!notes.includes(note)) notes.push(note);
  saveNotes(notes);
}

// ---- rendering

function render() {
  if (pool) {
    const cell = pool.cell.out_point;
    const waiting = intents.filter(({ cell }) => !cell.output.type && cell.data === "0x");
    $("pool").innerHTML = `
      <dt>面额</dt><dd>${ckb(pool.config.denomination)} CKB</dd>
      <dt>池内</dt><dd>${ckb(pool.ledger.capacity)} CKB（其中 ${pool.ledger.leaf_count - pool.ledger.nullifier_count} 张票据未取）</dd>
      <dt>存款 / 取款</dt><dd>${pool.ledger.leaf_count} / ${pool.ledger.nullifier_count}</dd>
      <dt>等待成交的意向</dt><dd>${waiting.length}</dd>
      <dt>池子 cell</dt><dd>${txLink(cell.tx_hash)}:${Number(cell.index)}</dd>
      <dt>当前根</dt><dd class="mono">${toHex(pool.ledger.current_root())}</dd>
      <dt>链高</dt><dd>${tip}</dd>
      <dt>校验</dt><dd>本页从创建交易起重放了池子的全部交易，结果与链上 cell 的数据和容量逐字节一致。</dd>`;
  }

  $("wallet").innerHTML = owner
    ? `已连接 <span class="mono">${escape(owner.address)}</span> <button id="disconnect">断开</button>`
    : `<span class="hint">未连接</span>`;
  document.getElementById("disconnect")?.addEventListener("click", () => {
    signer = owner = undefined;
    render();
  });

  ($("deposit") as HTMLButtonElement).disabled = !fresh || !($("backed-up") as HTMLInputElement).checked || !signer;
  $("backup").hidden = !fresh;
  if (fresh) ($("backup-note") as HTMLTextAreaElement).value = noteString(fresh);

  const list = $("notes");
  list.innerHTML = notes.length ? "" : `<p class="hint">这个浏览器里还没有票据。</p>`;
  notes.forEach((note, index) => {
    if (note === fresh) return;
    const status = statusOf(note);
    const item = document.createElement("div");
    item.className = "note";
    let text = "";
    const buttons: [string, () => void][] = [];
    switch (status.kind) {
      case "new":
        text = "未存入（没有找到它的意向）";
        buttons.push(["删除", () => remove(note)]);
        break;
      case "sent":
        text = `意向交易已发出 ${txLink(status.tx)}，等待上链`;
        break;
      case "waiting": {
        const mine = owner?.hash === status.args.owner_lock_hash;
        const capacity = BigInt(status.cell.output.capacity);
        text = `意向等待成交：${ckb(capacity)} CKB（小费 ${ckb(capacity - pool!.config.denomination)}），${txLink(status.cell.out_point.tx_hash)}。`;
        if (!status.args.reclaimable) text += ` <span class="bad">等待期写成了 ${status.args.reclaim_after}，无法撤回，只能等成交。</span>`;
        else if (status.left > 0n) text += ` 还要 ${status.left} 个区块才能撤回。`;
        else text += mine ? " 现在可以撤回。" : " 已过等待期，用创建它的钱包连接后可以撤回。";
        if (status.args.reclaimable && status.left === 0n && mine) buttons.push(["撤回", () => reclaim(note, status.cell, status.args)]);
        break;
      }
      case "deposited":
        text = `已在池中（第 ${status.leaf} 片叶子），可以取款`;
        if (note.withdrawal) {
          const known = aggregatorStatus.get(note.withdrawal.message.nullifier_hash);
          if (note.withdrawal.via === "aggregator") {
            text += `；取款已交给聚合者（${escape(known?.status ?? "状态未知")}${known?.tx ? ` ${txLink(known.tx)}` : ""}${known?.reason ? `：${escape(known.reason)}` : ""}）`;
          } else if (note.withdrawal.tx) {
            text += `；取款交易已自己发出 ${txLink(note.withdrawal.tx)}`;
          }
        }
        buttons.push(["取款", () => openWithdraw(note, status.leaf)]);
        break;
      case "withdrawn":
        text = `已取款${note.withdrawal?.tx ? ` ${txLink(note.withdrawal.tx)}` : ""}`;
        buttons.push(["删除", () => remove(note)]);
        break;
      case "reclaimed":
        text = `已撤回 ${txLink(note.reclaimed!)}`;
        buttons.push(["删除", () => remove(note)]);
        break;
    }
    item.innerHTML = `<div><span class="status">票据 ${index + 1}</span> <span class="mono">${short(note.commitment)}</span> · ${text}</div><div class="row"></div>`;
    const row = item.querySelector(".row")!;
    buttons.push(["显示票据", () => prompt("票据（请妥善保存，不要给别人）", noteString(note))]);
    for (const [label, run] of buttons) {
      const button = document.createElement("button");
      button.textContent = label;
      button.dataset.action = label;
      button.addEventListener("click", run);
      row.append(button);
    }
    list.append(item);
  });

  $("withdraw").hidden = !withdrawing;
  if (withdrawing) {
    $("withdraw-note").innerHTML = `票据 <span class="mono">${short(withdrawing.note.commitment)}</span>，第 ${withdrawing.leaf} 片叶子`;
    const ready = !!withdrawing.message;
    ($("to-aggregator") as HTMLButtonElement).disabled = !ready;
    ($("send-self") as HTMLButtonElement).disabled = !ready;
    $("message").hidden = !ready;
    if (ready) $("message").textContent = JSON.stringify(withdrawing.message, null, 2);
  }
}

function remove(note: Note) {
  if (!confirm("从这个浏览器里删除这张票据？如果它还有钱没取，删除后只能靠你的备份。")) return;
  notes = notes.filter((other) => other !== note);
  saveNotes(notes);
  render();
}

// ---- wallet

async function use(next: ccc.Signer) {
  await next.connect();
  const address = await next.getRecommendedAddressObj();
  if (address.prefix !== "ckt") throw new Error("这个钱包不在 CKB 测试网上");
  const lock = scriptJson(address.script);
  signer = next;
  owner = { address: address.toString(), lock, hash: scriptHash(lock) };
  log(`钱包已连接 <span class="mono">${escape(owner.address)}</span>`);
  render();
}

function listWallets() {
  const controller = new ccc.SignersController();
  void controller.refresh(client, (wallets) => {
    const row = $("wallets");
    row.innerHTML = "";
    for (const wallet of wallets) {
      for (const info of wallet.signers) {
        const button = document.createElement("button");
        button.textContent = `${wallet.name} · ${info.name}`;
        action(button, () => use(info.signer));
        row.append(button);
      }
    }
  }, { name: "CKB 混币池 v2（测试网）" });
}

// ---- deposit

async function deposit() {
  if (!fresh || !signer || !owner || !pool) throw new Error("先连接钱包并生成票据");
  const tipShannons = parseCkb(($("tip") as HTMLInputElement).value);
  const waitText = ($("wait") as HTMLInputElement).value.trim();
  if (!/^\d+$/.test(waitText)) throw new Error("等待的区块数必须是整数");
  const wait = BigInt(waitText);
  // A number of blocks, never an encoded `since`: no script checks this when
  // the cell is made, and a wait of 2^56 or more could never be reclaimed.
  if (wait < 1n || wait > LONGEST_WAIT) throw new Error("等待的区块数必须在 1 到 2^56−1 之间");
  if (wait > 200_000n && !confirm(`${wait} 个区块大约是 ${(Number(wait) * 8) / 86400} 天。没人成交的话要等这么久才能撤回，确定吗？`)) return;

  const lock = newIntentLock(fromHex(fresh.commitment), owner.lock, wait);
  // Read the lock back the way the scripts will, before any money moves.
  const args = intentArgs(lock.args);
  if (!args || !args.reclaimable || !args.canonical || args.reclaim_after !== wait || args.owner_lock_hash !== owner.hash || args.pool_type_hash !== pool.typeHash) {
    throw new Error("意向的 lock 参数自检没有通过，没有发送任何交易");
  }
  const capacity = intentCapacity(tipShannons);
  if (capacity < plainCellCapacity(lock)) throw new Error("意向 cell 的容量不够它自身占用的 145 CKB");

  const note = fresh;
  const tx = ccc.Transaction.from({
    outputs: [{ capacity, lock: { codeHash: lock.code_hash, hashType: lock.hash_type as ccc.HashTypeLike, args: lock.args } }],
    outputsData: ["0x"],
  });
  await tx.completeInputsByCapacity(signer);
  await tx.completeFeeBy(signer, FEE_RATE);
  const hash = await signer.sendTransaction(tx);
  update(note, { intent: { tx: hash, reclaimAfter: wait.toString() } });
  fresh = undefined;
  ($("backed-up") as HTMLInputElement).checked = false;
  log(`意向已创建 ${txLink(hash)}：${ckb(capacity)} CKB，${wait} 个区块后可撤回`);
}

// ---- reclaim

async function reclaim(note: Note, cell: LiveCell, args: IntentArgs) {
  try {
    if (!signer || !owner) throw new Error("先连接创建这个意向的钱包");
    if (owner.hash !== args.owner_lock_hash) throw new Error("这个意向不属于当前连接的钱包");
    const tx = reclaimDraft(cell, args.reclaim_after);
    // The intent lock wants an input under the owner's lock next to it.
    await tx.completeInputsAddOne(signer);
    await tx.completeFeeBy(signer, FEE_RATE);
    const hash = await signer.sendTransaction(tx);
    update(note, { reclaimed: hash });
    log(`撤回交易已发出 ${txLink(hash)}，${ckb(BigInt(cell.output.capacity))} CKB 回到你的钱包`);
  } catch (error) {
    const text = describe(error);
    log(/Immature/.test(text) ? "撤回失败: 等待期还没有过（节点报告 Immature），过几个区块再试" : `撤回失败: ${text}`, true);
  }
  render();
}

// ---- withdraw

function openWithdraw(note: Note, leaf: number) {
  withdrawing = { note, leaf };
  render();
  $("withdraw").scrollIntoView({ behavior: "smooth" });
}

let vkChecked = false;

/** The bundled circuit has to be the one this pool's verifying key came from. */
async function checkCircuit() {
  if (vkChecked) return;
  const vk = await (await fetch("/circuit/verification_key.json")).text();
  if (toHex(ckb_hash(vk_from_snarkjs(vk))) !== pool!.config.vk_hash) throw new Error("页面带的电路不是这个池子的电路");
  vkChecked = true;
}

async function prove(button: HTMLButtonElement) {
  if (!withdrawing || !pool) return;
  const state = withdrawing;
  state.message = undefined;
  const address = ($("recipient") as HTMLInputElement).value.trim();
  if (!address.startsWith("ckt1")) throw new Error("收款地址必须是 CKB 测试网地址（ckt1…）");
  const recipient = scriptJson((await ccc.Address.fromString(address, client)).script);
  const fee = parseCkb(($("fee") as HTMLInputElement).value);
  if (pool.config.denomination - fee < plainCellCapacity(recipient)) throw new Error("扣掉手续费后不够收款 cell 的最低容量");

  await refresh(); // prove against the newest root
  await checkCircuit();
  const { note, leaf } = state;
  const prepared = JSON.parse(
    pool.ledger.withdraw_input(leaf, fromHex(note.nullifier), fromHex(note.secret), fromHex(scriptHash(recipient)), fee),
  );
  button.textContent = "正在生成证明…";
  const started = performance.now();
  const { proof, publicSignals } = await snarkjs.groth16.fullProve(prepared.input, "/circuit/withdraw.wasm", "/circuit/withdraw_final.zkey");
  if (JSON.stringify(publicSignals) !== JSON.stringify(prepared.public)) throw new Error("证明的公开输入和预期不一致");
  const message: Message = {
    pool: pool.typeHash,
    root: prepared.root,
    nullifier_hash: prepared.nullifier_hash,
    recipient,
    fee: Number(fee),
    proof: toHex(proof_from_snarkjs(JSON.stringify(proof))),
  };
  // The pool's own scripts are the judge: a dry run of the batch holding only this withdrawal.
  const draft = batchDraft(pool, [], [message]);
  const { cycles } = await rpc.dryRun(txJson(draft));
  const least = feeFor(txWeight(txSize(draft), cycles), 1000n);
  state.message = message;
  update(note, { withdrawal: { message } });
  log(
    `证明已生成（${((performance.now() - started) / 1000).toFixed(1)} 秒），节点试运行通过，${cycles} cycles。` +
      (fee < least ? ` <span class="bad">手续费低于自己发送所需的最低矿工费 ${ckb(least)} CKB，只能交给聚合者。</span>` : ""),
  );
}

const aggregatorUrl = () => (localStorage.getItem(AGGREGATOR) ?? DEFAULT_AGGREGATOR).replace(/\/+$/, "");

async function toAggregator() {
  if (!withdrawing?.message) return;
  const { note, message } = withdrawing;
  const response = await fetch(`${aggregatorUrl()}/withdrawals`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(message),
  });
  const body: any = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error ?? `聚合者返回 ${response.status}`);
  update(note, { withdrawal: { message, via: "aggregator" } });
  aggregatorStatus.set(message.nullifier_hash, body);
  log(`取款消息已交给聚合者 ${escape(aggregatorUrl())}，等它打包。它不理你的话，随时可以“自己发送”。`);
  withdrawing = undefined;
}

/** The fallback of docs/intent-aggregation.md §4.3: a batch of this one withdrawal, unsigned. */
async function sendSelf() {
  if (!withdrawing?.message) return;
  const { note, message } = withdrawing;
  await refresh();
  const refused = refusal(pool!, message);
  if (refused) throw new Error(refused);
  let hash: string;
  try {
    hash = await rpc.send(txJson(batchDraft(pool!, [], [message])));
  } catch (error) {
    // Someone else's batch may have taken the pool cell first, or be in the
    // node's pool paying more: the message stays good while its root is known.
    throw new Error(`${error instanceof Error ? error.message : error}。如果是池子 cell 刚被别的交易用掉或占住，等一个区块再点一次；证明所依据的根还在历史里就不用重新生成。`);
  }
  update(note, { withdrawal: { message, via: "self", tx: hash } });
  log(`取款交易已发出 ${txLink(hash)}：${ckb(pool!.config.denomination - BigInt(message.fee))} CKB 给收款地址，手续费归矿工`);
  withdrawing = undefined;
}

// ---- aggregator

async function showAggregator() {
  ($("aggregator") as HTMLInputElement).value = aggregatorUrl();
  const box = $("aggregator-info");
  try {
    const info: any = await (await fetch(`${aggregatorUrl()}/info`)).json();
    if (info.error) throw new Error(info.error);
    box.innerHTML = `在线。地址 <span class="mono">${escape(info.address)}</span>；取款手续费不低于 ${ckb(BigInt(info.min_withdraw_fee))} CKB，意向小费不低于 ${ckb(BigInt(info.min_tip))} CKB，离可撤回不足 ${escape(info.min_blocks_left)} 个区块的意向它不碰。`;
  } catch (error) {
    box.innerHTML = `<span class="bad">连不上（${describe(error)}）。</span>存款意向会一直等到有聚合者成交或你撤回；取款可以“自己发送”。`;
  }
}

// ---- start

async function main() {
  await initWasm();
  $("fee").setAttribute("value", ckb(DEFAULT_WITHDRAW_FEE));
  render();

  action($("use-key") as HTMLButtonElement, async () => {
    const input = $("key") as HTMLInputElement;
    const key = input.value.trim();
    if (!/^(0x)?[0-9a-fA-F]{64}$/.test(key)) throw new Error("私钥应当是 64 位十六进制");
    input.value = "";
    await use(new ccc.SignerCkbPrivateKey(client, key.startsWith("0x") ? key : `0x${key}`));
  });
  $("new-note").addEventListener("click", () => {
    if (fresh) return;
    fresh = newNote();
    update(fresh, {});
    ($("backed-up") as HTMLInputElement).checked = false;
    render();
  });
  $("backed-up").addEventListener("change", render);
  $("copy-note").addEventListener("click", () => fresh && navigator.clipboard.writeText(noteString(fresh)));
  $("download-note").addEventListener("click", () => {
    if (!fresh) return;
    const link = document.createElement("a");
    link.href = URL.createObjectURL(new Blob([noteString(fresh) + "\n"], { type: "text/plain" }));
    link.download = `ckb-mixer-testnet-note-${fresh.commitment.slice(2, 10)}.txt`;
    link.click();
    URL.revokeObjectURL(link.href);
  });
  $("cancel-note").addEventListener("click", () => {
    notes = notes.filter((note) => note !== fresh);
    saveNotes(notes);
    fresh = undefined;
    render();
  });
  action($("deposit") as HTMLButtonElement, deposit);
  action($("import") as HTMLButtonElement, async () => {
    const input = $("import-note") as HTMLInputElement;
    const note = parseNote(input.value);
    if (notes.some((other) => other.commitment === note.commitment)) throw new Error("这张票据已经在这里了");
    update(note, {});
    input.value = "";
  });
  const proveButton = $("prove") as HTMLButtonElement;
  action(proveButton, () => prove(proveButton));
  action($("to-aggregator") as HTMLButtonElement, toAggregator);
  action($("send-self") as HTMLButtonElement, sendSelf);
  $("close-withdraw").addEventListener("click", () => {
    withdrawing = undefined;
    render();
  });
  $("save-aggregator").addEventListener("click", () => {
    localStorage.setItem(AGGREGATOR, ($("aggregator") as HTMLInputElement).value.trim());
    void showAggregator();
  });

  listWallets();
  void showAggregator();

  if ((await rpc.chain()) !== CHAIN) throw new Error("节点不是 CKB 测试网");
  for (;;) {
    try {
      await refresh();
    } catch (error) {
      log(`同步失败: ${describe(error)}`, true);
    }
    await new Promise((done) => setTimeout(done, 10_000));
  }
}

main().catch((error) => log(`启动失败: ${describe(error)}`, true));
