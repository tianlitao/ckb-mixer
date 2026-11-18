# CKB 上的 Tornado Cash 式混币器原型（PoC）

一个可以在 CKB 上跑通「存款 → 零知识证明 → 匿名取款」的最小可行实现，
所有关键组件都跑在真实的 CKB-VM 里：先用 `ckb-testtool` 作链逐笔验证并测出 cycles，
再在 **CKB 公共测试网**上完整跑了一遍（部署、建池、5 笔存款、5 笔取款，见第五节）。

核心结论：

* **可行，验证成本不高。** 一次取款（Groth16/BN254 验证 + 256 层 nullifier 集合）在默认构建下实测
  **约 69.4M–69.5M cycles**（多次运行波动约 0.2%），约占 CKB 单块上限（3.5G cycles）的
  **2%**；一次存款约 8.4M cycles（0.24%）。
* **测试网上成立。** 上链的是体积更小的 size 构建，取款约 **1.025 亿 cycles**（超过节点默认的
  `max_tx_verify_cycles` = 70M），都在 12–23 秒内被打包；链上 cycles 与本地测量只差 0.2%–0.5%。
  首尾相接的一笔存款和两笔取款连发后进了同一个区块。
* **最大的固定成本是部署，不是验证。** CKB 上 1 字节占 1 CKB：脚本和验证密钥要锁 14.1 万 CKB
  （size 构建，测试网实付）到 29.7 万 CKB（默认构建），所有池子共享一份；每个池子另外永久占用 1,819 CKB。
* 链上不需要存储 Merkle 树，只需要 1,640 字节的池子状态：30 个历史根、20 层 frontier、
  nullifier 集合的根。
* 取款交易**不需要任何签名**，只有一个输入（池子 cell），收款地址和手续费被电路绑定，
  因此没有中继者也无法被抢跑改收款人。
* **池子 v2 的链上部分已实现**（存款意向 cell + 批量结算 + Type ID，
  [docs/intent-aggregation.md](docs/intent-aggregation.md)）：存款人把钱放进意向 cell 就可以离开，
  任何人都能把一批意向和取款拼成一笔交易。本地实测每多一笔取款 69.6M cycles、每多一笔存款 0.40M，
  一个块最多装 50 笔取款。v2 只有本地测试，聚合者工具没有写，也没有上过测试网；
  本文第一到第七节描述的都是 v1。

## 一、设计要点

### cell 布局

一个池子就是一个 cell：

| 部分 | 内容 |
| --- | --- |
| `type` | 池子 type script（本仓库 `contracts/pool-type`），args = 面额(8) ‖ 验证密钥 cell 的 data hash(32) ‖ 池子 lock 的 code hash(32) ‖ lock 的 hash type(1) |
| `lock` | 池子 lock（`contracts/pool-lock`），args = 池子 type script 的 hash；自身逻辑是"这个 cell 必须带池子 type script"，因此**没有私钥**，也没有人能把池子锁死 |
| `data` | `PoolState`，1640 字节：`next_index`(4) ‖ `root_pos`(4) ‖ 最近 30 个 commitment 树根(32×30) ‖ frontier(32×20) ‖ nullifier 集合根(32) |

池子 cell 永远不会被销毁（type script 要求输入输出都是 1 个），也不会被换成别人的 lock。

### 存款

交易把池子 cell 作为输入，在 `WitnessArgs.input_type` 里放：

```
0x00 ‖ commitment(32) × n
```

type script 用 frontier 把 n 个叶子追加进 depth-20 的 Poseidon 树（只有最后一个叶子会一直
hash 到根，批量 n 个叶子约 n + 20 次 Poseidon），把新根写进环状历史，并要求输出 cell 的
data 逐字节等于新状态、capacity 恰好等于 `输入 + n × 面额`。

由于一次存款交易只记录**一个**新根，取款证明在之后 30 笔存款交易内都有效。

### 取款

交易**只有池子 cell 一个输入、没有签名**。`input_type` 里放：

```
0x01 ‖ fee(8, LE) ‖ root(32) ‖ nullifierHash(32) ‖ Groth16 证明(128) ‖ nullifier 集合证明(32·k+32)
```

type script 依次做：

1. 根在最近的 30 个根里（`UnknownRoot`），且 root/nullifierHash 是规范域元素
   （非规范别名 `x + r` 会被拒绝，否则同一个 nullifier 能被花无限次）；
2. 用 256 层稀疏 Merkle 树把 `nullifierHash` 标记为已花，要求"标记前"的路径等于当前
   nullifier 集合根（否则 `NullifierSpent`）；
3. 收款 cell 必须是**池子 cell 之后的那个输出**，没有 type script、没有 data、capacity 恰好
   等于 `面额 - fee`（固定位置写死，两个池子在同一笔交易里不可能共用同一个收款输出）；
