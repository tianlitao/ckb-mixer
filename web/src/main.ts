import { ccc } from "@ckb-ccc/ccc";
import "@ckb-ccc/connector";
import type { ConnectorConnection, ConnectorConnectionEvent, SelectClientEvent, WebComponentConnector } from "@ckb-ccc/connector";
import * as snarkjs from "snarkjs";
import initWasm, { ckb_hash, nullifier_hash, proof_from_snarkjs, vk_from_snarkjs } from "../wasm/mixer_wasm.js";
import { CHAIN, DEPS, DEFAULT_WITHDRAW_FEE, EXPLORER, FEE_RATE, LONGEST_WAIT, RPC_URL, type ScriptJson } from "../shared/config";
import { ckb, fromHex, parseCkb as readCkb, toHex } from "../shared/hex";
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
const TAB = "ckbmix2.testnet.tab";
const DEFAULT_AGGREGATOR: string = import.meta.env.VITE_AGGREGATOR_URL ?? "http://localhost:8787";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const escape = (text: unknown) =>
  String(text).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const short = (hash: string) => `${hash.slice(0, 8)}…${hash.slice(-6)}`;
const txLink = (hash: string) => `<a href="${EXPLORER}/transaction/${hash}" target="_blank" rel="noreferrer" class="mono">${short(hash)}</a>`;
const middle = (text: string, head = 14, tail = 8) => `${text.slice(0, head)}…${text.slice(-tail)}`;

/** CKB written by a person, in shannons; a field left half-typed counts as zero. */
function parseCkb(text: string): bigint {
  try {
    return readCkb(text);
  } catch {
    return 0n;
  }
}

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

// ---- the activity log

function log(html: string, bad = false) {
  const line = document.createElement("div");
  line.className = bad ? "bad" : "";
  line.innerHTML = `<time>${new Date().toLocaleTimeString("zh-CN", { hour12: false })}</time><span class="line-text">${html}</span>`;
  $("log").prepend(line);
}

const describe = (error: unknown) => escape(error instanceof Error ? error.message : String(error));

/** Run a button's action, with the button disabled meanwhile and failures logged. */
function action(button: HTMLButtonElement, run: () => Promise<void>) {
  button.addEventListener("click", async () => {
    const label = button.dataset.label ?? button.textContent ?? "";
    button.dataset.label = label;
    button.disabled = true;
    try {
      await run();
    } catch (error) {
      log(`${escape(label)}失败：${describe(error)}`, true);
    } finally {
      // The action may have replaced this control (a tab switch, a note that
      // left the list); only touch it if it is still the page's.
      if (button.isConnected) {
        button.textContent = label;
        button.disabled = false;
      }
      render();
    }
  });
}

const scriptJson = (script: ccc.Script): ScriptJson => ({ code_hash: script.codeHash, hash_type: script.hashType, args: script.args });

// ---- tabs

const TABS = ["deposit", "notes", "pool", "activity"] as const;
type TabName = (typeof TABS)[number];

function showTab(name: TabName, focus = false) {
  if (!TABS.includes(name)) name = "deposit";
  // Switching tabs, from a key or from a page load, is not a fragment change
  // to undo: the click must not leave a dead fragment behind.
  for (const tab of TABS) {
    $(`panel-${tab}`).hidden = tab !== name;
    const button = $(`tab-${tab}`);
    button.setAttribute("aria-selected", String(tab === name));
    button.tabIndex = tab === name ? 0 : -1;
    if (tab === name && focus) button.focus();
  }
  localStorage.setItem(TAB, name);
}

