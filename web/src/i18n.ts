// Two languages, one page. Every string the page shows is here, so a switch
// changes all of it at once: the markup carries keys, the code asks for keys.
// The choice is remembered; a browser that prefers Chinese starts in Chinese.

import { ckb } from "../shared/hex";
import { DEFAULT_TIP, DEFAULT_WAIT } from "../shared/config";

export const LANGS = ["zh", "en"] as const;
export type Lang = (typeof LANGS)[number];

const STORE = "ckbmix2.testnet.lang";

const zh = {
  // masthead and hero
  "brand.home": "CKB 混币池 v2",
  "brand.testnet": "测试网 · 无署名 · 无仪式",
  "lang.label": "语言",
  "hero.a11y": "存款时留下一张票据，取款时什么都不留。",
  "hero.l1": "存款时留下",
  "hero.l2": "一张票据",
  "hero.l3": "取款时",
  "hero.l4": "什么都不留",
  "hero.note":
    "一笔存款就是一张<b>票据</b>：两张随机数，从不和钱一起出现。取款时池子只看见一张有效的证明，看不出钱从哪来、进过谁的手。",

  // the pool's ledger
  "readout.title": "池子此刻",
  "readout.deposits": "已存入",
  "readout.withdrawals": "已取出",
  "readout.open": "未取",
  "readout.pool": "池内",
  "readout.replaying": "正在从链上重放池子…",
  "readout.verified": "重放到块 {tip}，与链上 cell 逐字节一致",

  // tabs
  "tabs.label": "操作",
  "tab.deposit": "存款",
  "tab.notes": "票据",
  "tab.pool": "池子",
  "tab.activity": "动作",

  // the receipt after signing
  "receipt.title": "意向已签名",
  "receipt.hint": "聚合者会把它和别人的一起成交进池子；没人成交的话，等待期过后你可以撤回。",
  "receipt.line":
    "意向交易 {tx} 已交给节点，正在等它进入区块。{amount} CKB 锁在里面，{wait} 个区块后可撤回；聚合者成交后，票据在「票据」里会变成「在池中」。",

  // deposit
  "deposit.asPrefix": "以",
  "deposit.asSuffix": "的身份存款",
  "deposit.disconnect": "断开",
  "deposit.lede1":
    "存款先进一个只认你、也只认这个池子的<strong>意向 cell</strong>：面额 150 CKB 加上给聚合者的小费。聚合者把它和别人的一起成交进池子——你签一次名，之后不需要在线。",
  "deposit.lede2":
    "没有人来成交的话，等过 {wait} 个区块，同一笔钱可以原样撤回。取款不需要这个钱包，也不需要事先有 CKB。",
  "deposit.newNote": "生成票据",
  "deposit.import": "导入已有的票据",
  "deposit.backupTitle": "先备份，再签名",
  "deposit.backupBody": "取款只凭这张票据，它没有经过任何服务器。丢了它，这笔钱谁也取不出来——包括你。",
  "deposit.copy": "复制",
  "deposit.download": "下载为文件",
  "deposit.discard": "放弃这张",
  "deposit.terms": "每笔存款 = 面额 150 CKB + 小费 <span>{tip}</span> CKB，等待 <span>{wait}</span> 个区块后可撤回。",
  "deposit.backedUp": "票据已经保存到别处",
  "deposit.sign": "签名，创建意向",
  "deposit.importHint": "粘贴一张票据。它会留在这个浏览器的本地存储里。",
  "deposit.importButton": "导入",
  "deposit.cancel": "取消",

  // notes
  "notes.lede": "存在这个浏览器里的票据。连接创建它时的钱包，才能撤回或用作意向的所有者。",
  "notes.empty": "这个浏览器里还没有票据。生成一张，或者在存款页导入已有的票据。",
  "state.new": "未存入",
  "state.sent": "正在上链",
  "state.waiting": "等聚合者成交",
  "state.locked": "无法撤回",
  "state.deposited": "在池中",
  "state.withdrawn": "已取款",
  "state.reclaimed": "已撤回",
  "detail.new": "链上找不到它的意向——可能被放弃了，也可能还没有上链。",
  "detail.sent": "意向交易 {tx}，等它进入区块。",
  "detail.locked": "等待期字段写成了 {n}，超出 2^56，这个意向只能等人成交。",
  "detail.left": "还有 {n} 个区块可以撤回。意向 {tx}。",
  "detail.reclaimableMine": "已经过了等待期，现在可以撤回。",
  "detail.reclaimableOther": "已经过了等待期；用创建它的钱包连接后才能撤回。",
  "detail.leaf": "第 {n} 片叶子，可以取款。",
  "detail.withdrawAgg": "取款{state}{tx}{reason}。",
  "detail.withdrawSelf": "取款交易 {tx}。",
  "detail.withdrawnTx": "交易 {tx}。",
  "detail.withdrawn": "已从池中取走。",
  "detail.reclaimedTx": "交易 {tx}。",
  "agg.queued": "等聚合者打包",
  "agg.sent": "聚合者已发出",
  "agg.settled": "已上链",
  "agg.dropped": "被聚合者退回",
  "agg.unknown": "状态未知",
  "detail.reason": "：{reason}",
  "button.reclaim": "撤回",
  "button.withdraw": "取款",
  "button.note": "票据",
  "button.remove": "删除",

  // pool
  "pool.lede":
    "v2 的池子 cell 只有一个。每次批量结算由意向成交和取款一起花掉它，吐出新的状态——池子不记谁存了、谁取了，只记承诺和已经用过的 nullifier。",
  "pool.verifyTitle": "本页的独立性",
  "pool.verifyBody": "本页从创建交易起重放池子的每一笔交易，重放结果必须和链上 cell 的数据、容量逐字节一致，否则不动手。",
  "fact.denomination": "面额",
  "fact.capacity": "池内",
  "fact.root": "承诺根",
  "fact.nullifierRoot": "nullifier 根",
  "fact.cell": "池子 cell",
  "fact.type": "池子 type 脚本",
  "fact.waiting": "等待成交的意向",
  "fact.history": "保存的历史根",
  "fact.output": "输出 {n}",
  "fact.count": "{n} 个",

  // activity
  "activity.lede": "这个页面做过的事，最近的在前。交易都在浏览器里重新构造，没有转发给任何服务。",

  // colophon
  "colophon.body":
    "可信设置是本机生成的、一次性的，没有仪式，也没有审计。合约、页面和聚合者都只用于演示；不要往这里放任何有价值的东西，也不要把它指向主网。",
  "colophon.pool": "池",
  "colophon.denomination": "面额",
  "colophon.root": "根",
  "colophon.tip": "高",

  // withdrawal dialog
  "withdraw.close": "关闭",
  "withdraw.kicker": "取款",
  "withdraw.title": "票据",
  "withdraw.titleNote": "票据 {commitment}",
  "withdraw.sub": "第 {n} 片叶子。到手 {amount} CKB。",
  "withdraw.recipient": "收款地址",
  "withdraw.fee": "手续费",
  "withdraw.body":
    "收款地址到手 = 面额 − 手续费。证明在这个浏览器里生成，票据的秘密不离开这里；证明绑定了收款地址和手续费，聚合者改不了。",
  "withdraw.prove": "生成证明",
  "withdraw.toAggregator": "交给聚合者",
  "withdraw.self": "自己发送（保底）",
  "withdraw.explain":
    "“交给聚合者”会把它和别人的存取款打包，手续费归聚合者；“自己发送”是不带签名的单笔交易，手续费归矿工——聚合者不理你时用它。",
  "withdraw.raw": "取款消息",
  "withdraw.messageEmpty": "生成证明后出现在这里。",

  // wallet
  "wallet.connect": "连接钱包",
  "wallet.notTestnet": "这个钱包不在 CKB 测试网上",
  "wallet.connected": "钱包已连接 {address}（{from}）。",
  "wallet.failed": "钱包连接失败：{error}",
  "wallet.disconnected": "钱包已断开。",

  // actions and their logs
  "action.failed": "{label}失败：{error}",
  "deposit.needWallet": "先连接钱包并生成票据",
  "deposit.lockCheck": "意向的 lock 参数自检没有通过，没有发送任何交易",
  "deposit.tooSmall": "意向 cell 的容量不够它自身占用的 145 CKB",
  "log.intent": "意向已创建 {tx}：{amount} CKB，{wait} 个区块后可撤回。",
  "reclaim.needWallet": "先连接创建这个意向的钱包",
  "reclaim.notMine": "这个意向不属于当前连接的钱包",
  "reclaim.sent": "撤回交易已发出 {tx}，{amount} CKB 回到你的钱包。",
  "reclaim.immature": "撤回失败：等待期还没有过（节点报告 Immature），过几个区块再试。",
  "reclaim.failed": "撤回失败：{error}",
  "prove.wrongCircuit": "页面带的电路不是这个池子的电路",
  "prove.badAddress": "收款地址必须是 CKB 测试网地址（ckt1…）",
  "prove.tooSmall": "扣掉手续费后不够收款 cell 的最低容量",
  "prove.working": "正在生成证明…",
  "prove.mismatch": "证明的公开输入和预期不一致",
  "prove.done": "证明已生成（{seconds} 秒），节点试运行通过，{cycles} cycles。",
  "prove.lowFee": "手续费低于自己发送的最低矿工费 {fee} CKB，只能交给聚合者。",
  "aggregator.http": "聚合者返回 {status}",
  "aggregator.queued": "取款消息已交给聚合者，等它打包。它不理你的话，可以“自己发送”。",
  "self.busy":
    "{error}。如果池子 cell 刚被别的交易用掉或占住，等一个区块再试一次；证明所依据的根还在历史里就不用重新生成。",
  "self.sent": "取款交易已发出 {tx}：{amount} CKB 给收款地址，手续费归矿工。",
  "aggregator.online": "聚合者在线（{host}），取款手续费不低于 {fee} CKB，意向小费不低于 {tip} CKB。",
  "aggregator.offline": "聚合者暂时连不上。存款意向会等它回来或自己撤回；取款可以“自己发送”。",
  "note.copied": "票据已复制到剪贴板。",
  "note.copyPrompt": "票据（请妥善保存，不要给别人）",
  "note.copyFailed": "浏览器不允许复制，请手动选中。",
  "note.removeConfirm": "从这个浏览器里删除这张票据？如果它还有钱没取，删除后只能靠你的备份。",
  "note.imported": "票据已导入。",
  "note.duplicate": "这张票据已经在这里了",
  "chain.wrong": "节点不是 CKB 测试网",
  "sync.failed": "同步失败：{error}",
  "boot.failed": "启动失败：{error}",
  "title": "CKB 混币池 v2 · 测试网演示",
} as const;

