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
* **池子 v2 已实现并在测试网上跑过**（存款意向 cell + 批量结算 + Type ID，
  [docs/intent-aggregation.md](docs/intent-aggregation.md)）：存款人把钱放进意向 cell 就可以离开，
  任何人都能把一批意向和取款拼成一笔交易。本地实测（默认构建）每多一笔取款 69.6M cycles、
  每多一笔存款 0.40M，一个块最多装 50 笔取款。测试网上（size 构建）用 `mixer2` 跑了 55 笔存款、
  55 笔取款，最大的一批是 **20 笔取款、20.5 亿 cycles（块上限的 59%）**，14 秒后出块；
  意向撤回、13 种攻击交易的 dry-run、两个聚合者抢同一批意向（按手续费替换）都在真实节点上验证了，
  见第五节的 v2 小节。除了那一小节，本文第一到第七节描述的都是 v1。

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
  src/v2.rs                v2：部署/建池/意向/批量结算/撤回交易，取款消息，从链上重放池子状态
  src/cli.rs               两个命令共用的部分：note、状态文件、参数解析、交易表格
  src/bin/mixer.rs         v1 的命令行入口
  src/bin/mixer2.rs        v2 的命令行入口：存款人、取款人、聚合者三个角色
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

池子 v2 用同一个 crate 里的另一个命令 `mixer2`，状态文件是 `state-v2.json`（同样不进 git）。
它一个人扮演三个角色：存款人建意向、取款人把取款消息写到 `requests/`、聚合者把找到的东西拼成一批。

```bash
./target/debug/mixer2 deploy --variant size          # 存 pool-type-v2 和 intent-lock；验证密钥和 pool-lock 复用 v1 的 cell
./target/debug/mixer2 create --denomination 150 --root-history 32   # 建池（Type ID 由第一个输入算出）
./target/debug/mixer2 intent --count 4 --tip 1       # 建 4 个存款意向 cell，各 151 CKB；--reclaim-after 是可撤回前要等的块数
./target/debug/mixer2 aggregate                      # 聚合者：把等着的意向和 requests/ 里的取款拼成一笔交易
./target/debug/mixer2 request --note 0 --to self     # 取款人：给 note 0 出证明，写出取款消息（不含任何秘密）
./target/debug/mixer2 aggregate --deposits 2 --withdrawals 2 --wait no   # 限制批次大小；不等打包就返回
./target/debug/mixer2 reclaim --note 5               # 存款人：等待期满后撤回意向
./target/debug/mixer2 attack                         # 只做 dry-run：脚本必须拒绝的交易
./target/debug/mixer2 pay --key stranger.key --ckb 100   # 给另一把密钥转一点 CKB（没有就新建），让它当第二个聚合者
./target/debug/mixer2 aggregate --key stranger.key   # 换一个聚合者来拼这一批
./target/debug/mixer2 race --key stranger.key --rates 5000,6000,7000   # 两个聚合者抢同一批意向：
                                                     # 先发我们的批次，再让对方按每个费率各发一笔冲突的
./target/debug/mixer2 info                           # 从链上重建池子状态，列出历史、等着的意向和发过的交易
```

* 聚合者不需要任何人的签名或配合：意向 cell 和取款消息都是公开的，它只给自己垫付手续费的那个输入签名。
  意向里超出面额的部分（小费）和取款的 fee 都落到它的找零里。
* `--reclaim-after` 是**块数**（默认 100），写进意向 lock 的 args；撤回交易的 `since` 由工具换算。
  这个字段必须小于 2^56，工具会检查——脚本不检查，写错的后果见第七节。
* 取款消息是 `requests/` 下的 JSON 文件（不含秘密，不进 git）；工具没有做网络传输，
  "把消息交给聚合者"就是把文件放进这个目录。
* 矿工费按交易的**权重**算（`--fee-rate`，默认 5,000 shannons/KB）。带取款的批次权重由 cycles 决定，
  远大于字节数，所以工具先 dry-run 得到 cycles，再定手续费。
* 意向 cell 至少占 145 CKB，所以面额 + 小费不能低于 145 CKB；v1 那个 100 CKB 的面额在 v2 里做不了意向存款。

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
  存款人的签名还绑定了池子 cell 的版本。节点对冲突交易按手续费替换，当时只读了源码，
  后来在 v2 上实测了（本节的 v2 小节）；分析和对策见 [docs/intent-aggregation.md](docs/intent-aggregation.md)。

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
* 多个用户同时抢同一个 v1 池子 cell 没有测：同块连发是同一个人按顺序接的。互相冲突的交易谁留下、
  谁被顶掉，后来在 v2 的池子上实测了（下面的 v2 小节）。一条链接到 3 笔，没有试到节点的上限（默认 25 笔）。