function wireTabs() {
  for (const tab of TABS) {
    const button = $(`tab-${tab}`) as HTMLButtonElement;
    button.addEventListener("click", () => showTab(tab));
    button.addEventListener("keydown", (event) => {
      const at = TABS.indexOf(tab);
      const next = event.key === "ArrowRight" ? TABS[(at + 1) % TABS.length] : event.key === "ArrowLeft" ? TABS[(at + TABS.length - 1) % TABS.length] : undefined;
      if (!next) return;
      event.preventDefault();
      showTab(next, true);
    });
  }
  const wanted = location.pathname === "/" || location.pathname === "/index.html" ? (localStorage.getItem(TAB) as TabName) : undefined;
  showTab(wanted || "deposit");
}

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
  const total = notes.filter((note) => note !== fresh).length;
  const open = pool ? Number(pool.ledger.leaf_count - pool.ledger.nullifier_count) : 0;

  // the ledger in the hero
  if (pool) {
    $("o-deposits").textContent = String(pool.ledger.leaf_count);
    $("o-withdrawals").textContent = String(pool.ledger.nullifier_count);
    $("o-open").textContent = String(open);
    $("o-capacity").textContent = ckb(pool.ledger.capacity);
    const verified = $("o-verified");
    verified.className = "readout-foot ok";
    verified.textContent = `重放到块 ${tip.toLocaleString("en-US")}，与链上 cell 逐字节一致`;
  }

  // the tabs carry how much waits behind them
  const notesTab = $("tab-notes");
  notesTab.innerHTML = `票据${total ? `<span class="count">${total}</span>` : ""}`;

  // footer facts
  if (pool) {
    $("f-type").textContent = middle(pool.typeHash, 10, 6);
    $("f-denomination").textContent = `${ckb(pool.config.denomination)} CKB`;
    $("f-root").textContent = middle(toHex(pool.ledger.current_root()), 10, 6);
    $("f-tip").textContent = tip.toLocaleString("en-US");
  }

  renderWallet();
  renderDeposit();
  renderNotes();
  renderPool();
  renderWithdraw();
}

function renderWallet() {
  const button = document.querySelector(".connect") as HTMLButtonElement | null;
  if (button) {
    const label = button.querySelector(".label")!;
    label.textContent = owner ? middle(owner.address, 8, 6) : "连接钱包";
    // The connector draws its own connected view when it is opened again.
    const open = (connector?.style.display ?? "none") !== "none";
    button.dataset.state = open ? "open" : "closed";
    button.setAttribute("aria-expanded", String(open));
  }
  const manage = document.querySelector(".connect-manage") as HTMLButtonElement | null;
  if (manage) manage.hidden = !owner;
  const badge = $("wallet-badge");
  if (!badge) return;
  badge.hidden = !owner;
  const who = badge.querySelector(".mono");
  if (who) who.textContent = owner ? middle(owner.address, 18, 6) : "";
}

function renderDeposit() {
  const button = $("deposit") as HTMLButtonElement;
  button.disabled = !fresh || !($("backed-up") as HTMLInputElement).checked || !signer;
  $("backup").hidden = !fresh;
  $("new-note").toggleAttribute("disabled", !!fresh);
  if (fresh) ($("backup-note") as HTMLElement).textContent = noteString(fresh);
}

