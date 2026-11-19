# 池子 v2 的网页前端和聚合者 Worker（仅测试网）

> **仅限 CKB 测试网。** 池子绑定的可信设置是开发用的（单人生成，没有仪式），合约、这个页面和这个 Worker 都未经审计。不要指向主网，不要放任何有价值的东西。

这里有两样东西，都针对 README §五 里已经部署在测试网上的池子 v2：

- **前端**（`index.html`、`src/`、`src/i18n.ts`）：纯静态页面（中 / 英两种语言，右上角切换），部署在 Cloudflare Workers 静态资源上。登录用 CCC 官方的 `<ccc-connector>`（`@ckb-ccc/connector`）：右上角一个「连接钱包」，点开就是它自己的钱包选择、连接、断开界面，页面只负责收下连上的 signer。存款、建意向、在浏览器里生成取款证明、撤回、看池子和自己票据的状态都在这里；聚合者的地址在构建时编进去，页面上只读。它直接连公共节点 `https://testnet.ckb.dev/rpc`（该节点允许跨域），没有自己的后端。
- **聚合者**（`worker/`）：一个 Cloudflare Worker。HTTP 接口收取款消息，定时任务把等待中的意向和收到的取款打成批次发出去，用它自己的密钥付矿工费、收小费和手续费。

两边共用 `shared/`（节点 RPC、池子重放、交易组装）和 `wasm/`。`wasm/` 是 `crates/mixer-wasm` 的构建产物：Poseidon、承诺树、nullifier 集合、池子状态和所有编码都是链上脚本用的那几个 crate 编出来的，没有用 JS 重写。

## 它怎么对待信任

- 页面和 Worker 都从池子的创建交易起**重放**池子的全部交易，结果必须和链上池子 cell 的数据、容量逐字节一致，否则拒绝工作。本地缓存的检查点只是加速，同样要过这一关。
- 票据的两个秘密在浏览器里生成，只存在这个浏览器的 localStorage 里，不发往任何地方。页面要求你确认已经备份才允许签名；丢了票据，那笔钱取不出来。
- 取款消息不含秘密，证明绑定了收款地址和手续费。聚合者不理你，可以点“自己发送（保底）”：只含这一笔取款的交易，不需要签名，也不需要事先有 CKB，手续费归矿工。
- 意向的等待期是**区块数**，不是编码过的 `since`。页面和 wasm 都拒绝 2^56 及以上的值（那样的 cell 建得出来，但永远撤不回），并在签名前把 lock 参数按脚本的读法再解析一遍核对。
- 页面带的电路文件（`public/circuit/`）必须是池子的那一份：取款前会把 `verification_key.json` 转成链上格式，哈希后和池子参数里的 `vk_hash` 比对。

## 页面

一张纸、一种墨。字体是自托管的 Instrument Sans（正文）和 Martian Mono（数字、哈希），不连字体 CDN；颜色只有纸色、墨色、一根发丝线和池子自己的绿。登录和取款弹窗跟着页面的样式走（CCC 的组件用 CSS 变量换色），取款是一个 `dialog`，小屏上也能用。动画只有加载时标题的一次擦入，`prefers-reduced-motion` 下关掉。

右上角有中文 / English 两个按钮。所有文案都在 `src/i18n.ts` 里按 key 集中：静态文案由 `data-i18n`、`data-i18n-html`（允许 `<b>`、`<span>` 这类标记）和 `data-i18n-label`（无障碍标签）标在 `index.html` 上，JS 里一律用 `t(key, vars)` 取；动作日志存的是 key 和插值，所以切语言会把页面上已经说过的话一起改写。选择记在 `localStorage`（`ckbmix2.testnet.lang`），第一次访问按浏览器语言决定；CCC 的登录弹窗跟着切（`zh-Hans` / `en`），`<html lang>` 和标题也一起改。`shared/` 里链上逻辑抛出的错误还是中文，只出现在 `{error}` 插值里。

