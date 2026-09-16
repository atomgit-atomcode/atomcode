# 交接：双引擎收掉了，手写链删了。2026-09-16

> 🟢 **接手先读这一页。** 它替掉 `handoff-coding-on-harness-2026-09-14.md` 成为
> plexus/coding 这条线的入口。那一页说的「两个引擎并行、差分台对跑」已经不成立 ——
> 链没了，差分台现在对的是存档。
>
> 分支 `feat/collapse-dual-engine`（从 `feat/plexus-plugin-architecture@617a93fe` 分出），
> **34 个 commit，全部本地，未 push**（用户明确要求不推）。`feat/plexus-plugin-architecture`
> 本身已与 origin 同步(0/0)，所以合回去是纯 fast-forward。

## 一句话结论

`atomcode-coding` 现在只有一套装配：`parts::prepare` 建能力图 → `runtime::mount`
挂成 plexus 行清单。手写的 kernel 中间件/钩子链和 `ATOMCODE_ENGINE` 开关都不存在了，
生产默认路径就是树。

```
                  以前                          现在
  prepare ──┬── parts::assemble ──┐      prepare ── runtime::mount ── AgentHandle
            │   (assemble.rs 333)  ├─ Handle
            └── on_harness::mount ─┘
              ↑ ATOMCODE_ENGINE 选
```

`crates/atomcode-coding/src/parts.rs` 3532 → 2507 行，`assemble.rs`(333)整份删除。
分支总账 120 文件 +9483/−3030，其中 1389 行是新录的 golden 存档。

## 三条不要重议的决定（用户 09-16 拍板）

1. **会话：原生快照是主，harness JSONL 是从。** resume / undo / rewind / restore /
   会话目录 / 租约 / 落盘 fail-close 全认 `SessionManager`。`session-persistence-jsonl`
   那一行照写，但 `resume = false`，而且写在**自己的 root**
   `<home>/sessions/harness/`，和原生 transcript 不共用文件。谁也不许读对方的文件当权威。
   （AGENTS.md 已写死这一条。）
2. **翻默认和删链在同一分支，分成两个 commit**（`c81218a4` 翻、`f3a7f048` 删），
   完事之后不留引擎开关。
3. **差分台变成「树 对 存档」。** 删链前用链式把 56 个场景的事件流录成
   `crates/atomcode-coding/tests/golden/differential/*.json`，棘轮文件
   `gates/differential.baseline` 照旧。**这些 golden 不能重录**（录它的引擎没了）——
   要改就改文件本身，并在 commit 里写清为什么。

## 现在的装配长什么样

```rust
let parts   = prepare(&cfg, opts).await?;                       // 能力图
let mounted = runtime::mount(&parts, &cfg, &opts, provider).await?;
// mounted.handle 驱动回合；mounted.app 必须比 handle 活得久
```

`Mounted { handle, app, providers }` 是唯一公开装配入口，`runtime.rs` /
测试 / 差分台全走它。行清单在 `on_harness.rs`(2326 行)，只有产品能造的那几行在
`host_rows.rs`(1455 行)：会话存储、`task`/`team`/`code_review`/`recall`/
`request_user_input`、CodingPlan 限流判定、原生压缩检查点。

## 12 步都做了什么（细节在 `docs/collapse-dual-engine.md`）

那一页有完整的步骤表、每步的 commit 和判据。这里只列最容易踩的四件：

- **`parts::wire_side_providers`** 是 review / subagent 槽的唯一接线处（分层、
  tier 会话 id、detached 用量记账、带 surface 计量、logout 清槽）。以前两个引擎各接一遍。
- **`HOST_PROVIDED` 缝**：`models`、`tool-driver`(工具进度 + 提问)、会话存储等
  必须由宿主填，行自己造不出来。
- **`host_only_tools`**：`request_user_input` / `task` / `team` / `code_review` /
  `recall` 五个由产品挂，`harness_option_rows` 里对应的 harness 自带行永远 disabled。
  两边同时挂 = 工具重名，判据会红。
- **`HostState.model`** 用的是人选的模型名，不是 provider 自报的那个（人设和 datalog 都读它）。

## 删链那一步测试抓到的 6 个真 bug（都已修）

12 个链式测试文件改挂树，6 个第一次跑就红，每一条都是真差异，不是测试没写对：
transcript 两个写者、mount 对不完整聚合 fail-open、第一回合溢出无法恢复、
内部提醒的回话外露、模型名取错、resume 后重复提醒。逐条见
`docs/collapse-dual-engine.md#删链时测试抓到的真问题各自已修`。

**这一条值得记住**：把旧测试原样挂到新装配上跑，比再写一批新测试更能抓 bug。

## 删链之后又修了 6 条（2026-09-16）

两个对抗性 review agent 查完，6 条发现全部成立、全部已修。**其中两条是安全性的，而且都
不在「已知差异」清单里——不是有意保留，是漏掉了。** 逐条见
`docs/collapse-dual-engine.md#删链之后的-review-与修复2026-09-16`。

最该读的一条是 A：`ToolExec.pre_approved: bool` 换成了
`Authorization`(`No`/`Presumed`/`ByPerson`)。一个 bool 同时承载「人对这一次点了头」和
「有条配置说别问」，凭据边界分不出来只好都认，于是 `allow = ["Bash(curl *)"]` 让
`credential_shell = "strict"` 形同虚设。