export type Key = keyof typeof zh;

const en: Record<Key, string> = {
  "brand.home": "CKB mixer v2",
  "brand.testnet": "testnet · no names · no ceremony",
  "lang.label": "Language",
  "hero.a11y": "Deposit and keep a single note; withdraw and leave nothing behind.",
  "hero.l1": "Deposit and keep",
  "hero.l2": "a single note",
  "hero.l3": "Withdraw and leave",
  "hero.l4": "nothing behind",
  "hero.note":
    "A deposit is a <b>note</b>: two random numbers that never appear next to the money. At withdrawal the pool sees only a valid proof — not where the money came from, nor whose hands it passed through.",

  "readout.title": "The pool right now",
  "readout.deposits": "Deposited",
  "readout.withdrawals": "Withdrawn",
  "readout.open": "Unclaimed",
  "readout.pool": "In pool",
  "readout.replaying": "Replaying the pool from chain…",
  "readout.verified": "Replayed to block {tip}, byte for byte the cell on chain",

  "tabs.label": "Sections",
  "tab.deposit": "Deposit",
  "tab.notes": "Notes",
  "tab.pool": "Pool",
  "tab.activity": "Activity",

  "receipt.title": "Intent signed",
  "receipt.hint": "The aggregator settles it together with other people's; if nobody does, you can reclaim it once the wait is over.",
  "receipt.line":
    "Intent transaction {tx} handed to the node, waiting for a block. {amount} CKB locked inside, reclaimable after {wait} blocks. When a batch takes it, the note shows as “in the pool” under Notes.",

  "deposit.asPrefix": "Depositing as",
  "deposit.asSuffix": "",
  "deposit.disconnect": "Disconnect",
  "deposit.lede1":
    "A deposit goes into an <strong>intent cell</strong> that answers only to you, and only to this pool: 150 CKB face value plus a tip for the aggregator. The aggregator settles it alongside other people's — you sign once and can go offline.",
  "deposit.lede2":
    "If nobody settles it, the same money comes back unchanged after {wait} blocks. Withdrawing needs no wallet, and no CKB to start with.",
  "deposit.newNote": "Make a note",
  "deposit.import": "Import a note",
  "deposit.backupTitle": "Back it up, then sign",
  "deposit.backupBody": "Withdrawing takes only this note; it has never been through a server. Lose it and nobody can take the money out — including you.",
  "deposit.copy": "Copy",
  "deposit.download": "Download",
  "deposit.discard": "Discard",
  "deposit.terms": "Each deposit = 150 CKB face value + <span>{tip}</span> CKB tip, reclaimable after <span>{wait}</span> blocks.",
  "deposit.backedUp": "The note is saved somewhere else",
  "deposit.sign": "Sign and create the intent",
  "deposit.importHint": "Paste a note. It stays in this browser's local storage.",
  "deposit.importButton": "Import",
  "deposit.cancel": "Cancel",

  "notes.lede": "Notes kept in this browser. Connect the wallet that made one to reclaim it, or to act as its owner.",
  "notes.empty": "No notes in this browser yet. Make one, or import an existing note from the deposit section.",
  "state.new": "Not deposited",
  "state.sent": "Confirming",
  "state.waiting": "Waiting for a batch",
  "state.locked": "Can't be reclaimed",
  "state.deposited": "In the pool",
  "state.withdrawn": "Withdrawn",
  "state.reclaimed": "Reclaimed",
  "detail.new": "No intent for it on chain — it may have been discarded, or never made it into a block.",
  "detail.sent": "Intent transaction {tx}, waiting for a block.",
  "detail.locked": "The wait was written as {n}, past 2^56; this intent can only be settled.",
  "detail.left": "{n} blocks left to reclaim it. Intent {tx}.",
  "detail.reclaimableMine": "The wait is over; you can reclaim it now.",
  "detail.reclaimableOther": "The wait is over; connect the wallet that made it to reclaim.",
  "detail.leaf": "Leaf {n}, ready to withdraw.",
  "detail.withdrawAgg": "Withdrawal {state}{tx}{reason}.",
  "detail.withdrawSelf": "Withdrawal transaction {tx}.",
  "detail.withdrawnTx": "Transaction {tx}.",
  "detail.withdrawn": "Taken out of the pool.",
  "detail.reclaimedTx": "Transaction {tx}.",
  "agg.queued": "queued at the aggregator",
  "agg.sent": "sent by the aggregator",
  "agg.settled": "on chain",
  "agg.dropped": "returned by the aggregator",
  "agg.unknown": "in an unknown state",
  "detail.reason": " ({reason})",
  "button.reclaim": "Reclaim",
  "button.withdraw": "Withdraw",
  "button.note": "Note",
  "button.remove": "Delete",

  "pool.lede":
    "The v2 pool is a single cell. Each batch settlement spends it — intents settling and withdrawals together — and outputs a new state. The pool records neither who deposited nor who withdrew, only commitments and spent nullifiers.",
  "pool.verifyTitle": "How this page earns trust",
  "pool.verifyBody":
    "This page replays every pool transaction from the one that created the pool. It won't act unless the replay matches the on-chain cell's data and capacity byte for byte.",
  "fact.denomination": "Face value",
  "fact.capacity": "In pool",
  "fact.root": "Commitment root",
  "fact.nullifierRoot": "Nullifier root",
  "fact.cell": "Pool cell",
  "fact.type": "Pool type script",
  "fact.waiting": "Intents waiting",
  "fact.history": "Roots kept",
  "fact.output": "output {n}",
  "fact.count": "{n}",

  "activity.lede": "What this page has done, newest first. Every transaction was rebuilt in the browser; none was sent to a service.",

  "colophon.body":
    "The trusted setup was generated once, on this machine: no ceremony, no audit. The contract, the page and the aggregator are demonstration only. Don't put anything valuable here, and don't point it at mainnet.",
  "colophon.pool": "Pool",
  "colophon.denomination": "Face value",
  "colophon.root": "Root",
  "colophon.tip": "Height",

  "withdraw.close": "Close",
  "withdraw.kicker": "Withdraw",
  "withdraw.title": "Note",
  "withdraw.titleNote": "Note {commitment}",
  "withdraw.sub": "Leaf {n}. {amount} CKB arrives.",
  "withdraw.recipient": "Recipient",
  "withdraw.fee": "Fee",
  "withdraw.body":
    "What the recipient gets = face value − fee. The proof is made in this browser; the note's secrets never leave it, and the proof commits to the recipient and the fee, so the aggregator can't change either.",
  "withdraw.prove": "Generate proof",
  "withdraw.toAggregator": "Send to the aggregator",
  "withdraw.self": "Send it myself (fallback)",
  "withdraw.explain":
    "“Send to the aggregator” packs it with other deposits and withdrawals, and the fee goes to the aggregator; “send it myself” is a single unsigned transaction, and the fee goes to the miner — use it when the aggregator ignores you.",
  "withdraw.raw": "Withdrawal message",
  "withdraw.messageEmpty": "Appears here once a proof is generated.",

  "wallet.connect": "Connect wallet",
  "wallet.notTestnet": "This wallet is not on the CKB testnet",
  "wallet.connected": "Wallet connected {address} ({from}).",
  "wallet.failed": "Couldn't connect the wallet: {error}",
  "wallet.disconnected": "Wallet disconnected.",

  "action.failed": "{label} failed: {error}",
  "deposit.needWallet": "Connect a wallet and make a note first",
  "deposit.lockCheck": "The intent's lock arguments failed their self-check; nothing was sent",
  "deposit.tooSmall": "The intent cell can't cover the 145 CKB it needs for itself",
  "log.intent": "Intent created {tx}: {amount} CKB, reclaimable after {wait} blocks.",
  "reclaim.needWallet": "Connect the wallet that made this intent first",
  "reclaim.notMine": "This intent belongs to another wallet",
  "reclaim.sent": "Reclaim sent {tx}; {amount} CKB goes back to your wallet.",
  "reclaim.immature": "Reclaim failed: the wait isn't over yet (the node says Immature). Try again in a few blocks.",
  "reclaim.failed": "Reclaim failed: {error}",
  "prove.wrongCircuit": "The circuit shipped with this page is not this pool's circuit",
  "prove.badAddress": "The recipient must be a CKB testnet address (ckt1…)",
  "prove.tooSmall": "After the fee there isn't enough left for the recipient cell's minimum capacity",
  "prove.working": "Generating proof…",
  "prove.mismatch": "The proof's public inputs don't match what was expected",
  "prove.done": "Proof generated in {seconds} s; the node's dry run passed ({cycles} cycles).",
  "prove.lowFee": "The fee is below the minimum a self-sent transaction needs ({fee} CKB), so only the aggregator will take it.",
  "aggregator.http": "The aggregator answered {status}",
  "aggregator.queued": "Withdrawal handed to the aggregator, waiting for its batch. If it ignores you, use “send it myself”.",
  "self.busy":
    "{error}. If the pool cell was just spent or held by another transaction, wait a block and try again; as long as the root this proof was built on is still in history, you don't need to prove again.",
  "self.sent": "Withdrawal sent {tx}: {amount} CKB to the recipient, the fee to the miner.",
  "aggregator.online": "Aggregator online ({host}): withdrawals from {fee} CKB, intents from {tip} CKB.",
  "aggregator.offline": "The aggregator can't be reached right now. Intents wait for it or get reclaimed; withdrawals can be sent directly.",
  "note.copied": "Note copied to the clipboard.",
  "note.copyPrompt": "Note (keep it safe; don't give it to anyone)",
  "note.copyFailed": "The browser won't let this page copy; select the text by hand.",
  "note.removeConfirm": "Delete this note from this browser? If it still holds money, only your backup can get it out.",
  "note.imported": "Note imported.",
  "note.duplicate": "That note is already here",
  "chain.wrong": "The node is not the CKB testnet",
  "sync.failed": "Sync failed: {error}",
  "boot.failed": "Startup failed: {error}",
  "title": "CKB mixer v2 · testnet demo",
};

