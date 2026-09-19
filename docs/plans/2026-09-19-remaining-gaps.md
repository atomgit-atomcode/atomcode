# 还差什么：2026-09-19 摸底与拍板

承接 [`docs/tui-replaces-tuix-plan.md`](../tui-replaces-tuix-plan.md) 的 M5.6 与 M6。
这份文档做两件事：把「新引擎 + tui 这套方案下还差什么」按**功能**列全，以及记下
2026-09-19 拍的九条决策。

摸底方法：五路独立测绘（tuix↔tui 深度对照、宿主契约能力面、删 tuix 卡点与死码、
配置/provider、多入口矩阵），结论逐条回代码复核过，**有四处推翻了测绘自己的判断**，
见第四节。

---

## 一、2026-09-19 拍板

| # | 问题 | 定的 | 要紧的理由 |
|---|---|---|---|
| 1 | 新机器上怎么加第一个 provider | **TUI 里给一条 OAuth 流**（扫码，全程不碰 api_key） | 「provider 增改删归配置文件」这条边界留了个洞：新机器上 TUI 里没有路能加第一个 |
| 2 | auth 的逻辑住哪 | **住 cli**。cli 实现端口、装配成 tui 的 plugin 行 | 照 `trait Settings`（`tui/settings.rs:539`）现成的形状：屏幕知道的产品信息都从缝过来，不从产品自己的服务里拿（0022 §3） |
| 3 | 先翻默认还是先补引导 | **向导做不完就不翻** | 翻默认只要一行，但翻了之后开箱不可用的锅要自己背 |
| 4 | 首启引导的深度 | **四步向导**（Intro / Language / Setup / Confirm + 扫码） | 对齐 tuix 的 `onboarding_wizard.rs`（2336 行）所覆盖的场景 |
| 5 | 向导的步骤是数据还是代码 | **数据**。tui 出通用 `Wizard` overlay，cli 喂步骤定义 | 「上游把本该是数据的东西写成常量」是下游 fork 90% 的定制成本（见开放性测绘）；`/setup` 将来直接复用 |
| 6 | 二维码谁画 | **tui 画**。cli 给 URL + code | 二维码是 presentation，归屏幕；URL / 轮询 / token 归 cli |
| 7 | `atomcode-auth` 拆两半这笔债 | **顺势拆**：Product 的那半（gateway_crypto / oauth / openrouter）与 Host 的那半（凭据文件读写） | 接的时候不拆，以后就是拆两遍 |
| 8 | `coding/src/team/`（1466 行）的去向 | **随 6.4 删 tuix 一起摘掉**，团队能力只留 harness 的 realm 版 | 新 tui 零消费团队事件；旧 team 面板是 0023 之前的形态 |
| 9 | `/plugin` 市场面板归 CLI 的判定 | **维持归 CLI** | 为它给 coding 开 capabilities 的 `plugin` feature，等于把市场/git 那套拉进 agent 进程，只为重复 CLI 已有的动作 |

**决策 2 + 决策 6 合起来划的那条线**：凭据、认证状态判定（有没有可用 provider、有没有本地
OAuth）、OAuth 轮询、步骤定义 —— 全在 cli；浮层机制、二维码渲染、按键 —— 全在 tui。
tui 今天**没有任何认证代码**，这条线是新划的，不是迁移。

---

## 二、顺序变了：引导先于翻默认

原计划 M6 是 6.1 自用 → 6.2 翻默认。决策 3 把引导插在前面：

```
A 引导（Wizard overlay + cli 的 auth 行 + auth 拆两半）
        │
        ▼
B ACP 命令投影（解开 6.3 的尾巴）／ C 便宜的修 ／ D /config 收尾   ← 可并行
        │
        ▼
E 6.1 自用 + 真模型冒烟  →  翻默认  →  soak
        │
        ▼
F 删 tuix + 摘 team/  →  daemon 迁契约  →  动 DriverCommand
        │
        ▼
G 体验项（/usage 画图、/resume 预览、输入历史落盘 …）
```

G 类七条**刻意排在翻默认之后**：自用会告诉我们哪几条是真缺的，预补出来的是「我以为要」的。

---

## 三、待办清单

判据规矩不变（0019）：**先写判据、摘掉被测代码证伪一次**，反证写进 commit；
`gates/tui-test-count.baseline` 随判据上抬。

### A. 引导（挡住翻默认）——**已完成，2026-09-19**

六条都落地了，顺序与当初写的不同：A6 先做，因为它是 A2 的触发时机。

- [x] **A1 tui 的通用 `Wizard` overlay**（决策 5）。步骤是数据：id、标题、已排好的
      body 行、四种要法（读一段 / 选一个 / 打一段 / 等宿主）。这个文件里没有
      onboarding、没有语言、也不知道屏上那个码是用来扫的。等待是它比 `Picker` 多
      出来的那件事：宿主留着 `Arc`，进行中 `say()`、落地 `resolve()`。
      顺带给 `Overlays` 开了 `finish(id, value)`——按名字关，因为登录迟到而人已经
      打开了别的东西时，按「当前开着的那个」关会关错。