4. 用链上验证密钥 cell（cell dep，按 data hash 定位）验证 Groth16 证明，公开输入为
   `[root, nullifierHash, recipient, fee]`，其中 `recipient` 是收款 lock 脚本 hash 去掉最后一个
   字节（32 字节的哈希放不进域，去掉末字节后必小于 2^248，见 `field::recipient_field`）。

### 电路（`circuits/withdraw.circom`）

* `commitment = Poseidon(nullifier, secret)`，是 depth-20 Merkle 树的某个叶子；
* `nullifierHash = Poseidon(nullifier, index)`，`index` 由路径 bit 在电路内推出。
  这样**同一个 commitment 存两次会有两个不同的 nullifierHash**，两笔都能取——
  CKB 上没有 Tornado 在 Solidity 里那种"commitment 已用"表，这件事改由电路解决；
* `recipient`、`fee` 通过平方约束进入证明，任何人无法用别人的证明改收款人或抬高手续费。

规模：5,322 约束、4 个公开输入，链上验证约 67.4M cycles（占取款的 97%）。

### nullifier 集合（`crates/mixer-core/src/smt.rs`）

自研的 256 层稀疏 Merkle 树：每个高度一次 blake2b(height ‖ left ‖ right)，空子树哈希为 0，
除此之外没有别的捷径节点。证明 = 32 字节 bitmap（第 h 位表示第 h 层兄弟非空）+ 非空兄弟
（每个 32 字节）。取款时 256 层约 **1.96M cycles**。

不使用 `sparse-merkle-tree` crate 的原因见第七节。

## 二、目录结构

```
circuits/                  电路、dev-only 可信设置脚本、snarkjs 出证明脚本
  withdraw.circom
  scripts/{setup.sh,prove.mjs,gen-poseidon-constants.mjs}
crates/mixer-core/         链上/链下共用的核心逻辑（no_std）
  src/{field,poseidon,merkle,smt,state,action,error}.rs
crates/mixer-core-v2/      v2 的核心逻辑：可变长根历史、批量 action、args 布局、Type ID
crates/prover/             主机侧出证明、格式转换
contracts/                 链上脚本（独立 workspace，目标 riscv64imac-unknown-none-elf）
  pool-type/               池子 type script（v1，测试网上部署的就是它）
  pool-lock/               池子 lock script（v1、v2 共用）
  pool-type-v2/            池子 type script v2：意向存款、批量取款、Type ID
  intent-lock/             存款意向 cell 的 lock script
scripts/build-contracts.sh 构建 4 种变体（default / size / bext / profile）
tests/                     主机侧测试（ckb-testtool 模拟链）
  src/{snark.rs,harness.rs,harness_v2.rs}  证明格式转换、测试脚手架
  tests/{e2e.rs,cycles.rs,smt_crate_forgery.rs}  v1
  tests/{v2.rs,cycles_v2.rs}                     v2
tools/testnet/             测试网工具（独立 workspace，直接讲 JSON-RPC）
  src/rpc.rs               节点与索引器的 RPC
  src/wallet.rs            资金账户：选币、找零、secp256k1 签名、地址
  src/pool.rs              部署/建池/存款/取款交易，以及从链上交易重放出池子状态
  src/proof.rs             出证明并在链下预验证
  src/bin/mixer.rs         命令行入口
docs/intent-aggregation.md 池子 v2 的设计、安全性分析和实测：存款意向 cell + 聚合
```

## 三、如何运行

依赖：Rust（含 `riscv64imac-unknown-none-elf` target）、Node.js ≥ 18、bash。

```bash
rustup target add riscv64imac-unknown-none-elf
```

首次准备工具与电路（circom 装在项目内 `.tools/`，约 2 分钟）：

```bash
make tools       # cargo install circom v2.2.3 到 ./.tools
make setup       # npm ci + 编译电路 + dev-only 可信设置
```

> `make setup` 生成的是**单人可信设置**，toxic waste 就在本机。只能用来看 demo，
> 绝不能守护真实资产。

构建链上脚本并跑测试：

```bash
make contracts   # 编译四个脚本到 contracts/target/...
make test        # cargo test --workspace：v1 29 单元 + 26 端到端/对抗性，v2 13 单元 + 24 端到端/对抗性，
                 # 另有 2 个 SMT 复现和 1 个 v1 cycles 报告
make bench       # 构建全部 4 种变体并打印 cycles / 体积 / 阶段分解（v1 和 v2；v2 的报告要跑几分钟）
```

只跑某个测试文件：

```bash
cargo test -p mixer-tests --test e2e -- --nocapture
cargo test -p mixer-tests --test v2 -- --nocapture
cargo test -p mixer-tests --test cycles -- --nocapture
cargo test -p mixer-tests --test cycles_v2 -- --nocapture --ignored
```