存款页只接受一种意向：面额 150 CKB、小费 0.1 CKB、等待 30 个区块后可撤回（`shared/config.ts` 的 `DEFAULT_TIP`、`DEFAULT_WAIT`），页面上只陈述、不提供输入框。签名之后页面里会留下回执：意向交易的链接、锁住的容量、等待的区块数，直到生成下一张票据才消失——钱包自己的确认提示关掉之后，这是唯一还看得见的凭据。生成票据后「票据已经保存到别处」默认勾选，票据已经显示在页面上。页面上没有私钥输入：存款和撤回都要连钱包。

## 本地运行

需要 Node 22+。

```bash
cd web
npm install
```

聚合者要一把**新的、只用于测试网**的私钥，并且账户里要有一点 CKB 垫矿工费（一个找零 cell 至少 61 CKB，放 100 就够）。写进 `worker/.dev.vars`（已在 .gitignore 里）：

```
AGGREGATOR_KEY=0x<64 位十六进制>
```

然后开两个终端：

```bash
npm run worker:dev
```

```bash
npm run dev
```

页面在 <http://localhost:5173>，Worker 在 <http://localhost:8787>。本地不加 `VITE_AGGREGATOR_URL` 时页面连的是 `http://localhost:8787`，正好是下面那个 `wrangler dev`。`wrangler dev` 不会自己按 cron 触发，要手动触发一次定时任务：

```bash
curl 'http://localhost:8787/__scheduled?cron=*+*+*+*+*'
```

另有两个自检脚本：`npm run sync` 重放测试网池子并和链上比对，同时报告耗时、请求数和 CPU（加 `-- --checkpoint <文件>` 会把检查点写成 JSON，可以拿去填 Worker 的 KV）；`npx tsx scripts/bench.ts` 测量聚合者自身计算的耗时。

### 聚合者的接口

| 接口 | 作用 |
| --- | --- |
| `POST /withdrawals` | 收一条取款消息（`docs/intent-aggregation.md` §4.2 的 JSON）。先做格式和状态检查，再让节点试运行“只含这一笔”的批次——证明由池子自己的脚本在节点上验证。 |
| `GET /withdrawals/<nullifier_hash>` | 这笔取款的状态：`queued` / `sent` / `settled` / `dropped`。 |
| `GET /info` | 聚合者的地址和它的要求（最低手续费、最低小费等）。 |
| 定时任务 | 上一个批次还没上链就等；否则挑意向（无 type、无 data、承诺合法、小费够、离可撤回还有至少 `MIN_BLOCKS_LEFT` 个区块）和取款（手续费高的优先），试运行拿到 cycles，按权重付矿工费，发送。批次不赚钱就不发。 |

参数在 `worker/wrangler.toml` 的 `[vars]` 里。

## 部署到 Cloudflare

已经部署在（2026-11-19，账户 `tianlitao3399@gmail.com`）：

- 页面：<https://ckb-mixer-web.tianlitao3399.workers.dev> —— Workers 静态资源（不是 Pages）
- 聚合者：<https://ckb-mixer-aggregator.tianlitao3399.workers.dev> —— 每分钟一次的 cron，KV 存检查点和取款队列

中国大陆的网络访问 `*.workers.dev` 需要代理（本机直连是 DNS 直接失败）。走代理时：

```bash
export https_proxy=http://127.0.0.1:7897 http_proxy=http://127.0.0.1:7897 all_proxy=socks5://127.0.0.1:7897
curl https://ckb-mixer-aggregator.tianlitao3399.workers.dev/info
```

### 为什么是 Workers 静态资源

Cloudflare 现在把 Pages 并进了 Workers：不带 `--force` 的 `wrangler pages project create` 会委派给 `wrangler deploy`，对一个还不存在的项目名只会报错（wrangler 4.164 实测）。静态资源这一套配置更简单，和聚合者一样是一条 `wrangler deploy`，所以两个都走 Workers，仓库里也只维护这一条路。

Pages 试过一次，已经撤掉：当时为了不让聚合者的账号子域出现在 URL 栏和动作日志里，加了一个 Pages Function 把 `/aggregator/*` 同源转发。但两个站点并存意味着 `dist/` 要维护两套构建（带 Function 的、纯静态的），容易部署错，所以项目、代码和 `pages:*` 脚本一起删了（`8de4114`）。现在页面上偶尔能看到的 `ckb-mixer-aggregator.tianlitao3399.workers.dev` 就是聚合者本身，这是这一版接受的取舍。

