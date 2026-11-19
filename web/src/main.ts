import { ccc } from "@ckb-ccc/ccc";
import "@ckb-ccc/connector";
import type { ConnectorConnection, ConnectorConnectionEvent, SelectClientEvent, WebComponentConnector } from "@ckb-ccc/connector";
import * as snarkjs from "snarkjs";
import initWasm, { ckb_hash, nullifier_hash, proof_from_snarkjs, vk_from_snarkjs } from "../wasm/mixer_wasm.js";
import { CHAIN, DEPS, DEFAULT_TIP, DEFAULT_WAIT, DEFAULT_WITHDRAW_FEE, EXPLORER, FEE_RATE, RPC_URL, type ScriptJson } from "../shared/config";
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
import { applyStatic, connectorLocale, lang, onLangChange, setLang, t, type Key, type Lang } from "./i18n";
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
//
// Lines are kept as a key and its values, not as text: switching languages
// rewrites what the page has said so far, in the language now chosen.

interface LogEntry {
  key: Key;
  vars?: Record<string, string | number>;
  bad?: boolean;
  at: number;
}

const logEntries: LogEntry[] = [];

function log(key: Key, vars?: Record<string, string | number>, bad = false) {
  logEntries.unshift({ key, vars, bad, at: Date.now() });
  renderLog();
}

function renderLog() {
  const time = (at: number) => new Date(at).toLocaleTimeString(lang() === "zh" ? "zh-CN" : "en-GB", { hour12: false });
  $("log").innerHTML = logEntries
    .map((entry) => `<div class="${entry.bad ? "bad" : ""}"><time>${time(entry.at)}</time><span class="line-text">${t(entry.key, entry.vars)}</span></div>`)
    .join("");
}

const describe = (error: unknown) => escape(error instanceof Error ? error.message : String(error));