测试会调用 `node circuits/scripts/prove.mjs` 现场生成真实证明（每笔约 1–2 秒），
`MIXER_CONTRACTS_DIR=<目录>` 可以指定用哪份编译产物跑测试。

### 在测试网上跑（`tools/testnet`）

独立的 cargo workspace，直接用 JSON-RPC 访问公共节点 `https://testnet.ckb.dev/rpc`，不需要本地节点；
启动时会检查对方确实是 `ckb_testnet`，否则退出。前置条件是已经 `make setup`（出证明和验证密钥要用），
并构建了要部署的变体（在仓库根目录执行）：

```bash
make contracts VARIANT=size
```

资金账户是 `tools/testnet/dev.key` 里的一把 secp256k1 私钥（一行 64 位十六进制）。新建一把只用于测试网的：

```bash
(umask 077 && openssl rand -hex 32 > tools/testnet/dev.key)
```

构建工具：

```bash
cd tools/testnet && cargo build
```

用 `mixer address` 看地址，到测试网水龙头（<https://faucet.nervos.org>，每个地址有 30 万 CKB 的上限）领币。
命令（在 `tools/testnet` 下执行）：

```bash
./target/debug/mixer address                        # 资金账户的地址和余额
./target/debug/mixer deploy --variant size          # 把验证密钥和两个脚本存上链（size 构建锁 141,063 CKB）
./target/debug/mixer create --denomination 100      # 建池，面额以 CKB 计
./target/debug/mixer deposit --count 2              # 一笔交易存入 2 个新 note
./target/debug/mixer withdraw --note 0              # 把 note 0 取到一个新生成的地址；--fee 以 shannon 计，默认 1,000,000
./target/debug/mixer withdraw --note 2 --after-deposits 1   # 出完证明先再存 1 笔，再用已经变旧的根取款
./target/debug/mixer attack --note 3                # 只做 dry-run：池子必须拒绝的 6 种交易
./target/debug/mixer chain --note 3                 # 一笔存款 + 两笔取款首尾相接地连发，不等出块
./target/debug/mixer info                           # 从链上重建池子状态，列出历史和发过的交易
```

要点：

* 每笔交易发送前都先交给节点的 `test_tx_pool_accept` 完整执行一遍，通过了才发。
* 池子状态不落盘：每次运行都从建池交易开始，沿着池子 cell 的血缘读回全部交易并重放。
  `state.json` 只记链上找不回来的东西——部署和建池交易的 hash、note 的秘密、每笔交易的发送时间。
* `dev.key`、`recipient-<n>.key`、`state.json` 都是 600 权限并且在 `.gitignore` 里。
  **丢了 `state.json` 就取不出还在池子里的 note；`dev.key` 是一次性的测试网密钥，绝不能放真钱。**
* **池子和 `circuits/build/` 里的那份可信设置绑定。** `make setup` 每次都用新的随机数重新生成，
  `make clean` 会删掉它；之后验证密钥的 hash 就和池子 args 里的对不上，没人能再为这个池子出证明
  （工具会报错退出，不会发交易）。`circuits/build/` 不在 git 里。

## 四、实测结果

环境：Apple Silicon macOS，rustc 1.98.1，ckb-std 1.1，ckb-testtool 1.1.1（CKB-VM 解释执行），
snarkjs 0.7.5 + circom 2.2.3。CKB 主网 `max_block_cycles = 3,500,000,000`
（测试网相同，已用 `get_consensus` 核对）。本节是本地测量，链上的数字在第五节。

### 取款与存款（默认构建：opt-level 3）

| 操作 | cycles | 占单块上限 | 交易字节 |
| --- | ---: | ---: | ---: |
| 创建池子 | 121,656 | 0.003% | — |
| 存款 ×1（20 次 Poseidon） | 8,397,895 | 0.24% | 2,305 |
| 存款 ×2 | 8,402,924 | 0.24% | 2,337 |
| 存款 ×4 | 8,820,117 | 0.25% | 2,401 |
| 存款 ×8 | 10,069,674 | 0.29% | 2,529 |
| 存款 ×16 | 12,979,168 | 0.37% | 2,785 |
| **取款** | **约 69.4M–69.5M** | **约 1.98%** | 2,461 |

每次额外的 Poseidon 约 416k cycles。取款之间每多一个已花 nullifier（证明多带一个 32 字节兄弟、
bitmap 多一位）只增加约 23k–31k cycles：256 层遍历是固定开销（1.96M），与集合里有多少 key 无关。

### 编译变体对比

| 变体 | pool-type 体积 | 部署它要锁定 | 存款 ×1 | 取款 |
| --- | ---: | ---: | ---: | ---: |
| default（opt 3） | 276,640 B | 276,701 CKB | 8.40M | **69.4M–69.5M** |
| size（opt z，groth16-ckb 官方用的档位） | 121,400 B | 121,461 CKB | 11.26M | 102.3M–102.4M |
| bext（opt 3 + zba/zbb/zbc/zbs） | 269,936 B | 269,997 CKB | 8.31M | **68.6M–68.7M** |