### 重新部署

聚合者（`worker/wrangler.toml`，绑定见下表）：

```bash
npm run worker:deploy
```

页面（`deploy/wrangler.toml`）。聚合者的地址是在**构建时**编进包里的，改了地址要重新构建：

```bash
VITE_AGGREGATOR_URL=https://ckb-mixer-aggregator.tianlitao3399.workers.dev npm run build
```

```bash
npm run web:deploy
```

### 一个全新的账号要准备什么

1. `npx wrangler login`。
2. `npx wrangler kv namespace create STATE`，把 id 填进 `worker/wrangler.toml` 的 `kv_namespaces`。
3. `npx wrangler secret put AGGREGATOR_KEY --config worker/wrangler.toml`，粘进一把**只用于测试网**的私钥。
4. 给这把私钥的地址转至少一个约 100 CKB 的找零 cell（垫矿工费；聚合者自己收不收费取决于池子参数，见下面的账目）。

聚合者的密钥只用来付矿工费和收小费，地址可以用 `GET /info` 看到（现在是 `ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqvztn2045czqj62790rxak6wqywj27a9ugrxl9qa`）。它不持有任何人的资金。

### 线上的配置（照着核对）

`worker/wrangler.toml`：

| 项 | 值 |
| --- | --- |
| cron | `* * * * *`（每分钟一次） |
| KV 绑定 | `STATE` = `84b84499f52f48a98bc8cf227c20b213` |
| secret | `AGGREGATOR_KEY` |
| `RPC_URL` | `https://testnet.ckb.dev/rpc` |
| `MIN_WITHDRAW_FEE` / `MIN_TIP` | `200000` / `1000000` shannons（0.002 / 0.01 CKB） |
| `MIN_BLOCKS_LEFT` | `10` |
| `MAX_WITHDRAWALS` / `MAX_INTENTS` | `4` / `16` |
| `LIST_EVERY` | `2`（见下） |

`deploy/wrangler.toml`：`name = "ckb-mixer-web"`，`assets = { directory = "../dist" }`，没有 Worker 脚本。静态资源请求不计费、不算 Worker 请求数。

### 免费档的限额决定了这些参数

从 Cloudflare 文档（2026-11-19 抓取）和线上实测得到的数字：

| 限额 | 免费档 | 这个项目怎么用 |
| --- | --- | --- |
| Worker CPU | **每次 HTTP 请求和每次 cron 触发 10 ms** | 空闲 tick 实测 **1–2 ms**；带取款的批次没测到 |
| Worker 请求 | 100,000 / 天 | 每分钟 1 次 cron = 1,440 / 天，加页面请求，够用 |
| 子请求 | 50 / 次调用 | 增量同步从检查点起 1 次请求，首次重放 4 次（分批 `get_transaction`） |
| KV 读 | 100,000 / 天 | 每次 tick 一定读 `pending`；有活才读检查点和队列里的条目，空闲时连池子都不碰 |
| **KV 写** | **1,000 / 天** | 空闲 tick 不写；有批次时写 `pending` + 每笔取款一条 |
| **KV 列表** | **1,000 / 天** | 一天 1,440 分钟 > 1,000，所以 `LIST_EVERY = 2`：每两分钟才列一次取款队列 |
| KV 删 | 1,000 / 天 | 只有 `pending` 结束时删一条 |
| cron 触发 | 5 个 / 账号 | 用了 1 个 |

`LIST_EVERY` 就是为免费档的 1,000 次列表限额设的；付费档可以改成 `1`（每分钟都看队列）。KV 对同一个键限 1 写/秒，本项目的写法不会踩到；但两分钟才看一次队列，意味着**一笔取款最多等约两分钟**才可能被打进批次。

KV 是最终一致的，两次 cron 理论上可能重叠；重叠时第二笔批次会因为池子 cell 已被花费而被节点拒绝，不会造成资金损失，但没有制造过这种情况来验证。

### 线上已经发生过的事（2026-11-19，UTC）