function renderNotes() {
  const list = $("notes-list");
  list.innerHTML = "";
  const shown = notes.filter((note) => note !== fresh);
  if (!shown.length) {
    list.innerHTML = `<li class="empty">这个浏览器里还没有票据。生成一张，或者在存款页导入已有的票据。</li>`;
    return;
  }
  shown.forEach((note, index) => {
    const status = statusOf(note);
    const item = document.createElement("li");
    item.className = "note";
    item.style.setProperty("--n", String(Math.min(index, 8)));
    const amount = amountOf(note, status);
    const kind = status.kind === "waiting" && !status.args.reclaimable ? "locked" : status.kind;
    let state = "";
    let detail = "";
    const buttons: [string, () => void][] = [];
    switch (status.kind) {
      case "new":
        state = "未存入";
        detail = "链上找不到它的意向——可能被放弃了，也可能还没有上链。";
        break;
      case "sent":
        state = "正在上链";
        detail = `意向交易 ${txLink(status.tx)}，等它进入区块。`;
        break;
      case "waiting": {
        const mine = owner?.hash === status.args.owner_lock_hash;
        state = "等聚合者成交";
        if (!status.args.reclaimable) {
          state = "无法撤回";
          detail = `等待期字段写成了 ${status.args.reclaim_after}，超出 2^56，这个意向只能等人成交。`;
        } else if (status.left > 0n) {
          detail = `还有 ${status.left} 个区块可以撤回。意向 ${txLink(status.cell.out_point.tx_hash)}。`;
        } else {
          detail = mine ? "已经过了等待期，现在可以撤回。" : "已经过了等待期；用创建它的钱包连接后才能撤回。";
        }
        if (status.args.reclaimable && status.left === 0n && mine) buttons.push(["撤回", () => reclaim(note, status.cell, status.args)]);
        break;
      }
      case "deposited": {
        state = "在池中";
        detail = `第 ${status.leaf} 片叶子，可以取款。`;
        const sent = note.withdrawal;
        if (sent) {
          const known = aggregatorStatus.get(sent.message.nullifier_hash);
          if (sent.via === "aggregator") {
            const says: Record<string, string> = { queued: "等聚合者打包", sent: "聚合者已发出", settled: "已上链", dropped: "被聚合者退回" };
            detail += `取款${says[known?.status ?? ""] ?? "状态未知"}${known?.tx ? ` ${txLink(known.tx)}` : ""}${known?.reason ? `：${escape(known.reason)}` : ""}。`;
          } else if (sent.tx) {
            detail += `取款交易 ${txLink(sent.tx)}。`;
          }
        }
        buttons.push(["取款", () => openWithdraw(note, status.leaf)]);
        break;
      }
      case "withdrawn":
        state = "已取款";
        detail = note.withdrawal?.tx ? `交易 ${txLink(note.withdrawal.tx)}。` : "已从池中取走。";
        break;
      case "reclaimed":
        state = "已撤回";
        detail = `交易 ${txLink(note.reclaimed!)}。`;
        break;
    }
    buttons.push(["票据", () => showNote(note)]);
    if (status.kind !== "deposited" && status.kind !== "waiting") buttons.push(["删除", () => remove(note)]);

    item.innerHTML = `
      <span class="note-index">${String(index + 1).padStart(2, "0")}</span>
      <div class="note-body">
        <div class="note-head">
          <span class="note-state" data-kind="${kind}"></span>
          ${amount ? `<span class="note-amount">${amount}</span>` : ""}
          <span class="mono muted">${short(note.commitment)}</span>
        </div>
        <p class="note-detail"></p>
      </div>
      <div class="note-actions"></div>`;
    (item.querySelector(".note-state") as HTMLElement).textContent = state;
    (item.querySelector(".note-detail") as HTMLElement).innerHTML = detail;
    const row = item.querySelector(".note-actions") as HTMLElement;
    for (const [label, run] of buttons) {
      const button = document.createElement("button");
      button.textContent = label;
      button.addEventListener("click", run);
      row.append(button);
    }
    list.append(item);
  });
}

function amountOf(note: Note, status: Status): string {
  if (!pool) return "";
  if (status.kind === "waiting") return `${ckb(BigInt(status.cell.output.capacity))} CKB`;
  if (status.kind === "deposited" || status.kind === "withdrawn") return `${ckb(pool.config.denomination)} CKB`;
  return "";
}

function renderPool() {
  if (!pool) return;
  const cell = pool.cell.out_point;
  const waiting = intents.filter(({ cell }) => !cell.output.type && cell.data === "0x").length;
  $("pool-facts").innerHTML = `
    <div><dt>面额</dt><dd>${ckb(pool.config.denomination)} CKB</dd></div>
    <div><dt>池内</dt><dd>${ckb(pool.ledger.capacity)} CKB</dd></div>
    <div><dt>承诺根</dt><dd class="mono">${toHex(pool.ledger.current_root())}</dd></div>
    <div><dt>nullifier 根</dt><dd class="mono">${toHex(pool.ledger.nullifier_root())}</dd></div>
    <div><dt>池子 cell</dt><dd class="mono">${txLink(cell.tx_hash)}<br />输出 ${Number(cell.index)}</dd></div>
    <div><dt>池子 type 脚本</dt><dd class="mono">${txLink(DEPS.poolType.tx_hash)}<br />输出 ${Number(DEPS.poolType.index)}</dd></div>
    <div><dt>等待成交的意向</dt><dd>${waiting} 个</dd></div>
    <div><dt>保存的历史根</dt><dd>${pool.config.root_history} 个</dd></div>`;
}