`opt-level = z` 把二进制缩小 2.3 倍，但 cycles 增加 47%：加载脚本只按 4 字节 1 cycle 计费
（27.7 万字节也不过约 7 万 cycles），省下的体积换不回 z 档代码变慢的部分。

选哪一档取决于哪种资源更紧，两者不是一个量级：

* **链上空间**：代码 cell 每字节占 1 CKB，代码在链上一天就锁一天。default 比 size 多锁 155,240 CKB。
* **cycles**：size 档每笔取款多约 3,300 万 cycles，块占用 2.9% 对 2.0%，交易权重 17.5 KB 对 11.8 KB。
  按节点默认的最低费率（1,000 shannons/KB）折算，每笔只多付约 0.000056 CKB；真正的代价是每个全节点
  验证一笔取款要多花近一半时间，以及越过了 70M 的 `max_tx_verify_cycles`（影响见第七节）。

也就是说，default 档用多锁 15.5 万 CKB 换来更少的验证时间、更小的块占用和不越过 70M 阈值；
只算 CKB 的话 size 档便宜得多。测试网上部署的是 size 档（第五节），default 和 bext 没有上链。
B 扩展指令集只比 default 再省约 1.2%，而它能否执行取决于节点是否编译/开启了对应的 CKB-VM 特性
（这点本 PoC 没有在真实节点上验证），因此默认构建不依赖它。

### 取款阶段分解（profile 构建）

总约 69.5M cycles，下表取自一次运行（各阶段在多次运行间波动 < 0.2%）：

| 阶段 | cycles | 说明 |
| --- | ---: | --- |
| VM 启动 + 程序加载 | 70,722 | 276,640 B / 4 |
| 读脚本、状态、witness | 35,277 | |
| nullifier 集合（256 层 blake2b） | 约 1.96M | |
| 收款检查 + 读验证密钥 | 12,745 | |
| **Groth16 验证** | **约 67.4M** | 97% |
| 输出状态/容量比对 | 38,169 | |
| 池子 lock 与其它脚本 | 17,404 | |

存款（8.41M）里 Poseidon 树占 8.25M，其余约 160k。

### 与经济性相关的两点

* 交易打包权重 = `max(字节数, cycles × 0.0001705714)`，取款约相当于 **11.8k 字节**（size 构建约 17.5k），
  和 2% 的块占用一致：限制它的始终是 cycles，不是体积。
* **1 字节占 1 CKB，部署才是大头。** 池子 cell 占用 **1,819 CKB**（181,900,000,000 shannons）：
  1,640 字节状态 + 8 字节 capacity 字段 + lock 65 字节（code hash 32 + hash type 1 + args 32）
  + type 106 字节（32 + 1 + args 73）。池子 cell 不能销毁，所以这笔钱建池时付出后就永久留在里面。
  脚本贵得多：default 构建的 pool-type（276,640 B）要锁 **约 27.7 万 CKB**，加上 pool-lock（19,416 B）
  和验证密钥（392 B）共 296,631 CKB；size 构建共 141,063 CKB（测试网实付）。
  代码按 data hash 引用，所有面额的池子共用这一份。

## 五、测试网验证

2026-11-17 16:06–16:31（UTC）在 CKB 公共测试网（`ckb_testnet`；节点 `https://testnet.ckb.dev/rpc`，
CKB 0.210.0）上用 `tools/testnet` 走完了部署 → 建池 → 存款 → 取款。工具不保存池子状态：
每次都从链上交易重放出 commitment 树和 nullifier 集合，并要求重放结果与链上池子 cell 的状态和
capacity **逐字节一致**，否则不往下做。

### 部署了什么

上链的是 **size 构建**。default 构建的三个 cell 共需 296,631 CKB，水龙头每个地址只给 30 万 CKB，
建池和存款之后不剩重试的余地；size 构建只需 141,063 CKB。它的取款超过 70M cycles，
所以"超过 `max_tx_verify_cycles` 的交易能不能上链"也顺带验证了。

| cell（部署交易的输出） | 数据 | 锁定 | data hash |
| --- | ---: | ---: | --- |
| 0：验证密钥 | 392 B | 453 CKB | `0xfc49cb947eb743b15bddec9020c692ae0e40e0b0605f084e8c74dd62615220b4` |
| 1：pool-lock | 19,088 B | 19,149 CKB | `0xf24dbd685ea7c69568180977c6fbf9d1289931f4ebfe76e9be9dd1ddc58a763a` |
| 2：pool-type | 121,400 B | 121,461 CKB | `0x35b7c3facd07c523860da9968233d683d103c49cf7874a8f9d4ffab59fe7cc52` |