有人用**已部署的页面**（不是本地）对测试网池子跑了一遍。前三条是最早的一轮，只走了存款和保底取款：

| 时间 | 做了什么 | 交易 |
| --- | --- | --- |
| 03:19:24 | 页面建存款意向（150.1 CKB） | `0xc13b295f76c1dfb34586081b8839bc70f85568c21db9a82febfa1516632b3357` |
| 03:20:48 | 线上聚合者把它打进池子：池子 1950 → 2100 CKB，聚合者拿 0.1 CKB 小费 | `0x175fa29dd96d4786a22caf8ef4800b523029c4dce62fbcf2cd7ad7e871db4689` |
| 03:22:56 | 页面「自己发送（保底）」取款 149.99 CKB | `0x3fd0dcf8e5797a7c543857cd9e3bfbab30db3aaea5d13a464083198173fb46b5` |

之后又用页面走了一轮**存款 + 交给聚合者的取款**，两条都是线上 cron 发的批次：

| 时间 | 做了什么 | 交易 |
| --- | --- | --- |
| 06:14:04 | 线上聚合者打包一笔取款：池子 2250 → 2100 CKB，收款地址到账 149.99 CKB，聚合者收 0.01 CKB 手续费 | `0x15c71daa00720e4ba12aa56d59990880a677d577f1d34b1c47b4c2b07d6c2598` |
| 07:12:04 | 线上聚合者成交一笔存款意向：池子 2100 → 2250 CKB，聚合者拿 0.1 CKB 小费 | `0x138d95ba9288a091701e85a1b52721923f9be7321cc413605b4130c288002b78` |

第二组说明**真实 cron 触发、线上 KV、`wrangler secret`、公共节点**在线上都工作了，而且 `POST /withdrawals` → 排队 → 打包进批次这条取款路径已经在线上的聚合者里跑通（不再只是本地 `wrangler dev`）。页面保底那条路也一样跑通过。

截至最后一次同步（2026-11-19 07:30 UTC），池子重放 26 笔交易后是 **2250 CKB**、61 笔存款、59 笔取款、2 笔未取，与链上 cell 逐字节一致。

### 实测的 CPU（`wrangler tail`）

| 场景 | CPU |
| --- | --- |
| 首次从创建交易重放（部署后第一个 tick） | 72 ms |
| 空闲 tick，**旧版**（每分钟都重建池子） | 9–32 ms，多数 10–24 ms |
| 空闲 tick，新版（先找活，没活就不碰池子） | **1–2 ms** |
| `GET /info`（热） | 0–2 ms |
| 带存款的批次（03:20:48 那次） | 没抓到（当时的 tail 在批次前就断了） |
| 带取款的批次（06:14:04 那次） | 没抓到；它成功出块，但当时没有 tail 在跑 |

免费档每次 10 ms。新版空闲 tick 在限额以内；**带取款的批次大概率超**（本机不含网络、含 4 笔取款约 18 ms，另加签名），在线上一旦稳定超限会被终止。所以：要么上 Workers 付费档（CPU 上限可提到 5 分钟），要么把批次拆小、并把 nullifier 集合的证明改成持久化结构（现在每笔取款都要重建整个集合，成本随已取款数线性增长）。这次线上批次只带 1 个意向，没超，不代表取款批次不超。

## 重新生成 wasm

只有改了 `crates/mixer-wasm` 或它依赖的 crate 才需要：

```bash
cargo install wasm-bindgen-cli --version 0.2.130 --locked --root .tools
```

```bash
(cd crates/mixer-wasm && cargo build --release --target wasm32-unknown-unknown) && .tools/bin/wasm-bindgen --target web --out-dir web/wasm crates/mixer-wasm/target/wasm32-unknown-unknown/release/mixer_wasm.wasm
```

`public/circuit/` 里的三个文件是从 `circuits/build/` **复制**来的，是池子绑定的那一份可信设置。不要用重新 `make setup` 的结果替换它们：新的设置和链上的验证密钥对不上，证明会全部被拒。

## 在测试网上验证过什么（2026-11-19）

用本地 `vite` 页面（无头浏览器里操作）+ 本地 `wrangler dev` 的 Worker，针对测试网上的池子：