const tables: Record<Lang, Record<Key, string>> = { zh, en };

function initial(): Lang {
  const saved = localStorage.getItem(STORE);
  if (saved === "zh" || saved === "en") return saved;
  return navigator.language?.toLowerCase().startsWith("zh") ? "zh" : "en";
}

let current: Lang = initial();
const listeners = new Set<() => void>();

export const lang = (): Lang => current;
/** The tag the CCC connector ships translations for. */
export const connectorLocale = () => (current === "zh" ? "zh-Hans" : "en");

export function t(key: Key, vars?: Record<string, string | number>): string {
  const template = tables[current][key] ?? tables.en[key] ?? key;
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (match, name) => (name in vars ? String(vars[name]) : match));
}

/** Values every static string may refer to: the two numbers the page fixes. */
const constants = () => ({ tip: ckb(DEFAULT_TIP), wait: DEFAULT_WAIT.toString() });

/** Fill every keyed node in the document from the current language. */
export function applyStatic() {
  const vars = constants();
  document.documentElement.lang = current === "zh" ? "zh-CN" : "en";
  document.title = t("title");
  for (const node of document.querySelectorAll<HTMLElement>("[data-i18n]")) {
    node.textContent = t(node.dataset.i18n as Key, vars);
  }
  for (const node of document.querySelectorAll<HTMLElement>("[data-i18n-html]")) {
    node.innerHTML = t(node.dataset.i18nHtml as Key, vars);
  }
  for (const node of document.querySelectorAll<HTMLElement>("[data-i18n-label]")) {
    node.setAttribute("aria-label", t(node.dataset.i18nLabel as Key, vars));
  }
  for (const button of document.querySelectorAll<HTMLButtonElement>(".lang [data-lang]")) {
    button.setAttribute("aria-pressed", String(button.dataset.lang === current));
  }
}

export function onLangChange(fn: () => void) {
  listeners.add(fn);
}

export function setLang(next: Lang) {
  if (next === current) return;
  current = next;
  localStorage.setItem(STORE, next);
  applyStatic();
  for (const fn of listeners) fn();
}