function renderWithdraw() {
  const dialog = $("withdraw") as HTMLDialogElement;
  if (!withdrawing) {
    if (dialog.open) dialog.close();
    return;
  }
  if (!dialog.open) dialog.showModal();
  $("withdraw-title").textContent = `票据 ${short(withdrawing.note.commitment)}`;
  $("withdraw-note").textContent = `第 ${withdrawing.leaf} 片叶子。${pool ? `到手 ${ckb(pool.config.denomination - parseCkb(($("fee") as HTMLInputElement).value))} CKB。` : ""}`;
  const ready = !!withdrawing.message;
  ($("to-aggregator") as HTMLButtonElement).disabled = !ready;
  ($("send-self") as HTMLButtonElement).disabled = !ready;
  $("message").textContent = ready ? JSON.stringify(withdrawing.message, null, 2) : "生成证明后出现在这里。";
}

function showNote(note: Note) {
  void navigator.clipboard?.writeText(noteString(note)).then(
    () => log("票据已复制到剪贴板。"),
    () => window.prompt("票据（请妥善保存，不要给别人）", noteString(note)),
  );
}

function remove(note: Note) {
  if (!confirm("从这个浏览器里删除这张票据？如果它还有钱没取，删除后只能靠你的备份。")) return;
  notes = notes.filter((other) => other !== note);
  saveNotes(notes);
  render();
}

// ---- wallet: one button in the corner, CCC's own connector UI behind it
//
// Follows @ckb-ccc/connector's documented integration: the <ccc-connector>
// element borrows a Client the application owns, opens over the whole page,
// and reports the connection through a `connection` event carrying an
// Owner<ConnectorConnection>. The Owner is what holds the wallet open: move it
// into application state, dispose the previous one, and dispose `close` only
// hides what the connector draws.

let connector: WebComponentConnector | undefined;
let connectionOwner: ccc.Owner<ConnectorConnection> | undefined;

function wireConnect() {
  const mount = $("connect-mount");

  // "Manage" in the connector's connected view leads here; it belongs to the
  // app, not to the connector.
  const manage = document.createElement("button");
  manage.type = "button";
  manage.className = "connect-manage";
  manage.textContent = "私钥";
  manage.title = "用一把一次性的测试网私钥（没有钱包扩展时）";
  manage.hidden = true;
  manage.addEventListener("click", () => {
    connector && (connector.style.display = "none");
    const details = document.querySelector(".demo-key") as HTMLDetailsElement | null;
    if (details) details.open = true;
    showTab("deposit");
    $("demo-key").focus();
    renderWallet();
  });
  mount.append(manage);

  const button = document.createElement("button");
  button.className = "connect";
  button.type = "button";
  button.setAttribute("aria-haspopup", "dialog");
  button.setAttribute("aria-expanded", "false");
  button.innerHTML = `<span class="dot" aria-hidden="true"></span><span class="label">连接钱包</span>`;
  button.addEventListener("click", () => openConnect());
  mount.append(button);

  if (!connector) {
    const element = document.createElement("ccc-connector") as WebComponentConnector;
    element.client = client;
    element.hideKhie = true;
    element.locale = "zh-Hans";
    element.style.display = "none";
    element.style.zIndex = "100";
    element.style.cssText += [
      "color: #14161a",
      "--background: #fffefb",
      "--divider: #e3dfd6",
      "--btn-primary: #f3f1ec",
      "--btn-primary-hover: #eae6de",
      "--btn-secondary: #e9e6df",
      "--btn-secondary-hover: #dedad1",
      "--btn-color: #14161a",
      "--btn-color-hover: #063d2e",
      "--icon-primary: #14161a",
      "--icon-secondary: #5b636d",
      "--tip-color: #5b636d",
      "--tip-color-hover: #0a5c46",
    ].join(";");

    element.addEventListener("close", () => {
      element.style.display = "none";
      button.dataset.state = "closed";
      button.setAttribute("aria-expanded", "false");
      button.focus();
    });
    // Network and fee-rate choices are requests; this app only ever runs on
    // the testnet it was built for, so it takes the one it is given back.
    element.addEventListener("select-client", (event) => {
      element.client = (event as SelectClientEvent).client;
    });
    element.addEventListener("connection", (event) => {
      event.stopPropagation();
      const next = (event as ConnectorConnectionEvent).connectionOwner?.map((connection) => connection);
      const previous = connectionOwner;
      connectionOwner = next;
      void previous?.dispose();
      void onConnection(next?.value);
    });
    document.body.append(element);
    connector = element;
  }
}