* 4 个 note 都是同一个人存、同一个人取，谈不上匿名集合：这是功能验证，不是隐私演示。
* 可信设置仍是 dev-only 的（第七节），测试网上这个池子的验证密钥不提供任何安全性。

### 池子 v2（2026-11-18）

2026-11-18 08:02–12:42（UTC）用 `mixer2` 把 v2 部署到同一个测试网并跑了一遍，仍然是 size 构建、
同一个资金账户、同一个节点。和 v1 一样，工具每次都从建池交易起重放全部池子交易，
并要求结果与链上池子 cell 逐字节一致。下面的 cycles、字节数和手续费都是链上记录的值；
每一笔的链上 cycles 都与发送前 dry-run 得到的相同。

**部署和建池**

| cell | 数据 | 锁定 | data hash |
| --- | ---: | ---: | --- |
| 验证密钥、pool-lock | | | 复用 v1 部署交易 `0xae257281…` 的输出 0、1（字节相同，不用再锁一份） |
| pool-type-v2（`0x25d31a87…` 输出 0） | 127,368 B | 127,429 CKB | `0x9e98a45f112e4f9d5721a726a156122159011e23e8bf393b1876a9ac3c89ff6b` |
| intent-lock（`0x25d31a87…` 输出 1） | 23,624 B | 23,685 CKB | `0xb3753821112dae7a749a01954662056368fd9cf873294376e82cda98ad751783` |

v2 多锁 151,114 CKB，连同 v1 共 292,177 CKB。池子：面额 150 CKB、保留 32 个历史根、状态 1,704 字节、
空池子 cell 占 1,950 CKB；Type ID `0x22dcc42854379ee5996852e60fc7337d37257ffb4fd2c4acb48adca69373efa5`，
type script hash `0xb415f2f2842ca7cb87f229499f2cbb1804be7efbeb20452efc79de20da261632`。
面额选 150 是因为意向 cell 至少占 145 CKB。

**交易**（池子交易 17 笔中有代表性的；完整列表用 `mixer2 info` 从链上重建）