**从这条里学到的、会影响你以后所有改动的一件事：**

> plexus 的 `App::start` 写着 **"Order in the file is irrelevant"** ——
> 行在清单里的位置**不决定执行顺序**（顺序 = 注册时机 = 挂载顺序），
> `prepend` 是唯一的排序杠杆，而且只有「最前」「最后」两档。

所以**不要用顺序来保证安全**。A 最初的修法就是「把行摆到正确位置」，做不到；改成让边界
只认 `by_person()` 之后，权限闸门至今仍 prepend 在最前、行清单一个字没动，两条判据同时绿。

C 和 E 两条**没有判据**，我没假装有：唯一能让 patch 失败的途径是某一行拒绝重挂，测试够
不到。缺口写在 `finish_stopped_native_turn` 的文档里。**一条测试够不到的错误分支，等于
从来没人跑过它** —— C 和 E 都是这么长出来的。

## 验证现状

| 范围 | 结果 |
|---|---|
| `atomcode-coding` | 586 绿 |
| `atomcode-harness` / `-tui` / `-kernel` / `-capabilities` | 全绿 |
| `atomcode-tuix` / `-cli` / `-daemon` | 3 红 **且都是既有红**（daemon webui 内嵌资源 ×2、tuix 配置面板搜索），在链上也红 |
| `atomcode-review` | 3 条超时，**既有**（把本分支改过的 kernel 文件退回分叉点仍超时；review 只依赖 kernel） |
| `-auth` `-config` `-plexus` `-telemetry` `-updater` `-codingplan` `-clix` | 712 绿 |

**按 workspace 成员逐个点名跑过一遍**——review 那三条超时之所以躲到收尾才发现，就是因为
前面十几轮验证的 crate 清单里一直没有它。**「全绿」只对跑过的 crate 成立。**
| `clix` | `cargo check` 过（按用户要求「不删，只保证能编译」） |
| `cargo fmt --all -- --check` | 0 |
| 差分棘轮 | 不变 |
| 真模型冒烟 | 纯文本 / 带工具 / resume / undo 四项在真网关上过，undo 那条带反证 |

判据在 `crates/atomcode-coding/tests/runtime_criteria.rs`（48 个场景 + 3 条独立判据，
只经 `CodingRuntime` 公开面）。**每条都摘掉被测代码证伪过一次**，反证记在各自 commit
里 —— 这是用户反复要求的纪律，新加判据照办。

跑测试用 `cargo nextest run -p <crate>`，**不要 `--workspace`**（见 AGENTS.md）。

## 真模型冒烟怎么跑（会卡住新人的一点）

默认网关 `https://llm-api.atomgit.com/v1` 要请求签名，签名 crate
`atomcode-codingplan-crypto` 是闭源 overlay，公共仓里只有 stub，裸 checkout 下
二进制会报 `gateway authentication is unavailable in this build`。

做法：从主仓工作区 `/Users/lichao/project/gitcode/ai/atomcode/crates/atomcode-codingplan-crypto/`
把 `Cargo.toml` + `src/` 临时拷进来，`cargo build -p atomcode --features codingplan-crypto`，
跑完立刻：

```
rm -rf crates/atomcode-codingplan-crypto/src/{master.rs,kdf.rs,versions}
git checkout HEAD -- crates/atomcode-codingplan-crypto/ Cargo.lock
cargo clean -p atomcode-codingplan-crypto && cargo build -p atomcode
```

**绝不提交、绝不推送，Cargo.lock 也不许带上它的依赖。** 本分支已确认干净。

## 接着做什么

1. **soak 再 push。** 34 个 commit 一次都没推过；用户要的是先在本地用一段时间。
   合回 `feat/plexus-plugin-architecture` 是纯 fast-forward（它已与 origin 同步）。
   push 之前把 `docs/collapse-dual-engine.md` 的「已知差异」那一节再读一遍 ——
   它列的是**决定保留**的差异（自动压缩策略、摘要锚点、边界按回合、工具指引归属、
   `max_continuations`、流重连次数、链式 logout 不清槽），不是待办。
2. **阶段二：行清单从常量变成数据。** 这是 09-14 交接里就定下的下一阶段 ——
   产品 A 与产品 B 换 TOML 不换 Rust。分支 `feat/external-assembly` 已存在，
   本线没碰它。
3. **自动压缩这条尾巴**（唯一一条「知道怎么做但没做」的差异）：链式是原地把旧工具结果
   改成桩，追加式日志里没有对应事实。真要对齐，先给会话日志加事件 —— 那是日志模型的
   决定，不该塞进本线。

## 几个位置

- 计划与全部步骤判据：`docs/collapse-dual-engine.md`
- 行清单：`crates/atomcode-coding/src/on_harness.rs`、`host_rows.rs`
- 装配入口：`crates/atomcode-coding/src/runtime.rs` 的 `mount` / `Mounted`
- 测试脚手架：`crates/atomcode-coding/tests/support/mod.rs`
- 存档与棘轮：`crates/atomcode-coding/tests/golden/differential/`、`gates/differential.baseline`
- 上一页（已过期，只当历史读）：`docs/handoff-coding-on-harness-2026-09-14.md`