| 步骤 | 交易 |
| --- | --- |
| 页面建意向（150.1 CKB，5 个区块后可撤回） | `0xd14ea532…` |
| 页面建意向（150.1 CKB，300 个区块后可撤回） | `0x3a6bc1d2…` |
| Worker 定时任务成交后一个意向（前一个已过等待期，它没有碰） | `0x6b3fb3c4a67705635a9119627333fc345e380db7a3ebc02029db104a8cd8b294` |
| 页面撤回前一个意向，150.1 CKB 回到钱包 | `0xc3a72c99…` |
| 浏览器生成证明（约 1 秒），消息交给 Worker；Worker 打包取款，收款地址到账 **149.99 CKB** | `0xe889fe4d165b6fb8e761a4bfc9e1d846d0bded51945d6d3a82677c7f82a82bb0` |
| 再存一笔，Worker 成交 | `0x5038e2ffc7065268185332f7548ddde8d7dbfba5125cc215a7abe77a3020f519` |
| 浏览器生成证明后“自己发送（保底）”：不签名，手续费归矿工，收款地址再到账 149.99 CKB | `0x8c1efe8015145b6e96a9d8424d864221a453bed6d242495baadec5709d628bbb` |

另外验证过：未勾选“已备份”时不能签名；等待期填成编码过的 `since`（`9223372036854775838`）被拒绝，没有发出交易；聚合者三个批次的收支（小费 0.1 + 0.1、手续费 0.01，矿工费 13,055 / 89,077 / 13,055 shannons）。

## 没有验证的、已知的局限

- **线上的取款路径跑通过一次，但没有量 CPU、没有制造过竞争。** 06:14 那笔线上批次是 `POST /withdrawals` 收下一条取款、排队、再打包发出去的，交易上链、金额对得上；但那一轮没有开 `wrangler tail`，所以它花了多少 CPU 不知道，也还没有两次批次抢同一个池子 cell 的实测。
- **带取款的批次 CPU 没有在线上量过。** 本机上聚合者自己的计算很小（从检查点恢复约 1 ms，含 1 / 4 笔取款的批次约 6 / 18 ms，另加一次 secp256k1 签名），时间主要花在等节点。免费档每次调用 10 ms CPU、50 个子请求：空闲 tick 实测 1–2 ms，但带取款的批次大概率超出 CPU 限额。**应当按需要 Workers 付费档来准备**，或者把批次拆小。nullifier 集合的证明开销随已取款数线性增长（每笔取款要把整个集合重建一遍），池子大了以后这里要改成持久化的树。
- **钱包那条路只走通过一次。** 页面上没有私钥入口，存款和撤回都要连钱包，走的是 CCC 官方的 connector（JoyID、UTXO Global、MetaMask 等）。07:12 那笔线上存款是页面连钱包签名后发的，说明连接和签名能走通；但用的是哪个钱包没有记录，换一个钱包、换一个浏览器，它的弹出和签名流程是否一样没有试过，非 secp256k1 锁作为意向所有者时的撤回也没有验证。
- **聚合者之间的竞争没有测过。** 池子 cell 同一时刻只能被一笔交易花费。别人的批次先进了节点的交易池时，试运行仍会通过而发送被拒（试运行不检查 replace-by-fee）；Worker 的处理是下一轮重来，页面的保底路径是提示稍后再点一次。这两条都只是按设计写了，没有制造过冲突来验证。
- 批次里某笔取款被脚本拒绝时“逐笔试运行、剔除坏的”这条路径没有被触发过。
- Worker 没有限流和防滥用：任何人都能让它为一条取款消息做一次同步和一次节点试运行。
- KV 是最终一致的，两次定时任务理论上可能重叠；重叠时第二笔批次会因为池子 cell 已被花费而被节点拒绝，不会造成资金损失，但没有测过。
- 前端的主 JS 包约 1.3 MB（钱包库 + snarkjs），没有做拆分，加上 wasm 一共约 2.5 MB；电路文件约 4.7 MB（`withdraw.wasm` 1.7 MB + `withdraw_final.zkey` 3.0 MB），首次取款时下载。
