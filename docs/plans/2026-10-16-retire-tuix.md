# Retire atomcode-tuix Implementation Plan

**Goal:** 删除 atomcode-tuix 目录及生产依赖，用调用点、命令表和行为测试确认替代能力。

**Architecture:** CodingRuntime 继续唯一持有 agent 生命周期，既有 tui_front/host/background
接入保持。删除经典界面展示 Session 和 RuntimeControl 适配，不搬回 runtime owner。
native 事件会话与 daemon 单向历史 importer 保留。旧 classic 仅作为带提示的输入别名，
没有第二界面、旧 runtime 或自动 fallback。

**Tech Stack:** Rust/Cargo，atomcode-tui、coding、host-api、config、i18n。

## 基线与工作边界

- 2026-10-16：release/v5.2.4，17af32515d1e4edac76a80194c5900f813d21c29。
- 独立 worktree `/tmp/atomcode-retire-tuix`，分支 `refactor/remove-tuix-v5.2.4`。
- 实现阶段保留主工作树未提交修改，先在独立分支验证；合入和推送须经用户后续授权。
- `cargo tree -i atomcode-tuix --edges normal` 只发现 CLI 直接消费。
- 删除前查明：--classic、ui.screen=classic、ANSI 能力不足 fallback、panic restore、
  启动 Session::from_catalog_view 投影、遥测 session_id、classic runtime adapter。

## Task 1: 固定旧功能清单

从待删 command registry 提取命令名、别名，记录到测试 fixture；在默认 TUI 装配后验证
这些名字的已有处理方（不是仅查 MOST_USED 或空 CLASSIC_ONLY）。按缺口调整，不默许消失。
列出已有行为判据：模型/会话恢复、后台、审批/取消、共享、登录/插件/MCP、升级。

## Task 2: 清除经典界面接入

删除 crates/atomcode-tuix、workspace default-members、CLI 依赖/feature forwarding；
删除 main.rs 旧 Session 投影、runtime adapter、classic run 分支；遥测从 native id 取值。
默认界面直接复用既有 CodingRuntime。公开现有终端 emergency restore 给 CLI 调用，
不新建恢复状态 owner。不支持 ANSI 时在 runtime 启动前显式失败。

旧 --classic 隐藏并提示弃用，旧配置 classic 读取时规范为 rows 并给出启动提示；
设置页不再提供 classic。保留 --tui 兼容，不保留 engine 开关。
删除空 elsewhere 行与它的无效 classic 提示测试，保留实际 /upgrade 参数行为测试。

## Task 3: 构建、测试、文档收口

更新 Cargo.lock 中合法删除项，核对没有私有 crypto 依赖；新/旧 session 文件不重写。
历史文档留作历史，当前说明修正默认/经典界面事实；修正维护脚本真实指向。
记录被删测试及默认界面对应判据；不把注释里的 tuix 名字误认成生产依赖。

## Task 4: 验证与结果

cargo fmt --all -- --check；cargo nextest run -p atomcode（不可用时 cargo test）；
受影响 tui/config/i18n 测试；bash gates/compile.sh；cargo tree 无 tuix；
对目录、生产调用、fallback、Cargo.lock 搜索归零。CLI/headless/ACP 编译，恢复、
审批取消、后台和共享跑现有可执行判据；Windows 原生终端实测单独标未覆盖。
记录功能矩阵、实际删除项、仍留 importer/输入别名和达到的退役状态。

## 实施结果与功能验证矩阵

目录 `crates/atomcode-tuix/` 已实际删除；CLI 依赖、feature 转发、workspace 默认成员、
classic run、两个 runtime adapter、旧 Session 展示投影、旧终端恢复调用均已删除。
空的 tui_elsewhere 行及其四个提示词条删除，不保留引导人进入已不存在界面的 handler。
Cargo.lock 仅删除 69 行：tuix 与其独占的 arrayvec/urlencoding/vte 包，无版本更新，
无 hmac/hkdf/zeroize_derive 等私有解析结果。

| 功能/旧依赖 | 当前实现或处理 | 已执行的判据 |
| --- | --- | --- |
| 60 个内置命令 + new/exit 别名 | tui_front 实际装配的屏幕/宿主/runtime catalog | tui_front::every_retired_command_has_a_live_replacement；模型调用次数为 0 |
| 会话恢复、历史、删除、rewind | native SessionManager + host；不再加载 tuix Session 镜像 | host 50 项，包括 a_stored_session_is_listed_and_resumed_with_its_history、a_resumed_session_holds_every_fact_it_committed |
| 模型切换、登录/退出、配置 reload | host + tui_providers/login 等既有行 | host 的 switch/reload/sign-in/out 测试；CLI lib/bin 438 项 |
| 后台任务、提问、切换、返回结果 | background + tui_bg | tui_bg 38 项全部通过 |
| 审批、请求、取消、终态 | 既有 FrontEnd/HostConnection/CodingRuntime | host 的 question/stop/undo，acp_end_to_end 15 项（含 v2 approval、elicitation、resume） |
| /webui、/sync、/app、/desktop | tui_share + daemon live hub，已在当前屏幕装配 | 62 项命令检查；host::a_shared_session_is_the_same_conversation_for_a_viewer；sharing unit 测试 |
| MCP/skills/plugin | 原 runtime/宿主行，无 tuix 引用 | host 的 MCP manage/reload、技能 reload；live catalog 全命令判据 |
| /upgrade、/schedule、/proxy、引导 | 对应现有 CLI 行 | tui_front 13 项，包括升级参数拒绝、代理持久化、定时任务列表、旧 welcome 别名 |
| headless -p / --continue | 直接使用 native runtime | 独立临时 HOME、本地假 SSE 服务：fresh 和 continue 都 exit 0；第二次 main 请求含第一轮原文 |
| --classic 与 ui.screen=classic | 隐藏弃用别名/可读配置值，提示后只打开当前 UI | CLI 参数单测、config 读取测试；Mac PTY 实际启动和 Ctrl+D 退出为 0 |
| Windows 不支持 ANSI 的控制台 | runtime 创建前显式报错，提示 Windows Terminal 或 -p；无回退 | 全目标编译；Windows 原生实测未覆盖 |
| panic/普通退出恢复 | 调用 atomcode-tui 既有 emergency_restore；仍是同一 SCREEN_HELD owner | 原 TUI 测试、PTY 退出；未主动在真实终端注入 native crash |