/** Run a button's action, with the button disabled meanwhile and failures logged. */
function action(button: HTMLButtonElement, run: () => Promise<void>) {
  button.addEventListener("click", async () => {
    const key = button.dataset.i18n as Key | undefined;
    const label = key ? t(key) : (button.dataset.label ?? button.textContent ?? "");
    button.disabled = true;
    try {
      await run();
    } catch (error) {
      log("action.failed", { label: escape(label), error: describe(error) }, true);
    } finally {
      // The action may have replaced this control (a tab switch, a note that
      // left the list); only touch it if it is still the page's.
      if (button.isConnected) {
        button.textContent = key ? t(key) : label;
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

// The toggle is a segmented control: two buttons, the current one pressed.
function wireLang() {
  for (const button of document.querySelectorAll<HTMLButtonElement>(".lang [data-lang]")) {
    button.addEventListener("click", () => setLang(button.dataset.lang as Lang));
  }
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
    verified.textContent = t("readout.verified", { tip: tip.toLocaleString("en-US") });
  }

  // the tabs carry how much waits behind them
  const notesTab = $("tab-notes");
  notesTab.innerHTML = `${t("tab.notes")}${total ? `<span class="count">${total}</span>` : ""}`;

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
    label.textContent = owner ? middle(owner.address, 8, 6) : t("wallet.connect");
    // The connector draws its own connected view when it is opened again.
    const open = (connector?.style.display ?? "none") !== "none";
    button.dataset.state = open ? "open" : "closed";
    button.setAttribute("aria-expanded", String(open));
  }
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
    list.innerHTML = `<li class="empty">${t("notes.empty")}</li>`;
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
        state = t("state.new");
        detail = t("detail.new");
        break;
      case "sent":
        state = t("state.sent");
        detail = t("detail.sent", { tx: txLink(status.tx) });
        break;
      case "waiting": {
        const mine = owner?.hash === status.args.owner_lock_hash;
        state = t("state.waiting");
        if (!status.args.reclaimable) {
          state = t("state.locked");
          detail = t("detail.locked", { n: status.args.reclaim_after.toString() });
        } else if (status.left > 0n) {
          detail = t("detail.left", { n: status.left.toString(), tx: txLink(status.cell.out_point.tx_hash) });
        } else {
          detail = t(mine ? "detail.reclaimableMine" : "detail.reclaimableOther");
        }
        if (status.args.reclaimable && status.left === 0n && mine) buttons.push([t("button.reclaim"), () => reclaim(note, status.cell, status.args)]);
        break;
      }
      case "deposited": {
        state = t("state.deposited");
        detail = t("detail.leaf", { n: String(status.leaf) });
        const sent = note.withdrawal;
        if (sent) {
          const known = aggregatorStatus.get(sent.message.nullifier_hash);
          if (sent.via === "aggregator") {
            const says: Record<string, Key> = { queued: "agg.queued", sent: "agg.sent", settled: "agg.settled", dropped: "agg.dropped" };
            detail += t("detail.withdrawAgg", {
              state: t(says[known?.status ?? ""] ?? "agg.unknown"),
              tx: known?.tx ? ` ${txLink(known.tx)}` : "",
              reason: known?.reason ? t("detail.reason", { reason: escape(known.reason) }) : "",
            });
          } else if (sent.tx) {
            detail += t("detail.withdrawSelf", { tx: txLink(sent.tx) });
          }
        }
        buttons.push([t("button.withdraw"), () => openWithdraw(note, status.leaf)]);
        break;
      }
      case "withdrawn":
        state = t("state.withdrawn");
        detail = note.withdrawal?.tx ? t("detail.withdrawnTx", { tx: txLink(note.withdrawal.tx) }) : t("detail.withdrawn");
        break;
      case "reclaimed":
        state = t("state.reclaimed");
        detail = t("detail.reclaimedTx", { tx: txLink(note.reclaimed!) });
        break;
    }
    buttons.push([t("button.note"), () => showNote(note)]);
    if (status.kind !== "deposited" && status.kind !== "waiting") buttons.push([t("button.remove"), () => remove(note)]);

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
    <div><dt>${t("fact.denomination")}</dt><dd>${ckb(pool.config.denomination)} CKB</dd></div>
    <div><dt>${t("fact.capacity")}</dt><dd>${ckb(pool.ledger.capacity)} CKB</dd></div>
    <div><dt>${t("fact.root")}</dt><dd class="mono">${toHex(pool.ledger.current_root())}</dd></div>
    <div><dt>${t("fact.nullifierRoot")}</dt><dd class="mono">${toHex(pool.ledger.nullifier_root())}</dd></div>
    <div><dt>${t("fact.cell")}</dt><dd class="mono">${txLink(cell.tx_hash)}<br />${t("fact.output", { n: Number(cell.index) })}</dd></div>
    <div><dt>${t("fact.type")}</dt><dd class="mono">${txLink(DEPS.poolType.tx_hash)}<br />${t("fact.output", { n: Number(DEPS.poolType.index) })}</dd></div>
    <div><dt>${t("fact.waiting")}</dt><dd>${t("fact.count", { n: waiting })}</dd></div>
    <div><dt>${t("fact.history")}</dt><dd>${t("fact.count", { n: pool.config.root_history })}</dd></div>`;
}

function renderWithdraw() {
  const dialog = $("withdraw") as HTMLDialogElement;
  if (!withdrawing) {
    if (dialog.open) dialog.close();
    return;
  }
  if (!dialog.open) dialog.showModal();
  $("withdraw-title").textContent = t("withdraw.titleNote", { commitment: short(withdrawing.note.commitment) });
  $("withdraw-note").textContent = pool
    ? t("withdraw.sub", {
        n: String(withdrawing.leaf),
        amount: ckb(pool.config.denomination - parseCkb(($("fee") as HTMLInputElement).value)),
      })
    : "";

  const ready = !!withdrawing.message;
  ($("to-aggregator") as HTMLButtonElement).disabled = !ready;
  ($("send-self") as HTMLButtonElement).disabled = !ready;
  $("message").textContent = ready ? JSON.stringify(withdrawing.message, null, 2) : t("withdraw.messageEmpty");
}

function showNote(note: Note) {
  void navigator.clipboard?.writeText(noteString(note)).then(
    () => log("note.copied"),
    () => window.prompt(t("note.copyPrompt"), noteString(note)),
  );
}

function remove(note: Note) {
  if (!confirm(t("note.removeConfirm"))) return;
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

  const button = document.createElement("button");
  button.className = "connect";
  button.type = "button";
  button.setAttribute("aria-haspopup", "dialog");
  button.setAttribute("aria-expanded", "false");
  button.innerHTML = `<span class="dot" aria-hidden="true"></span><span class="label">${t("wallet.connect")}</span>`;
  button.addEventListener("click", () => openConnect());
  mount.append(button);

  if (!connector) {
    const element = document.createElement("ccc-connector") as WebComponentConnector;
    element.client = client;
    element.hideKhie = true;
    element.locale = connectorLocale();
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
    if (address.prefix !== "ckt") throw new Error(t("wallet.notTestnet"));
    const lock = scriptJson(address.script);
    signer = next;
    owner = { address: address.toString(), lock, hash: scriptHash(lock) };
    log("wallet.connected", { address: `<span class="mono">${escape(short(owner.address))}</span>`, from: escape(from) });
  } catch (error) {
    log("wallet.failed", { error: describe(error) }, true);
    signer = owner = undefined;
  }
  render();
}

function openConnect() {
  if (!connector) return;
  connector.style.display = "";
  renderWallet();
}

async function disconnect() {
  await connectionOwner?.dispose();
  connectionOwner = undefined;
  signer = owner = undefined;
  connector?.disconnect();
  log("wallet.disconnected");
  render();
}

// ---- deposit

async function deposit() {
  if (!fresh || !signer || !owner || !pool) throw new Error(t("deposit.needWallet"));
  // The demo page has one offer: the tip the aggregator asks for, and a wait
  // long enough for a batch or two but short enough to reclaim the same day.
  // (A number of blocks, never an encoded `since`: a wait of 2^56 or more
  // could never be reclaimed.)
  const tipShannons = DEFAULT_TIP;
  const wait = DEFAULT_WAIT;

  const lock = newIntentLock(fromHex(fresh.commitment), owner.lock, wait);
  // Read the lock back the way the scripts will, before any money moves.
  const args = intentArgs(lock.args);
  if (!args || !args.reclaimable || !args.canonical || args.reclaim_after !== wait || args.owner_lock_hash !== owner.hash || args.pool_type_hash !== pool.typeHash) {
    throw new Error(t("deposit.lockCheck"));
  }
  const capacity = intentCapacity(tipShannons);
  if (capacity < plainCellCapacity(lock)) throw new Error(t("deposit.tooSmall"));

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
  // Signing leaves the page without the wallet's own confirmation, so the
  // receipt stays on the page until another note is started. Waiting is the
  // honest state: the transaction is accepted by the node, not yet in a block.
  const receipt = $("receipt");
  receipt.hidden = false;
  receiptVars = { tx: txLink(hash), amount: ckb(capacity), wait: wait.toString() };
  renderReceipt();
  log("log.intent", { tx: txLink(hash), amount: ckb(capacity), wait: wait.toString() });
  render();
  receipt.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

/** The receipt stays until the next note; its values are kept so a language switch rewrites it. */
let receiptVars: Record<string, string> | undefined;
function renderReceipt() {
  const line = $("receipt-line");
  if (line && receiptVars) line.innerHTML = t("receipt.line", receiptVars);
}

// ---- reclaim

async function reclaim(note: Note, cell: LiveCell, args: IntentArgs) {
  try {
    if (!signer || !owner) throw new Error(t("reclaim.needWallet"));
    if (owner.hash !== args.owner_lock_hash) throw new Error(t("reclaim.notMine"));
    const tx = reclaimDraft(cell, args.reclaim_after);
    // The intent lock wants an input under the owner's lock next to it.
    await tx.completeInputsAddOne(signer);
    await tx.completeFeeBy(signer, FEE_RATE);
    const hash = await signer.sendTransaction(tx);
    update(note, { reclaimed: hash });
    log("reclaim.sent", { tx: txLink(hash), amount: ckb(BigInt(cell.output.capacity)) });
  } catch (error) {
    const text = describe(error);
    log(/Immature/.test(text) ? "reclaim.immature" : "reclaim.failed", { error: text }, true);
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
  if (toHex(ckb_hash(vk_from_snarkjs(vk))) !== pool!.config.vk_hash) throw new Error(t("prove.wrongCircuit"));
  vkChecked = true;
}

async function prove(button: HTMLButtonElement) {
  if (!withdrawing || !pool) return;
  const state = withdrawing;
  state.message = undefined;
  const address = ($("recipient") as HTMLInputElement).value.trim();
  if (!address.startsWith("ckt1")) throw new Error(t("prove.badAddress"));
  const recipient = scriptJson((await ccc.Address.fromString(address, client)).script);
  const fee = parseCkb(($("fee") as HTMLInputElement).value);
  if (pool.config.denomination - fee < plainCellCapacity(recipient)) throw new Error(t("prove.tooSmall"));

  await refresh(); // prove against the newest root
  await checkCircuit();
  const { note, leaf } = state;
  const prepared = JSON.parse(
    pool.ledger.withdraw_input(leaf, fromHex(note.nullifier), fromHex(note.secret), fromHex(scriptHash(recipient)), fee),
  );
  button.textContent = t("prove.working");
  const started = performance.now();
  const { proof, publicSignals } = await snarkjs.groth16.fullProve(prepared.input, "/circuit/withdraw.wasm", "/circuit/withdraw_final.zkey");
  if (JSON.stringify(publicSignals) !== JSON.stringify(prepared.public)) throw new Error(t("prove.mismatch"));
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
    "prove.done",
    { seconds: ((performance.now() - started) / 1000).toFixed(1), cycles: String(cycles) },
    false,
  );
  if (fee < least) log("prove.lowFee", { fee: ckb(least) }, true);
}

const aggregatorUrl = () => (localStorage.getItem(AGGREGATOR) ?? DEFAULT_AGGREGATOR).replace(/\/+$/, "");

/** The aggregator's host as the page shows it: same origin when it is proxied. */
function aggregatorHost(): string {
  try {
    return new URL(aggregatorUrl(), location.origin).host;
  } catch {
    return aggregatorUrl();
  }
}

async function toAggregator() {
  if (!withdrawing?.message) return;
  const { note, message } = withdrawing;
  const response = await fetch(`${aggregatorUrl()}/withdrawals`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(message),
  });
  const body: any = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error ?? t("aggregator.http", { status: response.status }));
  update(note, { withdrawal: { message, via: "aggregator" } });
  aggregatorStatus.set(message.nullifier_hash, body);
  log("aggregator.queued");
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
    throw new Error(t("self.busy", { error: escape(error instanceof Error ? error.message : String(error)) }));
  }
  update(note, { withdrawal: { message, via: "self", tx: hash } });
  log("self.sent", { tx: txLink(hash), amount: ckb(pool!.config.denomination - BigInt(message.fee)) });
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
    log("aggregator.online", {
      host: escape(aggregatorHost()),
      fee: ckb(BigInt(info.min_withdraw_fee)),
      tip: ckb(BigInt(info.min_tip)),
    });
  } catch {
    log("aggregator.offline");
  }
}

// ---- start

async function main() {
  // Every fragment is left over from something that should not have written it.
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  applyStatic();
  wireLang();
  await initWasm();
  wireTabs();
  wireConnect();
  onLangChange(() => {
    render();
    renderLog();
    renderReceipt();
    if (connector) connector.locale = connectorLocale();
    if (($("withdraw") as HTMLDialogElement).open) renderWithdraw();
  });
  render();

  ($("fee") as HTMLInputElement).value = ckb(DEFAULT_WITHDRAW_FEE);
  ($("fee") as HTMLInputElement).addEventListener("input", renderWithdraw);

  $("new-note").addEventListener("click", () => {
    if (fresh) return;
    $("receipt").hidden = true;
    receiptVars = undefined;
    fresh = newNote();
    update(fresh, {});
    // The point of the checkbox is that the person saw the note. A note that
    // was just shown on this page has been seen; they can clear it if their
    // backup is only in a tab they do not trust.
    ($("backed-up") as HTMLInputElement).checked = true;
    render();
    $("backup")?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  });
  $("backed-up").addEventListener("change", render);
  $("copy-note").addEventListener("click", () => {
    if (!fresh) return;
    void navigator.clipboard.writeText(noteString(fresh)).then(() => log("note.copied"), () => log("note.copyFailed", {}, true));
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
    if (notes.some((other) => other.commitment === note.commitment)) throw new Error(t("note.duplicate"));
    update(note, {});
    input.value = "";
    $("import").hidden = true;
    log("note.imported");
  });
  $("disconnect").addEventListener("click", () => void disconnect());
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

  if ((await rpc.chain()) !== CHAIN) throw new Error(t("chain.wrong"));
  for (;;) {
    try {
      await refresh();
    } catch (error) {
      log("sync.failed", { error: describe(error) }, true);
    }
    await new Promise((done) => setTimeout(done, 10_000));
  }
}

main().catch((error) => log("boot.failed", { error: describe(error) }, true));
