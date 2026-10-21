# 开发验证的历史依据与实验记录

本文件从 `821a2f337` 的 `AGENTS.md`「验证与交付」节原文迁出，保留历史证据和操作背景；时间、数字和当时的仓库状态不代表当前快照。当前必须遵守的规则见 [AGENTS.md](../AGENTS.md#验证与交付)。涉及编译门、私有 overlay / lockfile、测试加速或 profile 变更时，先阅读本文件对应小节，再按当前代码核实。

## 历史原文


- 修改过程中运行最小相关测试；一个逻辑单元完成后运行受影响 crate 的测试。

### 合之前跑 `bash gates/compile.sh`（按 crate 跑测试看不见的那类腐烂）

**「按 crate 跑 `-p <crate>`」这条规矩，结构上看不见一类腐烂：判据编译不过。**
2026-09-23 一天之内撞到两处，加上更早的两处，是同一种烂法：

| | 怎么烂的 | 为什么按 crate 跑看不见 |
|---|---|---|
| `atomcode-host-api` | 契约加了三个变体，测试里逐变体列举的两处 `match` 没跟着补 | 没人单独跑过这个 crate 的测试 |
| `atomcode-capabilities` 的 `session/snapshot.rs` | 被测代码把字符串换成枚举，测试还在 `.contains(...)` | **`session` 挂在非默认 feature 下**，`-p atomcode-capabilities`（默认 `provider + tools`）根本不编译那个文件 |
| `kernel/tests/conformance.rs` | `ToolMiddleware::after` 签名漂移 | 躺了一周，随 v5.0.9 发了出去 |
| `capabilities` 的 `append_jsonl_line`（H4） | 写的那半删了，测试还在调 | 同第一行 |

第二行是关键：**它不是谁偷懒，是那把尺子量不到**——consumer 开着 feature 编译的东西，
按 crate 跑的默认 feature 编译不到。

`cargo check --workspace --all-targets` 正好量这个，**CI 里本来就有而且是阻塞的**
（`check.yml`）。它没挡住上面这些，是因为 **CI 只在 push 时跑，而这套流程会在本地
连着合好几天才推一次**——那两处腐烂所在的提交，一次都没被推上去过。所以这个脚本不是
另立一套判据，它就是把同一条闸门搬到本地、**在合之前**跑：

```bash
bash gates/compile.sh     # 主检出热态 20.7s(2026-09-23 实测)
```

`check` 不做代码生成也不链接，所以它远比 `nextest run --workspace` 便宜——后者要构建
91 个测试二进制、一次吃掉 8.5GB 磁盘（本节下面记的那次把磁盘顶到 99%、`target/` 整个
没了）。**不要把这个闸门改成 `run`。**

### 提交前必须核 `Cargo.lock`（本机会反复污染它）

**前提（2026-10-12 实测补）**：污染只在**实现版 overlay 在场**时发生。本 clone 检出的是公开 stub
（`crates/atomcode-codingplan-crypto/Cargo.toml` 的 `[dependencies]` 为空），连跑多轮 `cargo test` 后
清掉标志核 `git diff Cargo.lock` 为空 —— 也就是**这棵树构建不污染 lock**。下面的止血因此只在实现版
在场时才需要；本机常态（跑开发/测试）用不着标志位，挂了反而会挡住切换（见下面「三件要知道的」）。

**本机每跑一次 `cargo build`，`Cargo.lock` 都会被写进一批公开仓库不该有的私有依赖。**

成因：`crates/atomcode-codingplan-crypto/` 在公开仓库里是**只有签名没有实现的 stub**，而本机放的是**实现版**——它的 `Cargo.toml` 声明了 `hmac`/`sha2`/`hkdf`/`zeroize`/`subtle`。根 `Cargo.toml` 的 `workspace.members = ["crates/*"]` 是 glob，把这个目录收成了 member，cargo 解析 workspace 时把这些依赖写进 lock。（**把它移出 member 名单并不解决问题**——真正让它们进 lock 的是 `atomcode-auth` 那条 optional path 依赖，见本节末尾的实测。）

证据链（2026-09-15 实测）：`6504f4ca` 删掉那 41 行 → `8fb2eaf9` 又加回来 42 行（同一批 + 1 行合法的 `atomcode-coding`）→ 手工剔干净后跑一次 `cargo build -p atomcode-tui --lib`，`Cargo.lock` 的 md5 立刻变回含私有依赖的版本。

**所以「剔掉那几行再提交」是一次性的，不是修好了。**

#### 本机设一次，之后不用再手工剔（2026-09-20）

```bash
git update-index --skip-worktree Cargo.lock
```

它改的是 `.git/index` 里那个条目上的一个标志位（设完 `git ls-files -v Cargo.lock` 打 `S`，普通状态是 `H`），意思是「别看工作树里的这个文件」。于是本机构建照常把私有依赖写进磁盘上那份 lock，而 `git status` / `git add -A` / `git commit -a` 全都跳过它，推上去的永远是仓库里那份干净的。

**不改仓库任何文件**：`.gitignore` 没动，`git config` 没动，lock 的内容也没动。验证方式是设完之后**故意**跑一次 `cargo build` 让它重新变脏，再看 `git status` 是不是空的——空才说明是「它脏了但 git 不看」，而不是「我刚剔干净了」。

三件要知道的：

- **只对这个 clone 生效**（那个位存在 `.git/index` 里）。换机器、重新 clone、`.worktrees/` 下的每个 worktree 都要各设一次。
- **它挡住的是「任何改动 lock 的工作树操作」，与工作树脏不脏无关**：`git checkout` / `git switch` / `git pull` 在目标分支的 lock 与当前不同时（`main` 与各 release 分支常态差十几行）一律被拒，报的正是「您对下列文件的本地修改将被检出操作覆盖：Cargo.lock」。（2026-10-12 实测：标志为 `S` 时切换被拒；清掉标志后 `git diff Cargo.lock` 为空、切换立即成功。）修法：`git update-index --no-skip-worktree Cargo.lock` → 切换/拉取 → 需要时再设回；**第二步不必 `git checkout -- Cargo.lock`**（没真脏时清掉标志就够了，不丢任何东西）。**`assume-unchanged`（`h`）不是替代品**——它能放行切换，但会被 checkout 自己清掉（实测设 `h` 后两次切换就变回 `H`）；**要守门又不挡切换**就把守门挪到提交边界（`.git/hooks/pre-commit` 拦下暂存里含 `hkdf` / `hmac` / `zeroize_derive` 的 lock，此时不需要任何标志位）。
- 撤销就是 `--no-skip-worktree`。

没设这个位、或者不确定设没设的时候，push 前照旧核一遍：

```bash
grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock   # 必须是 0
```

（判据只数这三个名字；仓库版 lock 里本来就有合法的 `subtle` / `zeroize` 各 1 个，别把它们加进判据 —— 2026-10-12 实测踩过一次假阳性。）

非 0 就用干净基线重来（`git show <干净 commit>:Cargo.lock > Cargo.lock`，再把该提交之后**合法的**依赖变化补回，例如 `atomcode-tui` 新增的 `atomcode-coding`），确认 `git diff Cargo.lock` 只剩你要删的那些行。**不要**用 `cargo update` 或 `cargo generate-lockfile` 去「修」它——它们只会把私有依赖写回来。

#### 为什么根治不了，以及原来写在这儿的判断是错的（2026-09-20 实测）

**根因不是「它是 workspace member」**：默认 feature 下 `cargo tree -i hkdf` 报 `did not match any packages`——依赖图里根本没有它，可 lock 里有。**`Cargo.lock` 记的是所有 optional 依赖的解析结果，不管 feature 开不开**，而 `atomcode-auth` 用 `optional = true` 的 path 依赖引着 `atomcode-codingplan-crypto`。只要这条 path 依赖还在，它的依赖就必然落进 lock。

原来这一段写的是「`[workspace.exclude]` 看着对症，但 `atomcode-auth` 用 path 依赖引它，排除会打断公开侧的 feature 门」。**两半都不成立**，实测如下（改根 `Cargo.toml` 加 `exclude = ["crates/atomcode-codingplan-crypto"]`，跑完还原）：

| 断言 | 实测 |
| --- | --- |
| exclude 能让 lock 干净 | ✗ `cargo metadata` 之后照样是 3 行——它只是说这个目录不是 member，挡不住那条 path 依赖 |
| exclude 会打断公开侧的 feature 门 | ✗ `cargo check -p atomcode-auth --features codingplan-crypto` 退出码 0——path 依赖本来就可以指向 workspace 之外 |

所以 exclude 这个方向不是「有代价的解法」，是**无效**：别再照着它试第二遍。真要根治，得动的是那条 optional path 依赖本身（私有 overlay 以什么身份进入依赖图），而不是 workspace 的成员名单。在那之前，上面那个 `skip-worktree` 是本机这一侧的止血。

**不要把 `Cargo.lock` 写进 `.gitignore`**：它已经 tracked，写进去也不生效（得 `git rm --cached`，那是把它从仓库删掉）；而 `.github/workflows/build.yml` 里五个平台的 `cargo build --release` 正是最需要它的场景——没有 lock，两次发布可能拿到不同的传递依赖版本，而且出事后无法复现当时的依赖图。

### 测试与构建命令（2026-09-13 实测定标，勿凭直觉推翻）

- 跑测试用 `cargo nextest run -p <crate>`，不用 `cargo test`。`cargo test` 一个 test binary 跑完才跑下一个，nextest 跨 binary 进程级并行。配置在 `.config/nextest.toml`。
- **但 nextest 不是无条件更快，收益与该 crate 的 test binary 数量成正比**（2026-09-13 实测，同一套测试）：

  | crate | test binary 数 | `cargo test` | nextest | + 虚拟时钟 |
  | --- | --- | --- | --- | --- |
  | kernel（305 个测试） | 28 | 570s | 31.4s | **0.64s** |
  | harness（280 个测试） | 22 | — | 14.8s | **4.04s** |
  | tui（397 个测试） | 1（几乎全是 lib 单测） | 1.63s | 2.12s | — |

  tui 反而略慢：只有一个 binary，`cargo test` 的串行短板压根不发作，显出来的是 nextest 的进程启动开销。所以判断依据是 binary 数量，不是"nextest 更快"这句话本身。改门或改脚本前先数一下 `ls crates/<c>/tests/*.rs | wc -l`。
- nextest 不跑 doctest。全仓 24 个 doc 代码块，带 `text`/`json`/`toml` 语言标记和 `ignore` 的都不跑——数 ``` 的个数会高估可跑数量，`atomcode-tui` 的 2 个块实测就是 0 passed / 2 ignored。真要确认某个 crate 丢没丢覆盖，跑 `cargo test --doc -p <crate>` 看 `test result` 那行，别靠 grep 估。
- 新增 async 测试，只要被测路径上有超时、退避或重试（provider retry、rate limit、stream timeout、conformance check timeout），一律写 `#[tokio::test(flavor = "current_thread", start_paused = true)]`，让 `tokio::time::sleep` 走虚拟时钟。裸 `#[tokio::test]` 会真等墙钟时间——sleep 往往不在测试里而在被测的生产代码里（如 `agent.rs` 的重试退避），grep 测试文件是看不出来的。kernel 曾有 11 个这样的测试，最慢单个 27s，合计占整套 305 个测试时间的 97%。范式见 `crates/atomcode-kernel/tests/tool_batch.rs`。
- 虚拟时钟不会让测试变空过：断言打在可观测结果上（重试是否耗尽、approval 问了几次），逻辑没跑到就会红。但 `start_paused` 要求 current_thread，改之前确认该测试不依赖真并行（无 `std::thread`、`spawn_blocking`、`block_on`）。
- **转之前先看该测试怎么断言时间，这里有两个陷阱**（2026-09-13 在 `harness/tests/recovery.rs` 踩到）：
  - 上界断言 `started.elapsed() < Duration::from_secs(20)`（"必须有界"）：若 `Instant` 来自 `std::time`，虚拟钟下真实时间几乎不走，断言变成恒真，判据当场作废。修法是把 `Instant` 换成 `tokio::time::Instant` —— 未暂停时等价 std，暂停时跟随虚拟钟，于是"超时被误写成 30s"这类退化仍然抓得住。
  - 下界断言 `run.elapsed >= Duration::from_millis(900)`（"必须真等过"）：**这种测试不要转**，虚拟钟会让它失去意义甚至判红。`recovery.rs` 的 `a_real_retry_after_is_honoured_over_any_guess` 和 `concurrency.rs` 就是这一类，它们至今仍真等墙钟时间，是刻意的。
  - 一句话：只转慢榜上的测试，且先 grep 一遍 `elapsed`。
- 按 crate 跑 `-p <crate>`，不要随手 `--workspace`。9 个 consumer 各开不同的 `atomcode-capabilities` feature 子集，resolver v2 刻意不跨 crate 统一 feature，于是这个 95k 行的库会被编 19 次（单份 28–138MB）；全量构建 91 个 test binary 一次吃掉 8.5GB 磁盘。2026-09-13 一次全量 nextest 把磁盘顶到 99%、swap 耗尽，test binary 被 SIGKILL，整个 66GB `target/` 随后消失。
- 不要给 dev profile 加 `[profile.dev.build-override] opt-level = 3`。已做过 A/B，结论为负：冷 check capabilities 从 21.7s 变 42.0s（多烧 144s CPU 把 syn/serde_derive 编成 -O3），稳态 CPU 无差异（1.95s vs 2.00s）。全仓 derive 密度约每 280 行一个，这笔一次性成本摊不平。
- 调试信息分两档：依赖 `[profile.dev.package."*"] debug = 0`（连行号也不留，panic 仍有符号名），workspace 内自己的 crate `[profile.dev] debug = "line-tables-only"`（**2026-09-18 用户决定改的**，此前是默认的 `debug = 2`）。原来那条规矩写的是"不要改成对整个 dev profile 生效，那会连自己代码的单步调试一起砍掉"——代价确实是这个：backtrace 的函数名与行号还在，失去的是调试器里看局部变量和单步。要单步某个 crate，临时加 `[profile.dev.package.atomcode-xxx] debug = 2` 只编那一个包。**收益尚未 A/B 量过**：改它的动机是 target 体积（量到过一个 worktree 56GB，其中 deps 34GB、incremental 21GB），但 macOS 上 debuginfo 留在 `.o` 里、不进最终二进制（见下一条 strip 的实测），所以省的是 `deps/` 与链接时间，不是二进制大小。谁先做出 A/B，把数字补在这里。
- 已排除的加速手段，别重复试：换链接器（macOS 已是 Apple 新 ld，`rust-lld` 不支持 mach-o）、sccache（不同 feature 组合是不同缓存键，救不了 19 个变体，还要额外磁盘）、砍 test binary 的 debuginfo（`profile.test` 的 `line-tables-only` 早已生效，strip 一个 98MB 的 binary 只掉 10MB）。
- `cargo check` 从来不是瓶颈：冷编译 capabilities 21.7s，稳态重编 2.5s。遇到"编译慢"先分清是 check 慢还是 test 慢，再分清是 CPU 饱和还是 I/O 等待（对比 `/usr/bin/time -p` 的 real 与 user+sys）。
- 仅当变更跨 crate、公共协议、持久化格式、workspace 依赖或构建配置时，运行相关 workspace 检查。
- `cargo test` 已完成相同编译验证时，不紧接着重复运行 `cargo check`；代码和环境未变化时不盲目重跑失败命令。
- 纯文档、注释和格式修改可以不运行测试，但必须检查 diff 和文档内部一致性。
- 交付前运行 `cargo fmt --all`；`cargo fmt --all -- --check` 必须退出 0。这是 `check.yml` 的 lint job 里唯一的**阻塞**门（clippy 仍是 report-only）。全仓已经 fmt-clean，所以现在跑它只会动你自己改过的文件——这正是它能当门的前提，别再让它积债。
- 纯格式化改动单独成一个 commit，不要混进功能 commit：全仓 fmt 会碰上百个文件，混在一起的 diff 没法评审，也会让 rebase 冲突面无谓地扩大。
- 若 `--check` 在你没动过的文件上报错，多半是 rustfmt 在 match arm 的长字符串字面量上不收敛（`cargo fmt` 不改、`--check` 照报）。按块形式重写该 arm、字面量用 `\` 续行；不要给这道门加回 `continue-on-error`。
- runtime/协议迁移按实际影响覆盖 CLI、TUI、daemon、headless、background、session resume、approval、cancel、provider reload 和持久化兼容；不受影响的入口无需机械重复验证。
- 退役任务的最终说明必须包含：验证基线、达到的四态、实际删除项、仍保留的 importer/legacy surface、测试结果和唯一下一步。
- 普通功能或修复只需报告行为变化、风险、验证结果和已知未验证范围，不强制套用迁移报告模板。
- 未删除旧类型、handler、依赖或 fallback 时，必须明确“尚未退役”，不得以功能可用替代完成声明。