- [x] **A2 tui 的 QR 渲染**（决策 6）。一张位图不是一串转义：颜色不走角色（扫码器
      要的是浅底深块），一格用 `▀` 装上下两个模块（否则码是宽高比 2:1，扫不出来）。
      终端不画单元格背景时一张都不画——半张码比没有更坏。
- [x] **A3 cli 侧的 auth 行**（决策 2）：`tui_onboarding`，四步（说清现状 / 语言 /
      扫码登录 / 看一眼结果），登录那步做的是 `atomcode login` 浏览器回来后的同样
      三件事减去打印。**这里一行都不能往 stdout 打。**
- [x] **A4 触发判定在 cli**：readiness 对 `NotConfigured` 点名 `onboarding`；行无
      条件挂上，跑不跑由启动时问宿主的那一答决定。
- [x] **A5 拆 `atomcode-auth`**（决策 7）：新 crate `atomcode-credentials` 拿走凭据
      文件、它的锁和文件里那点东西的形状；`atomcode-auth` 只剩协议（URL、轮询、
      换 token、gateway_crypto、openrouter）。方向由 `gates/layers.sh` 守着，三条都
      摘掉证伪过：存储反过来依赖协议、存储自己长出 HTTP 客户端、两半又合回去。
      **没有留 `pub use` 兼容面**——留了这拆就只是个 facade。
- [x] **A6 提交前预检**：`HostCommand::Readiness` 一问一答。这是「首启没引导」的
      根因：新桥只有提交后报错，旧 driver 协议的三个预检一个都没接。

**这一节顺带补的机制**（不在原计划里，是做的过程中缺的）：
- tui 的 `Repaint` 缝：循环外的活改了屏幕要能让它重画；登录轮询正是这种活，而恰恰
  在「等它」的那一步，下一次按键可能永远不来。
- `deliver()`：命令被「选中」和被「打出来」原本走两条路，选中那条把 `Outcome::Open`
  静默丢掉——选中项若要再开一个浮层，什么都不会发生。

### B. 解开 6.3 的尾巴（6.3 今天不能算完成）

- [ ] **B1 ACP 命令改投影**：`acp/commands.rs:52` 的 `ACP_COMMANDS` 15 条硬编码
      → 投影自 `AgentDescription` 的命令目录（tui 侧 `commands.rs:1447` 已是活投影，照抄）。
      `commands.rs:718` 那条把 15 条钉死的判据要一起改。
      **影响**：ACP 客户端今天拿不到 goal / loop / cd / team / worktree / review 等 ~30 条。

### C. 便宜且独立（各 30 分钟量级，随时插队）

- [ ] **C1 `/init`、`/setup`、`/guide` 三条登记** —— B1 里最便宜的三条，一直被记成已完成
- [ ] **C2 `CompactionFailed` 补 `NoticeKind`**：`kernel/src/session.rs:74` 加一种、
      `harness/plugins/handle.rs` 补映射，tui 走通用 NoticeBlock。今天
      `engine.rs:1368` 发出 → `cli/host.rs:504` 转成 `AgentEvent::CompactionFailed`
      → tui 只认 `Compacted`（`plugin.rs:1844`），失败那支掉进 wildcard，**全链路静默**
- [ ] **C3 删 `CodingRuntimeHandle::reprepare`**（`coding/runtime.rs:1627`，全仓 0 调用）

### D. `/config` 收尾（只剩两样）

- [ ] **D1 恢复默认**：要动契约 —— `host-api` 加 `ResetSetting`、`HostConfig` 加
      `reset_setting`、cli 调 `SettingSpec::reset`（`config/settings.rs:502` 机制已在）
- [ ] **D2 按 provider 变的动态 retry 项**：`config/settings.rs:566` 的
      `selection_retry_max_attempts` 在静态目录外，`settings()` 映射只认静态表

### E. 自用与翻默认

- [ ] **E1 真模型冒烟**：codingplan-crypto 临时拷贝流程，跑完还原，**绝不提交**
- [ ] **E2 翻 `Screen::Default`**：`cli/src/lib.rs:112` 一行，`:361-371` 的断言同步改，
      tuix 留 `--classic` soak

### F. 删（E 之后）

- [ ] **F1 删 tuix**：`main.rs` 的 26 处 `--classic` 分支（**唯一真卡点**）+ workspace member
      + `cli/Cargo.toml:21,34` 的 `distro-pm` 转发。daemon 那 4 行、`acp/commands.rs:5`
      都是注释，不是卡点