async function onConnection(connection: ConnectorConnection | undefined) {
  if (!connection) {
    signer = owner = undefined;
    render();
    return;
  }
  await adopt(connection.signerInfo.signer, connection.wallet.name);
  // A wallet that is not on the testnet leaves nothing connected.
  if (!owner) {
    void connectionOwner?.dispose();
    connectionOwner = undefined;
    connector?.disconnect();
  }
}

/** One path for every signer, whether a wallet picked it or the demo key made it. */
async function adopt(next: ccc.Signer, from: string) {
  try {
    const address = await next.getRecommendedAddressObj();
    if (address.prefix !== "ckt") throw new Error("这个钱包不在 CKB 测试网上");
    const lock = scriptJson(address.script);
    signer = next;
    owner = { address: address.toString(), lock, hash: scriptHash(lock) };
    log(`钱包已连接 <span class="mono">${escape(short(owner.address))}</span>（${escape(from)}）。`);
  } catch (error) {
    log(`钱包连接失败：${describe(error)}`, true);
    signer = owner = undefined;
  }
  render();
}

/** A one-off private key, for browsers without a wallet extension. Never persisted. */
async function useDemoKey() {
  const input = $("demo-key") as HTMLInputElement;
  const key = input.value.trim();
  if (!/^(0x)?[0-9a-fA-F]{64}$/.test(key)) throw new Error("私钥应当是 64 位十六进制");
  input.value = "";
  await adopt(new ccc.SignerCkbPrivateKey(client, key.startsWith("0x") ? key : `0x${key}`), "测试网私钥（演示）");
}

function openConnect() {
  if (!connector) return;
  connector.style.display = "";
  renderWallet();
  // The connector draws itself; its own footer ("Manage") is the app's cue to
  // show the demo-key entry it would otherwise never offer.
}

async function disconnect() {
  await connectionOwner?.dispose();
  connectionOwner = undefined;
  signer = owner = undefined;
  connector?.disconnect();
  log("钱包已断开。");
  render();
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
  log(`意向已创建 ${txLink(hash)}：${ckb(capacity)} CKB，${wait} 个区块后可撤回。`);
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
    log(`撤回交易已发出 ${txLink(hash)}，${ckb(BigInt(cell.output.capacity))} CKB 回到你的钱包。`);
  } catch (error) {
    const text = describe(error);
    log(/Immature/.test(text) ? "撤回失败：等待期还没有过（节点报告 Immature），过几个区块再试。" : `撤回失败：${text}`, true);
  }
  render();
}

// ---- withdraw

function openWithdraw(note: Note, leaf: number) {
  withdrawing = { note, leaf };
  render();
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
      (fee < least ? ` <span class="bad">手续费低于自己发送的最低矿工费 ${ckb(least)} CKB，只能交给聚合者。</span>` : ""),
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
  log(`取款消息已交给聚合者，等它打包。它不理你的话，可以“自己发送”。`);
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
    throw new Error(`${error instanceof Error ? error.message : error}。如果池子 cell 刚被别的交易用掉或占住，等一个区块再试一次；证明所依据的根还在历史里就不用重新生成。`);
  }
  update(note, { withdrawal: { message, via: "self", tx: hash } });
  log(`取款交易已发出 ${txLink(hash)}：${ckb(pool!.config.denomination - BigInt(message.fee))} CKB 给收款地址，手续费归矿工。`);
  withdrawing = undefined;
}