池子：面额 100 CKB，type script hash `0x1a0f972edbc41f81bbd25fe8ab2523169e54d5f37d9bc5c7c6145bc1adca3c89`，
type 和 lock 都以 `data2` 引用上面的代码。

### 交易

| 步骤 | 交易 | 区块 | 字节 | cycles | 手续费（CKB） | 确认耗时 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 部署（size 构建） | [`0xae257281…`](https://testnet.explorer.nervos.org/transaction/0xae25728119746b82cdc78d173548dbb2209957d2994bcc140fbbef534dc6dfeb) | 23,095,019 | 141,614 | 1,641,799 | 0.0070807 | 21 s |
| 建池（面额 100 CKB） | [`0x89e09f43…`](https://testnet.explorer.nervos.org/transaction/0x89e09f43dc283e4b63f7977bf46248a133df0e43b32226df97d768e7e4c6d43c) | 23,095,044 | 2,279 | 1,714,861 | 0.00011395 | 21 s |
| 存款 note 0 | [`0x478af823…`](https://testnet.explorer.nervos.org/transaction/0x478af823df21da96ad3f78ba8ccd3fbfb7a48dfd1e8f08078af81cb641cdc88a) | 23,095,069 | 2,421 | 12,893,907 | 0.00012105 | 28 s |
| 存款 note 1–2（一笔交易） | [`0xa09ea27d…`](https://testnet.explorer.nervos.org/transaction/0xa09ea27d5e89f556dc1fd347f279caca9b2c5b2918f130ddb0ef54bcc0bf3597) | 23,095,089 | 2,453 | 13,424,743 | 0.00012265 | 53 s |
| **取款 note 1** | [`0xf592d1ed…`](https://testnet.explorer.nervos.org/transaction/0xf592d1edfe0672c3bdf206daf2c7e8c9d40b75e4ab620c409d5ad860cee381be) | 23,095,111 | 2,484 | **102,467,811** | 0.01 | 17 s |
| **取款 note 0** | [`0x1bfa7118…`](https://testnet.explorer.nervos.org/transaction/0x1bfa71188ad0f14137e11f8b4d875c77f74ece258ae328d4e54c19eab8b4afaa) | 23,095,135 | 2,516 | **102,506,003** | 0.01 | 23 s |
| 存款 note 3 | [`0x2687434d…`](https://testnet.explorer.nervos.org/transaction/0x2687434d4e043b455fbbbd23cbd63e12ab66c803dc5294e025c22968d0947db2) | 23,095,202 | 2,421 | 12,899,204 | 0.00012105 | 26 s |
| **取款 note 2**（证明用的根已不是最新） | [`0x114b0f8a…`](https://testnet.explorer.nervos.org/transaction/0x114b0f8a2f51d57d0add498851d96659fc72da011a70ee5f0c18dddad4029a43) | 23,095,207 | 2,516 | **102,503,103** | 0.01 | 12 s |

"确认耗时"是从发出交易到节点报告 `committed` 的时间（每 3 秒轮询一次）；改用区块时间戳减去发送时刻，
8 笔依次是 13、6、26、45、6、19、23、6 秒。由资金账户出钱的交易按体积付 5,000 shannons/KB；
取款的手续费是面额里留给矿工的 0.01 CKB，约为节点默认最低费率的 57 倍。

### 结果

* **全流程成立。** 三笔取款各把 99.99 CKB（面额 − fee）付给一个此前从未在链上出现过的新地址。
  取款交易只有 1 个输入（池子 cell）、2 个输出、3 个 cell dep、1 个 witness（253 或 285 字节，
  里面只有 action，没有签名）；收款方不需要事先持有任何 CKB，手续费从面额里出。
* **超过 70M cycles 的交易可以上链。** 三笔取款都在 1.025 亿 cycles 左右，交给公共节点后 12–23 秒被打包。
  手续费给得很宽，最低费率下会不会一样快没有测。
* **链上 cycles 与本地 `ckb-testtool` 的测量吻合。** 取款：本地 102,273,287；链上三笔加上两次 dry-run
  在 102.47M–102.74M 之间，高 0.2%–0.5%，和不同证明之间的波动同一量级。存款 ×1：链上 12.89M–12.90M，
  本地 11.26M，多出的约 1.63M 是资金输入的 secp256k1 签名验证（建池交易的 1.71M 里有 1.61M 是同一项）；
  批量存款落在叶子 1–2 上，比本地从空树开始的 ×2 多一次 Poseidon。链上取款比本地表格里的大 23 字节：
  收款 lock 的 args 是 20 字节而不是测试用的 1 字节，另有 4 字节是交易在区块里的偏移量。
* **根历史有效。** note 2 的证明生成之后，先上链了一笔新存款（note 3），证明里的根因此不再是最新的；
  随后提交的取款仍被接受。
* **nullifier 集合有效。** 第 2、3 笔取款时集合里已经有 1 个、2 个 nullifier，witness 里各带一个
  32 字节的兄弟节点。
* **链上历史能还原出链上状态。** 工具从建池交易起沿池子 cell 的血缘读回全部 7 笔池子交易，
  用 `mixer-core` 重新执行，得到的 1,640 字节状态和 capacity 与链上池子 cell 完全相同。

### 对抗性 dry-run（真实节点）

`mixer attack` 把攻击交易交给节点的 `test_tx_pool_accept`：脚本完整执行，但交易不进池。
前后跑了两遍（第一笔取款之后针对 note 0，第三笔取款之后针对 note 3），结果相同：

| 交易 | 节点的回答 |
| --- | --- |
| 按证明原样取款（对照组，没有发送） | 接受，102,625,593 / 102,739,986 cycles |
| 把链上已有的一笔取款原样再发一次 | 拒绝，pool-type 退出码 34 `NullifierSpent` |
| 同一个证明，收款人换成别的 lock | 拒绝，44 `ProofRejected` |
| 同一个证明，fee 抬高 1 CKB | 拒绝，44 `ProofRejected` |
| 同一个证明，少付收款人 1 CKB | 拒绝，38 `RecipientCapacity` |
| 不走协议，把池子 cell 直接花成普通 cell | 拒绝，7 `GroupShape` |
| 存两个 commitment 只付一份面额 | 拒绝，51 `OutputCapacity` |

### 独立核对

不经过本工具，另外做了两项：用 `curl` 直接调节点的 `get_transaction`，确认三笔取款的结构
（输入、输出、witness、cell dep 的个数如上，输入 − 输出恰好 1,000,000 shannons，收款输出 99.99 CKB、
无 type、无 data）；用区块浏览器 API（`testnet-api.explorer.nervos.org`）核对全部 8 笔交易，
状态都是 committed，区块号、cycles、字节数、手续费与上表一致。

### 同块连发（2026-11-18）

为了弄清"所有交易共用一个池子 cell"到底把吞吐限制到什么程度，又做了一次实验（`mixer chain --note 3`）：
一笔存款和两笔取款，每一笔都花前一笔**尚未上链**的池子输出，3 秒内连发，不等出块。

| 步骤 | 交易 | 区块（块内序号） | 字节 | cycles | 手续费（CKB） | 区块时间 − 发送 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 存款 note 4 | [`0xc1c39972…`](https://testnet.explorer.nervos.org/transaction/0xc1c399726c94a35d97b721972ded0417d075507a8c710d759cf7de1f11fe5287) | 23,099,407（2） | 2,421 | 12,917,203 | 0.00012105 | 17 s |
| 取款 note 3 | [`0x6b6045e9…`](https://testnet.explorer.nervos.org/transaction/0x6b6045e913cf00342e1b5b0d9a33effa77f7e1f373a3a85675ce977c0372d344) | 23,099,407（3） | 2,548 | 102,690,161 | 0.01 | 15 s |
| 取款 note 4 | [`0x8e96c168…`](https://testnet.explorer.nervos.org/transaction/0x8e96c1684b5366aec3f8d983a47fb816d16fe23f3038f0ec14eabf8a9f197e43) | 23,099,407（4） | 2,580 | 102,461,184 | 0.01 | 14 s |

* **三笔进了同一个区块**，按依赖顺序排列；用 `get_block_by_number` 和区块浏览器 API 各核对了一遍。
  所以一个池子每块不止能处理 1 笔交易：知道前一笔交易内容的人可以一直往后接。
* 第三笔取的是第一笔刚存进去的 note，它的证明所用的根在发送时还不在链上。
* 这不等于多个用户可以各发各的。互不知情的用户仍会基于同一个池子 cell 构造出互相冲突的交易，
  存款人的签名还绑定了池子 cell 的版本。节点对冲突交易的处理（按手续费替换）是读源码得到的，
  没有实测；分析和对策见 [docs/intent-aggregation.md](docs/intent-aggregation.md)。

### 现状

* 资金账户 `ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqvknxgmtgdj7v3967rke3qdghad5j8tdpc060gf0`
  从水龙头领了 300,000 CKB：141,063 CKB 锁在上面三个 cell 里，1,819 CKB 永久留在池子 cell 里，
  先后存进池子 500 CKB（已全部取到五个收款地址，各得 99.99 CKB），六笔由它付费的交易
  手续费合计 0.0077 CKB，账上还剩 156,618 CKB。
* 池子 cell 是 `0x8e96c168…9f197e43` 的输出 0：capacity 1,819 CKB，5 笔存款、5 笔取款，里面已经没有未取的 note。
* 三个代码/密钥 cell 放在部署者的普通 secp256k1 lock 下，部署者可以把它们花掉。脚本是按 **data hash**
  引用的，所以这改不了池子的逻辑，也动不了池子里的钱；任何人把同样的字节重新部署一份就能继续用
  （要再锁 14.1 万 CKB）。这是可用性问题，不是资金安全问题；正式部署应把它们放在无人能解锁的 lock 下。

### 这次没有验证的

* default / bext 构建没有上链。size 构建上本地与链上的 cycles 吻合，default 的本地数字因此很可能
  也成立，但这是推断，不是测量。
* 只用了一个公共 RPC 节点。超过 70M cycles 的交易"交给这个节点 → 被打包"是通的；经其它默认配置的
  节点转发、以及链重组之后的行为没有观察。
* 30 个历史根的边界和树写满只在本地测试里覆盖。nullifier 集合在链上只到 3 个元素，大集合的开销是按
  "每多一个非空兄弟约 23k–31k cycles"外推的。
* 多个用户同时抢同一个池子 cell 没有测：同块连发是同一个人按顺序接的，互相冲突的交易真的发出去之后
  谁留下、谁被顶掉，只读了节点源码。一条链接到 3 笔，没有试到节点的上限（默认 25 笔）。
* 4 个 note 都是同一个人存、同一个人取，谈不上匿名集合：这是功能验证，不是隐私演示。
* 可信设置仍是 dev-only 的（第七节），测试网上这个池子的验证密钥不提供任何安全性。

## 六、已验证的不变量（`tests/tests/e2e.rs`）

诚实流程：

* 存款 → 取款全流程；批量存款后按任意顺序取款；存取交错；
* 证明在其根仍位于 30 个历史根内时一直有效，出环后 `UnknownRoot`，重新对新根出证明即可；
* 同一 commitment 存两次，两笔都可取（nullifier 绑定叶子下标）。

对抗性（每项前缀为"攻击者做的改动 → 期望的拒绝原因"）：

* 重放已花的证明（同一笔、新证明、旧的非存在证明）→ `NullifierSpent`；
* nullifier 别名 `N + r`（Groth16 方程成立）→ `NonCanonicalNullifier`，且验证器本身也拒收非规范公开输入；
* root 别名 → `NonCanonicalRoot`；
* 改收款人 / 抬高 fee 的中继攻击 → `ProofRejected`；少付 → `RecipientCapacity`；fee > 面额 → `FeeTooLarge`；
* 收款 cell 带 type script / 带 data / 不存在 → `RecipientShape` / `RecipientMissing`；
* 证明与 nullifier 混搭、翻转证明 bit、全零证明 → `ProofRejected` / `InvalidProof`；
* 攻击者自建一棵含自己叶子的树的根 → `UnknownRoot`；
* 缺验证密钥 cell、换成别的密钥 cell → `VkNotFound`（args 绑定了密钥 hash）；
* 多放一个输出偷钱、把池子容量抽干 → `OutputCapacity`；
* 输出状态不更新 nullifier 集合 / 塞入自选根 / 回滚 `next_index` / 多余字节 → `OutputState`；
* 存款容量差 1 或"两个叶子付一份钱" → `OutputCapacity`；非规范 commitment → `NonCanonicalCommitment`；
* 空 witness / 未知 action / 截断的 action → `Witness` / `ActionEncoding`；
* 池子初始状态非空、初始 lock 不是池子 lock → `InitialState` / `PoolLock`；
* 池子 cell 被换 lock → `PoolLock`；被销毁 / 拆两半 / 两个池子合并 → `GroupShape`；
* 池子 lock 用于非池子 cell → 101/102/103 号错误；
* 两个池子共用同一个收款输出（double satisfaction 攻击）→ `RecipientShape` / `ProofRejected` / `RecipientMissing`；
* 电路本身拒绝对错误 witness（密码错误、换 nullifier 下标、错误的根、未存款的 note）出证明。

另有 `tests/tests/smt_crate_forgery.rs`：复现第七节说的 `sparse-merkle-tree` 0.6.1 漏洞，
并验证自研 nullifier 集合对"已花 key 的非存在证明"一律拒绝。

## 七、已知问题与范围外

**安全与正确性方面**

* **可信设置是 dev-only 的**：单人、本机运行，toxic waste 未销毁。真实部署必须用多方仪式（MPC）或 universal setup。
* `verifier-core`（groth16-ckb）**未经审计**（项目自述 pre-audit），且是"Phase 0 占位格式"。
  上线前需要审计及版本化编码。
* **`sparse-merkle-tree` 0.6.1 有可伪造"不存在"证明的缺陷**（已向上游确认现象：
  2026-11-13 的 master 分支添加了相邻两侧合并的检查，但这个 `0x51` "折叠节点"路径仍然接受
  恶意字段，复现程序在 master 上依旧 20/20 成功）。所以 PoC 不复用它，也不建议任何
  "以已存在 key 的非存在证明"为基础的合约复用它。上游 crates.io 最新版仍是 2022 年的 0.6.1。
* 没有 Type ID：任何人可以用相同 args 再建一个同面额池子（各池子互不干扰，但这不是
  "同一个池子"），也没有"官方池子"的规范地址。测试网工具因此不按 type script 认池子，而是认建池交易：
  从带这个 type script 的 live cell 沿血缘往回走，能走到那笔建池交易的才算
  （`tools/testnet/src/pool.rs` 的 `Pool::load`）。冒牌池子偷不了钱，但会把用户引进一个匿名集合更小的
  池子。真实协议需要 Type ID 或登记表。v2 的池子 type script 自带 Type ID（已实现，未部署）。
* 电路里的 `recipient` 只覆盖收款 lock hash 的 31 个字节（末字节被置 0，否则 32 字节不一定是
  规范域元素，见 `crates/mixer-core/src/field.rs`）。证明因此没有完整绑定收款 lock hash：
  要偷走这笔钱，攻击者需要造出一个与受害者 lock hash 在前 248 位上相同的 lock 脚本，
  即该 hash 上的 248 位碰撞——按密码学假设不可行，但严格的实现应把 hash 拆成两个域元素。
* 没有域分离（domain tag）：电路只用于本池子的验证密钥，暂时安全，但协议扩展时需要。
* 交易编码是扁平的自定义格式，未使用 Molecule schema；args 长度检查靠代码。

**可用性与吞吐**

* **部署很贵。** 代码和验证密钥按 1 字节 1 CKB 锁定：size 构建 14.1 万 CKB，default 构建 29.7 万 CKB。
  只需部署一次、所有池子共用，但它是这个方案在 CKB 上最大的固定成本；每个池子另外永久占用 1,819 CKB。
* **池子 cell 是单点**：所有存款/取款都必须花同一个 cell，每笔交易都得建在前一笔的池子输出上。
  这**不等于**每块只能 1 笔：首尾相接的三笔池子交易实测进了同一个区块（第五节）。真正的问题是协调——
  存款人的签名绑定了池子 cell 的某一个版本，前面有交易插进来或被顶掉，这笔存款就得重签；
  互不知情的用户会构造出互相冲突的交易。这是与 Tornado（每笔交易独立、可并行）最大的现实差距。
  解决办法是池子 v2：存款意向 cell + 聚合，见 [docs/intent-aggregation.md](docs/intent-aggregation.md)。
  链上脚本已实现并有本地测试；拼批次的聚合者工具没有写，v2 没有上过测试网。
* 中继者拿不到保证：fee 最终给矿工，谁都可以打包这笔无签名交易。Tornado 用 relayer registry
  解决；PoC 没有。
* 取款证明生成在主机侧约 1–2 秒、依赖 snarkjs/Node；没有浏览器端实现。
* 根历史只有 30 笔存款交易；活跃池子里证明必须及时上链（Tornado 是 100 个根）。
  可以用更大的状态或"根=最近 N 块"的时间策略改善。v2 里根历史的长度是池子的参数，一个批次只用掉一个根。
* `max_tx_verify_cycles`（默认 70M）不是共识限制：本地提交按 `max_block_cycles`（3.5G）验证，
  70M 只影响"输入尚不存在的孤儿交易由哪类 worker 验证"以及重组后重新加回交易池的上限。
  默认构建的 69.4M–69.5M 刚好在 70M 以内（余量不到 1%，电路或验证密钥一改就可能越界）；
  opt z 档（102M）越过了它。测试网实测，五笔 1.025 亿 cycles 左右的取款交给公共节点后都正常打包（第五节）；
  按源码的读法，重组后这类交易会从交易池被丢弃（仍可重新提交），这一点没有在链上观察到。

**未验证（本次 PoC 没做的）**

* 测试网上只跑了 size 构建、一个池子、单个用户（包括同块连发），没覆盖的部分列在第五节末尾；
* 没有做手续费估算和面向用户的钱包：测试网工具用固定费率和节点自带的索引器，note 的秘密明文存在本机文件里；
* 没有做聚合证明、递归证明或多池子并行验证（两个池子在同一笔交易里是验证了，但不省 cycles）；
* 没有做拒绝服务/内存炸弹分析（`verifier-core` 已对长度前缀做过检查，256 层 SMT 是常数开销）。

## 八、许可与来源

* 本仓库代码：PoC，未选许可证（内部研究用）。
* `groth16-ckb` 的 `verifier-core`：MIT OR Apache-2.0，pin 在 commit `e969ad1`，作为库链接进 type script。
* circom / circomlib / snarkjs：GPL-3.0（工具链，不进入链上产物）。