- [ ] **F2 随 F1 摘 `coding/src/team/`**（决策 8）：半活半死，`stop_all` 还绑在
      `runtime.rs:4229/8965/9033` 三处 `quiesce_current_agent` / `stop_current_agent` 上，
      单独砍会断
- [ ] **F3 daemon 迁两份契约** —— `DriverCommand` 能不能删的前置。它**今天不是死码**：
      daemon 的 `live_hub.rs` / `kernel_runtime.rs` / `native_live.rs` / `live_api.rs`
      仍大量挂载（tui 侧已有判据禁止碰它：`tui/tests/guards.rs:101`）
- [ ] **F4 `daemon/src/legacy_convert.rs`（4485 行）**：唯一的历史 core JSON 单向 importer。
      消费者归零前不动；**在任何报告里都不得称「格式已删除」**

### G. 体验项（翻默认后，按自用暴露的顺序排）

- [ ] G1 `/usage` 画图（数据全在 `UsageWindow`，tuix 侧 1782 行的图一根线没画）
- [ ] G2 `/resume` 预览 + 搜索 + 删除 —— 给 `overlay.rs:32` 的 `Overlay` trait 加 preview
      概念，不是单做一个特化 picker（否则 `/rewind`、`/model` 将来要同样的东西时再加一次）
- [ ] G3 输入历史落盘 —— 今天只在 `tui/host.rs:1298` 进内存 `moment.history`，重启即丢。
      按 0024「日志是唯一权威」，**从会话日志里折**，不另开历史文件（tuix 的
      `input/history.rs` 704 行单独文件是旧架构的做法）
- [ ] G4 goal / loop 常驻状态行（`Moment::autonomy` 存着，只剩画一行）
- [ ] G5 `/view` 搜索 + 语法色 ｜ G6 `/model` 分组与能力标注 ｜ G7 `/cd` 书签

### H. 记账与既有的债

- [ ] **H1 更新 `2026-09-18-tui-panels-and-commands-inventory.md`** —— 三处报高，见第四节
- [ ] **H2 追并发红**：`coding::mount_wiring::the_configured_datalog_records_the_turn`，
      线索在 `capabilities/src/datalog.rs:290`。「偶尔红一下」会训练所有人无视闸门
- [ ] H3 `atomcode-updater` 的版本号 `-N` 修订后缀被 `split('-').next()` 丢掉（`lib.rs:1107`）
- [ ] **H4 `atomcode-capabilities` 的测试编译在本分支上是红的**，且**不是本线改出来的**：
      `session/manager.rs:4769` 的一条测试调 `mgr.append_jsonl_line(…)`，而这个方法
      在 HEAD 的同一文件里根本没有定义（0 处定义、1 处调用）。也就是说
      `cargo nextest run -p atomcode-capabilities` 在这条分支上跑不起来，已经有一阵
      子没人跑过它了。多半是 0024 事件日志迁移时删了方法、漏了这条测试。
      2026-09-19 发现于 A5，未修——不在那条线的范围里，但**别让它继续烂着**：
      一个跑不起来的 crate 等于没有判据。

---

## 四、摸底纠正的记账（H1 的内容）

**三处报高**，都已回代码核过：

1. **5.6c 标 ✅ 是错的**。`/init`、`/setup`、`/guide` 在 tui / harness / coding 里
   **零命中**；`commands::register` 全仓只有 6 个调用点（`harness/plugins/capabilities.rs:114/132/476/766`、
   `coding/host_rows.rs:1813/1969`），内置 skill 只有 `atomcode-automation-recommender`
   一个种子。B1 里最便宜的三条从未落地。
2. **`/config` 差四样 → 实际差两样**。清单写于 09-18 晚，之后的提交补上了「预填当前值」
   （`cli/tui_settings.rs:100-135`）与「写完不关连续改」（`:132-140`）。设置目录本身是
   健康的：28 项 1:1 全覆盖，`tui_settings.rs:203` 用 `SETTINGS.len()` 钉住，加新设置不会漏。
3. **6.3「完成」有例外**：ACP 命令投影未做（B1）。计划页 6.3 的进度里自己写了「只做了
   一半」，这次独立测绘扫到同一处。

**一处推翻测绘自己的判断**（记下来免得后人照着修）：测绘报告说
`SessionEvent::PolicyIntervention` 的 doc comment 承诺「resume 后原样再问一次」而
UI 没实现。**核过是错的**：`harness/plugins/handle.rs:422` 有 fact → `AgentEvent`
的回放映射，tui 消费 live 事件即可，承诺是兑现的。

**契约面本身是干净的**：`HostCommand` 28 / `HostReply` 15 / `HostEvent` 3 全部已接；
`AgentCommand` 13 个里 tui 不发的 3 个是宿主内部注入命令，不是缺口。

---

## 五、不在这条线上

- 提交前必核 `Cargo.lock` 的私有依赖（AGENTS.md）。本次摸底时实测为 0。
- 版本号、发布配置不属于本线。
