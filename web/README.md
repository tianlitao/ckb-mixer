# 池子 v2 的网页前端和聚合者 Worker（仅测试网）

> **仅限 CKB 测试网。** 池子绑定的可信设置是开发用的（单人生成，没有仪式），合约、这个页面和这个 Worker 都未经审计。不要指向主网，不要放任何有价值的东西。

这里有两样东西，都针对 README §五 里已经部署在测试网上的池子 v2：

- **前端**（`index.html`、`src/`）：纯静态页面，可以放到 Cloudflare Pages。连接钱包、建存款意向、在浏览器里生成取款证明、撤回意向、看池子和自己票据的状态。它直接连公共节点 `https://testnet.ckb.dev/rpc`（该节点允许跨域），没有自己的后端。
- **聚合者**（`worker/`）：一个 Cloudflare Worker。HTTP 接口收取款消息，定时任务把等待中的意向和收到的取款打成批次发出去，用它自己的密钥付矿工费、收小费和手续费。

两边共用 `shared/`（节点 RPC、池子重放、交易组装）和 `wasm/`。`wasm/` 是 `crates/mixer-wasm` 的构建产物：Poseidon、承诺树、nullifier 集合、池子状态和所有编码都是链上脚本用的那几个 crate 编出来的，没有用 JS 重写。

## 它怎么对待信任

- 页面和 Worker 都从池子的创建交易起**重放**池子的全部交易，结果必须和链上池子 cell 的数据、容量逐字节一致，否则拒绝工作。本地缓存的检查点只是加速，同样要过这一关。
- 票据的两个秘密在浏览器里生成，只存在这个浏览器的 localStorage 里，不发往任何地方。页面要求你确认已经备份才允许签名；丢了票据，那笔钱取不出来。
- 取款消息不含秘密，证明绑定了收款地址和手续费。聚合者不理你，可以点“自己发送（保底）”：只含这一笔取款的交易，不需要签名，也不需要事先有 CKB，手续费归矿工。
- 意向的等待期是**区块数**，不是编码过的 `since`。页面和 wasm 都拒绝 2^56 及以上的值（那样的 cell 建得出来，但永远撤不回），并在签名前把 lock 参数按脚本的读法再解析一遍核对。
- 页面带的电路文件（`public/circuit/`）必须是池子的那一份：取款前会把 `verification_key.json` 转成链上格式，哈希后和池子参数里的 `vk_hash` 比对。

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

页面在 <http://localhost:5173>，Worker 在 <http://localhost:8787>。`wrangler dev` 不会自己按 cron 触发，要手动触发一次定时任务：

```bash
curl 'http://localhost:8787/__scheduled?cron=*+*+*+*+*'
```

另有两个自检脚本：`npm run sync` 重放测试网池子并和链上比对；`npx tsx scripts/bench.ts` 测量聚合者自身计算的耗时。

### 聚合者的接口

| 接口 | 作用 |
| --- | --- |
| `POST /withdrawals` | 收一条取款消息（`docs/intent-aggregation.md` §4.2 的 JSON）。先做格式和状态检查，再让节点试运行“只含这一笔”的批次——证明由池子自己的脚本在节点上验证。 |
| `GET /withdrawals/<nullifier_hash>` | 这笔取款的状态：`queued` / `sent` / `settled` / `dropped`。 |
| `GET /info` | 聚合者的地址和它的要求（最低手续费、最低小费等）。 |
| 定时任务 | 上一个批次还没上链就等；否则挑意向（无 type、无 data、承诺合法、小费够、离可撤回还有至少 `MIN_BLOCKS_LEFT` 个区块）和取款（手续费高的优先），试运行拿到 cycles，按权重付矿工费，发送。批次不赚钱就不发。 |

参数在 `worker/wrangler.toml` 的 `[vars]` 里。

## 部署到 Cloudflare

以下命令需要你自己的 Cloudflare 账号，**我没有运行过**。

Worker：

```bash
npx wrangler login
```

```bash
npx wrangler kv namespace create STATE
```

把输出的 id 填进 `worker/wrangler.toml` 的 `kv_namespaces`，然后：

```bash
npx wrangler secret put AGGREGATOR_KEY --config worker/wrangler.toml
```

```bash
npm run worker:deploy
```

页面（把 Worker 的地址编进去；用户也可以在页面上改）：

```bash
VITE_AGGREGATOR_URL=https://ckb-mixer-aggregator.<你的子域>.workers.dev npm run build
```

```bash
npm run pages:deploy
```

也可以让 Pages 连 Git 仓库构建：根目录 `web`，构建命令 `npm run build`，输出目录 `dist`。`wasm/` 和 `public/circuit/` 都在仓库里，构建不需要 Rust。

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

- **没有部署到 Cloudflare。** Worker 只在本地 `wrangler dev`（workerd）里跑过；真实的 cron 触发、线上 KV、`wrangler secret`、Pages 部署都没有试过。
- **Worker 的 CPU 和子请求限额没有在线上量过。** 本机上聚合者自己的计算很小（从检查点恢复约 1 ms，含 1 / 4 笔取款的批次约 6 / 18 ms，另加一次 secp256k1 签名），时间主要花在等节点。但免费档每次调用 10 ms CPU、50 个子请求：带取款的批次大概率超出 CPU 限额，第一次同步要从创建交易重放（现在 19 笔交易，约 40 个请求），也贴近子请求上限。**应当按需要 Workers 付费档来准备**；之后的同步是增量的。nullifier 集合的证明开销随已取款数线性增长（每笔取款要把整个集合重建一遍），池子大了以后这里要改成持久化的树。
- **真实钱包没有测过。** 页面列出了 CCC 能发现的钱包（JoyID、UTXO Global、MetaMask 等），但端到端只用“测试网私钥”这条路走通——那是为演示加的，私钥只在页面内存里。钱包连接、它们各自的签名流程、非 secp256k1 锁作为意向所有者时的撤回，都没有验证。
- **聚合者之间的竞争没有测过。** 池子 cell 同一时刻只能被一笔交易花费。别人的批次先进了节点的交易池时，试运行仍会通过而发送被拒（试运行不检查 replace-by-fee）；Worker 的处理是下一轮重来，页面的保底路径是提示稍后再点一次。这两条都只是按设计写了，没有制造过冲突来验证。
- 批次里某笔取款被脚本拒绝时“逐笔试运行、剔除坏的”这条路径没有被触发过。
- Worker 没有限流和防滥用：任何人都能让它为一条取款消息做一次同步和一次节点试运行。
- KV 是最终一致的，两次定时任务理论上可能重叠；重叠时第二笔批次会因为池子 cell 已被花费而被节点拒绝，不会造成资金损失，但没有测过。
- 前端的 JS 包约 1.1 MB（钱包库 + snarkjs），没有做拆分；电路文件约 5 MB，首次取款时下载。