命令表判据只确认原名称/别名有实际处理方；不宣称旧经典界面的逐像素排版、全部键位、
参数以及网络服务行为与当前 UI 相同。经典渲染和它的交互差异随目录一起移除，例如
classic 的 /copy 默认取最后代码块不再适用；当前 /copy 语义不变（原文回复），文档已修正。

### 验证结果

- `cargo check -p atomcode --all-targets` 通过。
- `bash gates/compile.sh` 通过（含 workspace all-targets、feature 编译和阴性对照）。
- `cargo fmt --all -- --check`、`git diff --check` 通过。
- CLI：568 项测试覆盖通过。首次 `cargo test -p atomcode --tests --no-fail-fast` 的
  host binary 存在共享 ATOMCODE_HOME 的并发失败/挂起；停止该 binary 后，其余 binary
  均完成。`cargo test -p atomcode --test host -- --test-threads=1 --nocapture` 50/50
  全通过。未修改这些测试或以重试隐藏其失败；.config/nextest.toml 已记录此类进程环境
  隔离要求。本机无 nextest，官方预编译下载超时，故采用上述串行替代。
- `cargo test -p atomcode-tui --tests -q`：1502 + 178 + 7 + 5 = 1692 项全通过。
- `cargo test -p atomcode-config -p atomcode-i18n --lib -q -- --test-threads=1`：309 + 67 项全通过。
- 真实进程的本地假 SSE fresh/continue 检查和 Mac PTY 启退通过。所有测试 HOME 位于
  临时目录；未使用用户真实会话。未验证真实 OAuth/升级下载、手机/浏览器、Windows 原生 UI。
- `cargo tree -i atomcode-tuix` 返回 package not found；Cargo manifest/lock 无该包。

### 删除的测试、仍保留的兼容与四态

- 删除 tuix 全部单测和其 plugin_integration/use_command_oracle/smoke；旧 classic 选择
  单测及空 elsewhere 测试删除，替换为真实运行 catalog 的 60+2 入口判据与兼容参数测试。
  /upgrade 非法参数拒绝测试保留；native runtime、host、ACP、TUI 测试及 golden 不重录。
- 旧 UI backend 已达到第 4 态：实现、消费者、生产接口和回退均清除，没有另一 owner。
- 保留的输入兼容明确可见：隐藏 --classic 参数、Screen::Classic 读取值仅映射当前 UI，
  不能运行旧实现；不宣称这两个输入已经删除。--tui 保持有效。
- daemon 历史 core JSON 单向 importer、native 事件存储格式保留且未改动；不宣称旧数据格式
  已删除。daemon 的 ATOMCODE_TUIX_LOG 环境名仍用于独立诊断 trace，与旧 UI 无代码依赖。
- 实现阶段在 refactor/remove-tuix-v5.2.4 worktree 完成；后续用户已授权审计通过后合入并推送 release。
  审计未发现阻塞问题，distro-pm 非默认 feature 全目标检查通过；发布后唯一待办是 Windows 实机验证界面与终端恢复。

### 最终基线更新

删除提交已在本地更新到 release/v5.2.4 的 84690d30c（包含随后提交的 rewind 修复和
/worktree done 文案修正）。只在生成的两个搜索索引发生冲突，依据自动合并后的中英
HTML 重新生成索引，保留新 worktree 说明及本次 classic /copy 文档修正。
更新后再次通过完整编译闸门、host 50 项、tui_front 13 项、TUI 1502 项单测与 i18n
库单测。实现阶段主工作树未提交修改和 release 分支均未被改写；该记录之后的合入由用户单独授权。

### 提交前审计

核对了唯一 runtime owner、native 会话及遥测 session id、panic/普通退出恢复、旧配置
别名、ANSI 不足时显式失败、命令替代和 Cargo.lock 删除项，未发现阻塞问题。
`cargo check -p atomcode --all-targets --features distro-pm` 通过；updater 的包管理器
升级策略保持，未保留 tuix feature 入口。远端 release 基线为 84690d30c。
Windows 原生终端、真实 OAuth、手机/浏览器及在线升级仍不属于已执行的实机验证。