| 步骤 | 交易 | 区块 | 字节 | cycles | 手续费（CKB） | 区块时间 − 发送 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 部署 v2（size 构建） | [`0x25d31a87…`](https://testnet.explorer.nervos.org/transaction/0x25d31a87791fbb34b5111d537a38f3e116fcaaabf219839fd5f3939e3b3744b1) | 23,102,162 | 151,721 | 1,642,040 | 0.00758605 | 16 s |
| 建池（150 CKB，32 个根） | [`0x79fa82a1…`](https://testnet.explorer.nervos.org/transaction/0x79fa82a11aa151830e28f8ddebc474c8fb1afdde98c522e47ef55720f4dc0173) | 23,102,867 | 2,410 | 1,755,482 | 0.0001205 | 12 s |
| 建 1 个意向 | [`0xbb384960…`](https://testnet.explorer.nervos.org/transaction/0xbb3849607095e175b4770c3adad2d32c3c7ca33a7719dfaf7174dc8caad82c45) | 23,103,008 | 548 | 1,643,322 | 0.0000274 | 52 s |
| 建 13 个意向（一笔交易） | [`0x4a931e43…`](https://testnet.explorer.nervos.org/transaction/0x4a931e43a5029d79c6591504918f47b1f2bfc01e9a2bd1ac61accd59de7c142e) | 23,103,485 | 2,864 | 1,642,061 | 0.0001432 | 22 s |
| 批次：1 笔存款 | [`0x065eb0ce…`](https://testnet.explorer.nervos.org/transaction/0x065eb0ce8fff0dbfaebfd898eaa9ca222d9b4792cf7236db55ffa832230e7e20) | 23,103,049 | 2,611 | 12,955,362 | 0.00013055 | 16 s |
| 批次：5 笔存款 | [`0x72090b85…`](https://testnet.explorer.nervos.org/transaction/0x72090b85d1ccab0330d91de1e6e2745b9a2777b32e559d04e396e3a0681be829) | 23,103,386 | 2,819 | 14,782,158 | 0.00014095 | 40 s |
| 批次：12 笔存款 | [`0x74ffe6f0…`](https://testnet.explorer.nervos.org/transaction/0x74ffe6f0a66b3a326548c3e06bef484fd9b7053ea29ca20ccfba5fd4b2f7a845) | 23,104,003 | 3,183 | 19,565,642 | 0.00016705 | 12 s |
| 批次：1 笔取款 | [`0xab5cf8d2…`](https://testnet.explorer.nervos.org/transaction/0xab5cf8d28732739597c9c65e0db4750bd473d09b76d871c93ebcf9c870f57278) | 23,103,093 | 2,902 | 104,427,039 | 0.00089045 | 19 s |
| 批次：2 笔取款 | [`0x636c8ab8…`](https://testnet.explorer.nervos.org/transaction/0x636c8ab8b9f630e59c457bfb5b2eda07ff6fa8d2487c3c2bd24e0316cc269f5f) | 23,103,478 | 3,373 | 207,241,124 | 0.0017674 | 14 s |
| 批次：13 笔存款 + 3 笔取款 | [`0xb3afdafc…`](https://testnet.explorer.nervos.org/transaction/0xb3afdafc5405ce00d757d298c5cad319f7fe765bf822478a0e6de1c5a385d7f5) | 23,103,512 | 4,589 | 328,206,949 | 0.00279915 | 11 s |
| 批次：7 笔存款 + 4 笔取款 | [`0x2f22290b…`](https://testnet.explorer.nervos.org/transaction/0x2f22290b8a2f098711f589439184d7f79236fa8766d6030a4e53c5964df7d82e) | 23,103,566 | 4,748 | 426,450,013 | 0.0036369 | 16 s |
| **批次：8 笔取款** | [`0xa710a0ed…`](https://testnet.explorer.nervos.org/transaction/0xa710a0ed88af281d8afc0619b7988f5f8ca1582537fcad4eed05f6a691d7a471) | 23,103,590 | 6,327 | **822,699,751** | 0.00701655 | 15 s |
| **批次：16 笔取款** | [`0xab0c8cd0…`](https://testnet.explorer.nervos.org/transaction/0xab0c8cd0b57714ff407661852e7a548d7871be63425161ca0e68d8c1ecf2c549) | 23,103,701 | 10,799 | **1,641,707,368** | 0.0140014 | 24 s |
| **批次：20 笔取款** | [`0x6d267900…`](https://testnet.explorer.nervos.org/transaction/0x6d267900d7ed9b2b35d55167b75e9877e1fa8b93ee49b98fde9da2e8f0dc5c27) | 23,104,146 | 13,611 | **2,054,246,107** | 0.01752005 | 14 s |
| 另一把密钥当聚合者：2 笔存款，100,000 shannons/KB | [`0x93b6a96c…`](https://testnet.explorer.nervos.org/transaction/0x93b6a96c5bdb5402d0e330d9b42dfd66079cbfeddc5e8503a8d26ce89daf7c22) | 23,103,763 | 2,663 | 13,582,497 | 0.002663 | 27 s |
| 撤回意向（等待 6 个块） | [`0xc4437dd3…`](https://testnet.explorer.nervos.org/transaction/0xc4437dd305eec8f9ba3a1541af8e16601ee8888f388150bdc0c9f267fb028607) | 23,104,241 | 444 | 1,644,072 | 0.0000222 | 18 s |

**结果**

* **全流程成立。** 55 个 note 经意向 cell 存进池子，又全部取出（1 笔付给新生成的地址，其余付回资金账户，
  各 149.99 CKB）；池子 cell 回到 1,950 CKB。存款人只发了"建意向"那一笔交易，之后的批次里没有他们的签名。
* **大交易能上链。** 8、16、20 笔取款的批次分别是 8.2 亿、16.4 亿、20.5 亿 cycles（块上限 35 亿的
  23%、47%、59%），交给公共节点后 14–24 秒出块，和小交易没有区别。v1 时验证过的最大单笔是 1.03 亿。
  20 笔不是脚本或节点的上限，是资金的上限：水龙头的额度用完了，账上的钱只够同时在池子里放 20 个 note；
  本地测得 size 构建一个块最多 34 笔，21–34 笔没有上链试过。
* **链上 cycles 与本地测量吻合。** 8 笔取款：本地 821,600,169，链上 822,699,751（+0.13%）；
  16 笔：本地 1,640,059,819，链上 1,641,707,368（+0.10%）。本地的池子保留 128 个根、聚合者用的是测试 lock，
  链上是 32 个根外加一次 secp256k1 验签（约 1.6M），所以不是严格的同一笔交易。
  每多一笔取款链上是 1.024 亿–1.031 亿（nullifier 集合越大越贵一点），本地 1.025 亿。
  存款很便宜：1、5、8、12 笔存款的批次是 1,296 万、1,478 万、1,829 万、1,957 万 cycles，
  每多一笔约 0.5M–0.8M（和叶子落在树的什么位置有关；本地从空树开始测是 0.55M）。
* **手续费按权重收，不按字节。** 节点把交易的大小算作 `max(字节数, cycles × 597,000 / 35 亿)`。
  20 笔取款的交易只有 13.6 KB，权重却是约 35 万字节，按 5,000 shannons/KB 付了 0.0175 CKB；
  它从 20 笔取款里收到 0.2 CKB 的 fee。只按字节数算手续费的话，这笔交易的费率会低于节点的最低要求。
* **聚合者不需要许可。** `stranger.key` 只有一个 100 CKB 的普通 cell，和存款人没有任何联系，
  照样把两个意向成交了（上表倒数第二行）。
* **撤回受共识保护。** 意向建好后立刻撤回，节点拒绝：`Immature(Inputs[0]): the transaction is immature
  because of the since requirement`；过了 6 个块后同一笔交易被接受（撤回输入的 `since` 是
  `0x8000000000000006`）。做了两次：一次是普通意向，一次是用 `data1` 引用 lock 的意向。
* **链上历史能还原出链上状态。** 17 笔池子交易重放后与池子 cell 的 1,704 字节状态和 capacity 完全相同。
* 独立核对：用 `curl` 直接向节点查了 20 笔取款的批次（2 个输入、22 个输出：池子、20 个各 149.99 CKB 的
  收款 cell、聚合者的找零）、被顶掉的那笔和撤回交易；用区块浏览器 API 核对了 8、16、20 笔取款三笔的
  区块号、cycles、字节数和手续费，与上表一致。

**对抗性 dry-run（真实节点）**

`mixer2 attack` 把下面的交易交给节点的 `test_tx_pool_accept`，全部按预期被拒：

| 交易 | 拒绝它的脚本和退出码 |
| --- | --- |
| 按证明原样取款（对照组，没有发送） | 接受，102,585,941 cycles |
| 既不存也不取、空转池子 cell | pool-type-v2，14 `EmptyBatch` |
| 花掉意向，池子状态不变（不记账） | pool-type-v2，50 `OutputState` |
| 意向记了账，面额没进池子 | pool-type-v2，51 `OutputCapacity` |
| 所有者撤回，不带 `since` | intent-lock，63 `ReclaimTooEarly` |
| 所有者撤回，`since` 写 0 个块 | intent-lock，63 `ReclaimTooEarly` |
| 别人不经过池子直接花意向 | intent-lock，62 `Unauthorized` |
| 用 `data1` 引用 lock 的意向（池子不认），和池子放进同一笔交易想白拿 | intent-lock，60 `PoolMismatch` |
| 把链上已有的一笔取款再发一次 | pool-type-v2，34 `NullifierSpent` |
| 同一个证明，收款人换成别的 lock | pool-type-v2，44 `ProofRejected` |
| 同一个证明，fee 抬高 1 CKB | pool-type-v2，44 `ProofRejected` |
| 同一个证明，少付收款人 1 CKB | pool-type-v2，38 `RecipientCapacity` |
| 一批里把同一个 note 取两次 | pool-type-v2，34 `NullifierSpent` |
| 两笔取款互换收款人 | pool-type-v2，44 `ProofRejected` |

**两个聚合者抢同一批意向**

`mixer2 race`：资金账户先发一笔成交 2 个意向的批次，另一把密钥随后发三笔花同样输入的批次，费率依次提高。
这次是真的发出去，不是 dry-run。

| 交易 | 手续费（shannons） | 节点的回答 |
| --- | ---: | --- |
| 第一个聚合者的批次 `0x2645b58b…`，2,663 字节，5,000/KB | 13,315 | 进了交易池 |
| 对手，5,000/KB | 13,315 | `PoolRejectedRBF: RBF rejected: Tx's current fee is 13315, expect it to >= 17309 to replace old txs` |
| 对手，6,000/KB | 15,978 | 同样被拒 |
| 对手，7,000/KB [`0x7b5484c1…`](https://testnet.explorer.nervos.org/transaction/0x7b5484c17e35eeae7235aa6021fb9e11a62389264dcbc7f97b705906efc1e4bd) | 18,641 | 接受，在块 23,103,943 上链；第一笔被节点标为 `rejected`（`RBFRejected: replaced by tx 0x7b5484c1…`） |

* 门槛和读源码得到的规则一致：被顶掉的交易的手续费 + 1,500 shannons/KB × 新交易的字节数
  = 13,315 + 3,994 = 17,309。出价不够的留不下，出价够的把先到的顶掉。
* 两边的存款人都没有参与：意向 cell 没有签名，谁成交都一样进池子。输掉的聚合者只损失了时间。
* **`test_tx_pool_accept` 不执行替换规则**：三笔对手交易的 dry-run 都说"可以接受"，真发出去前两笔被拒。
  所以 dry-run 通过不代表发得出去。

**工具里踩到的一个坑：`reclaim_after` 是块数，不是 `since`**

第一版 `mixer2` 把编码好的 `since`（最高位是 1）写进了意向 args 的等待字段，而脚本把这个字段当作块数，
和撤回输入的 `since` 的低 56 位比较——这样的值永远满足不了，意向就**永远撤不回**。`mixer2 attack` 里
"所有者按要求的 `since` 撤回"被拒（退出码 63）才发现。修正：字段改名为 `reclaim_after`、
工具写入块数并拒绝 2^56 以上的值。链上脚本的字节没有变（改名前后的 size 构建产物逐字节相同）。

* 用旧工具建的普通意向（note 0–4、6）后来都正常成交进了池子，没有损失。
* **note 5 的意向（151 测试网 CKB）永久锁死了**：它既用 `data1` 引用 lock（池子不认，成交不了），
  等待字段又是坏的（撤不回）。它还挂在链上，`mixer2 info` 会一直报"1 个意向在等"。
* 脚本接受这个字段的任意 u64，建 cell 时也没有脚本运行，所以写错的钱包会造出只能成交、不能撤回的意向。
  这是协议层面的隐患，记在第七节。

**现状**

* v1 之后账上的 156,618 CKB：151,114 CKB 锁在两个新的代码 cell 里，1,950 CKB 永久留在 v2 池子 cell 里，
  151 CKB 锁死在 note 5 的意向里，149.99 CKB 在一个收款地址上，约 104 CKB 在 `stranger.key` 上，
  手续费合计约 0.06 CKB，资金账户还剩 3,148.94 CKB。水龙头每个地址 30 万 CKB 的额度已经用完。
* v2 池子 cell 是 `0x6d267900…f0dc5c27` 的输出 0：55 笔存款、55 笔取款，里面没有未取的 note。

**v2 没有验证的**

* 21 笔以上取款的批次（资金不够，见上）；default / bext 构建没有上链。
* 根历史的边界（32 个根用完之后旧证明失效）只在本地测试里覆盖；链上一共只记录了 11 个根，没有绕过一圈。
* 只用了一个公共 RPC 节点：大交易经其它节点转发、矿工在拥堵时会不会挑它，都没有观察。
  测试期间测试网不拥堵，"14–24 秒出块"说明不了有竞争时的情况。
* 两个"聚合者"是同一个人的两把密钥、同一个程序，取款消息靠本地文件传递。没有真正的第三方聚合者，
  没有消息的网络传输，也没有聚合者同时处理交易池里未上链批次的逻辑（工具每批都等上一批上链）。
* 抢意向只试了"一笔顶一笔"。顶掉一条链、对手持续抬价、以及带取款的批次被顶掉后重拼，都没有做。
* 所有 note 仍是同一个人存、同一个人取：功能验证，不是隐私演示。可信设置仍是 dev-only 的。

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
  池子。真实协议需要 Type ID 或登记表。v2 的池子 type script 自带 Type ID（已在测试网上建池验证）：
  `mixer2` 按 type script 找池子，要求带这个 type 的 live cell 恰好有一个。
* 电路里的 `recipient` 只覆盖收款 lock hash 的 31 个字节（末字节被置 0，否则 32 字节不一定是
  规范域元素，见 `crates/mixer-core/src/field.rs`）。证明因此没有完整绑定收款 lock hash：
  要偷走这笔钱，攻击者需要造出一个与受害者 lock hash 在前 248 位上相同的 lock 脚本，
  即该 hash 上的 248 位碰撞——按密码学假设不可行，但严格的实现应把 hash 拆成两个域元素。
* 没有域分离（domain tag）：电路只用于本池子的验证密钥，暂时安全，但协议扩展时需要。
* 交易编码是扁平的自定义格式，未使用 Molecule schema；args 长度检查靠代码。
* **v2 的意向 cell 建错了没人拦。** 建 cell 时不运行它的 lock，所以 args 写错只有在花的时候才暴露。
  已知两种写法会丢钱或锁钱：等待字段 `reclaim_after` 写成 2^56 以上的值（比如误写成编码后的 `since`），
  意向就永远撤不回，只能等人成交；再加上用池子不认的 hash type 引用 lock，就两条路都走不通，钱永久锁死
  ——测试网上就有这样一个 cell（151 CKB，第五节）。脚本没有改，现在的保护只在工具里：
  钱包必须在建意向之前自己校验 args。

**可用性与吞吐**

* **部署很贵。** 代码和验证密钥按 1 字节 1 CKB 锁定：size 构建 14.1 万 CKB，default 构建 29.7 万 CKB。
  只需部署一次、所有池子共用，但它是这个方案在 CKB 上最大的固定成本；每个池子另外永久占用 1,819 CKB。
* **池子 cell 是单点**：所有存款/取款都必须花同一个 cell，每笔交易都得建在前一笔的池子输出上。
  这**不等于**每块只能 1 笔：首尾相接的三笔池子交易实测进了同一个区块（第五节）。真正的问题是协调——
  存款人的签名绑定了池子 cell 的某一个版本，前面有交易插进来或被顶掉，这笔存款就得重签；
  互不知情的用户会构造出互相冲突的交易。这是与 Tornado（每笔交易独立、可并行）最大的现实差距。
  解决办法是池子 v2：存款意向 cell + 聚合，见 [docs/intent-aggregation.md](docs/intent-aggregation.md)。
  v2 已在测试网上跑通（第五节）。它把"存款人要重签"这个问题去掉了，池子 cell 仍然是单点：
  聚合者之间按手续费互相顶替，有人愿意一直抬价就能一直占着池子 cell；
  现有的聚合者工具是单机的演示，每批都等上一批上链，没有网络接口。
* 中继者拿不到保证：fee 最终给矿工，谁都可以打包这笔无签名交易。Tornado 用 relayer registry
  解决；PoC 没有。
* 取款证明生成在主机侧约 1–2 秒、依赖 snarkjs/Node；没有浏览器端实现。
* 根历史只有 30 笔存款交易；活跃池子里证明必须及时上链（Tornado 是 100 个根）。
  可以用更大的状态或"根=最近 N 块"的时间策略改善。v2 里根历史的长度是池子的参数，一个批次只用掉一个根。
* `max_tx_verify_cycles`（默认 70M）不是共识限制：本地提交按 `max_block_cycles`（3.5G）验证，
  70M 只影响"输入尚不存在的孤儿交易由哪类 worker 验证"以及重组后重新加回交易池的上限。
  默认构建的 69.4M–69.5M 刚好在 70M 以内（余量不到 1%，电路或验证密钥一改就可能越界）；
  opt z 档（102M）越过了它。测试网实测，五笔 1.025 亿 cycles 左右的取款交给公共节点后都正常打包，
  v2 的批次最大到 20.5 亿 cycles 也一样（第五节）；
  按源码的读法，重组后这类交易会从交易池被丢弃（仍可重新提交），这一点没有在链上观察到。

**未验证（本次 PoC 没做的）**

* 测试网上只跑了 size 构建、v1 和 v2 各一个池子、单个用户（包括同块连发和两把密钥的聚合者竞争），
  没覆盖的部分列在第五节两处"没有验证的"里；
* 没有做手续费估算和面向用户的钱包：测试网工具用固定费率和节点自带的索引器，note 的秘密明文存在本机文件里；
* 没有做聚合证明、递归证明或多池子并行验证（两个池子在同一笔交易里是验证了，但不省 cycles）；
* 没有做拒绝服务/内存炸弹分析（`verifier-core` 已对长度前缀做过检查，256 层 SMT 是常数开销）。

## 八、许可与来源

* 本仓库代码：PoC，未选许可证（内部研究用）。
* `groth16-ckb` 的 `verifier-core`：MIT OR Apache-2.0，pin 在 commit `e969ad1`，作为库链接进 type script。
* circom / circomlib / snarkjs：GPL-3.0（工具链，不进入链上产物）。