// ---- the aggregator that this page hands messages to (not editable here)

let aggregatorShown = false;

async function showAggregator() {
  if (aggregatorShown) return;
  aggregatorShown = true;
  try {
    const info: any = await (await fetch(`${aggregatorUrl()}/info`)).json();
    if (info.error) throw new Error(info.error);
    log(
      `聚合者在线（${escape(new URL(aggregatorUrl()).host)}），取款手续费不低于 ${ckb(BigInt(info.min_withdraw_fee))} CKB，意向小费不低于 ${ckb(BigInt(info.min_tip))} CKB。`,
    );
  } catch {
    log("聚合者暂时连不上。存款意向会等它回来或自己撤回；取款可以“自己发送”。");
  }
}

// ---- start

async function main() {
  // The app's own "manage" page is a fragment; every other one is left over
  // from something that should not have written it.
  if (location.hash && !location.hash.startsWith("#manage")) history.replaceState(null, "", location.pathname + location.search);
  await initWasm();
  wireTabs();
  wireConnect();
  render();

  // The connector's connected view links to the app's own page for anything
  // it does not manage: here, the one-off testnet key.
  if (/^#manage/.test(location.hash)) {
    const details = document.querySelector(".demo-key") as HTMLDetailsElement | null;
    if (details) details.open = true;
    showTab("deposit");
  }

  ($("fee") as HTMLInputElement).value = ckb(DEFAULT_WITHDRAW_FEE);
  ($("fee") as HTMLInputElement).addEventListener("input", renderWithdraw);

  $("new-note").addEventListener("click", () => {
    if (fresh) return;
    fresh = newNote();
    update(fresh, {});
    ($("backed-up") as HTMLInputElement).checked = false;
    render();
    $("backup")?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  });
  $("backed-up").addEventListener("change", render);
  $("copy-note").addEventListener("click", () => {
    if (!fresh) return;
    void navigator.clipboard.writeText(noteString(fresh)).then(() => log("票据已复制。"), () => log("浏览器不允许复制，请手动选中。", true));
  });
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
  $("open-import").addEventListener("click", () => {
    $("import").hidden = false;
    ($("import-note") as HTMLInputElement).focus();
  });
  $("close-import").addEventListener("click", () => {
    $("import").hidden = true;
  });
  action($("import-note-button") as HTMLButtonElement, async () => {
    const input = $("import-note") as HTMLInputElement;
    const note = parseNote(input.value);
    if (notes.some((other) => other.commitment === note.commitment)) throw new Error("这张票据已经在这里了");
    update(note, {});
    input.value = "";
    $("import").hidden = true;
    log("票据已导入。");
  });
  $("disconnect").addEventListener("click", () => void disconnect());
  action($("use-demo-key") as HTMLButtonElement, useDemoKey);
  const proveButton = $("prove") as HTMLButtonElement;
  action(proveButton, () => prove(proveButton));
  action($("to-aggregator") as HTMLButtonElement, toAggregator);
  action($("send-self") as HTMLButtonElement, sendSelf);
  ($("withdraw") as HTMLDialogElement).addEventListener("close", () => {
    withdrawing = undefined;
  });
  action($("close-withdraw") as HTMLButtonElement, async () => {
    ($("withdraw") as HTMLDialogElement).close();
    render();
  });

  void showAggregator();

  if ((await rpc.chain()) !== CHAIN) throw new Error("节点不是 CKB 测试网");
  for (;;) {
    try {
      await refresh();
    } catch (error) {
      log(`同步失败：${describe(error)}`, true);
    }
    await new Promise((done) => setTimeout(done, 10_000));
  }
}

main().catch((error) => log(`启动失败：${describe(error)}`, true));
