# 选项式审批面板(Selectable Approval Panel)设计

- 日期:2026-07-13
- 状态:已批准(待写实现计划)
- 范围:`crates/atomcode-tuix`(渲染 + 输入 + 状态)。tuix 以下(决策枚举、审批 plumbing)不改。

## 背景 / 动机

当前工具审批要求用户**盲敲 `y`/`a`/`n`**:审批提示以 `UiLine::ApprovalPrompt` 塞进正文,渲染成一行 `▶ Waiting for approval: Bash(rm -rf): Y↵Allow A Always N Deny`,由 `handle_approval_key` 读单键映射到决策。问题:①不直观(得记 y/a/n 各是什么);②提示在正文里、决策后要靠脆弱的 `pop_approval_prompt` 擦除正文行(已多次出 bug、被硬化过)。

参照 opencode / codex / 用户提供的 DevEco 截图:它们都用**可选项式**审批(方向键/Enter 选,选中项高亮),钉在底部一块面板里。本设计把 atomcode 的审批改成同类:**footer 区的竖排可选列表**。

## 已定决策(brainstorm 拍板)

1. **排布**:竖排列表(像 codex),每选项一行,选中行 `▸` 前缀 + 反色(atomcode cell 无 bg,用 reverse 模拟填充按钮)。
2. **交互**:纯键盘(v1 不做鼠标,保留终端原生滚轮/选中复制)。`↑/↓` 移动(环绕)、`Enter` 确认、`Esc` = Deny、保留 `y/a/n` 快捷键(直接选中+确认)。
3. **位置**:统一固定 —— footer 区一块面板(在 footer 堆叠里,输入框上方),带左侧 warning 色 `▌` 强调条,自包含(重述工具+命令)。
4. **选项**:仅现有三个决策 —— `Allow once` / `Always allow` / `Deny`。默认选中 `Allow once`(与现在 Enter=Allow 一致)。**不加** deny-with-feedback。
5. **纯前景 + reverse**,无 bg;`▌`/`▸`/`⚠` 有 ASCII 回退;plain(管道)渲染器保留非交互文本回退。

## 架构

### 组件划分

| 单元 | 职责 |
|---|---|
| `ApprovalPanel` 状态(state.rs) | 承载一次审批的:工具名、命令/detail、选项列表、当前选中 index。仅 `UiPhase::Approval` 期间有值。 |
| 纯选项构造器(新 pure fn) | 从审批请求(工具名 + 审批要求)构造 `Vec<ApprovalOption>`(label + 对应 `PermissionDecision` + 加速键)。可单测。 |
| footer 渲染(retained.rs) | 在 `paint_footer` 堆叠里新增审批区:左条 `▌` + 头(`⚠ <Tool> 需要授权` / detail)+ 竖排选项(选中 `▸`+reverse)+ 提示行。复用现有 cell/宽度/主题机制。 |
| 输入(event_loop.rs) | 扩展 `handle_approval_key`:`↑/↓` 改选中、`Enter` 确认选中项、`Esc`=Deny、`y/a/n` 直接选中+确认。复用菜单 up/down 的环绕逻辑。 |
| plain 渲染回退(plain.rs) | 管道/非 TTY 下渲染成静态文本选项(非交互),沿用现有文本审批回退风格。 |

### 数据类型

- `enum ApprovalOptionKind { AllowOnce, AlwaysAllow, Deny }`(或直接复用/映射到 `PermissionDecision`)。
- `struct ApprovalOption { label: String, decision: PermissionDecision, accel: char }`(accel:`y`/`a`/`n`)。
- `struct ApprovalPanel { tool: String, detail: String, options: Vec<ApprovalOption>, selected: usize }`,置于 `UiState`(仅审批期间 `Some`)。

### 「Always allow」标签(动态)

atomcode 的 `AllowAlways` 语义取决于审批要求(`atomcode-core::tool::ApprovalRequirement`):
- `RequireApproval` → 授权**整个工具**本会话(`grant_session(tool)`);
- `RequireApprovalScoped { scope }` → 只授权**该 scope**(`grant_session_scope(scope)`)。

**v1 标签**:`Always allow <tool>`(如 `Always allow bash`)—— 工具名始终可得、清晰、不会误导(整工具场景精确;scoped 场景也正确表达"这类调用不再问",只是不列出具体 pattern)。
**细化(plan 阶段核实后可选)**:若 `AgentEvent::ApprovalNeeded` 携带了 scope,scoped 场景改显更精确的范围提示(像 DevEco 的 `- path\*`)。不携带则保持工具名。

### 数据流

```
atomcode-core TurnRunner
  └─ AgentEvent::ApprovalNeeded { tool_name, reason, call, snapshot }
       ↓ (现有事件流)
atomcode-tuix event_loop 的 ApprovalNeeded handler
  1. (不变)bypass 检查 / 落盘 snapshot / push `▸ Tool(detail)` 正文行(永久记录)
  2. (改)不再 push `UiLine::ApprovalPrompt`;改为:
       state.approval_panel = Some(ApprovalPanel { tool, detail, options=build_options(...), selected:0 })
       state.on_approval_needed(...)  // phase → Approval(不变)
  3. 重绘 footer(面板出现)
       ↓ 用户 ↑↓ 选 / Enter 确认 / Esc 拒绝 / y a n 快捷
handle_approval_key
  - ↑/↓:改 approval_panel.selected(环绕),重绘 footer
  - Enter:取 options[selected].decision → deliver_approval(现有)
  - Esc:Deny → deliver_approval
  - y/a/n:定位对应 option → 确认
  → state.approval_panel = None;on_approval_resolved()(phase 回 Streaming)
  → footer 不再渲染审批区(面板消失,无需擦正文)
       ↓
deliver_approval(现有:local=cmd_tx / sync=LiveSession.approve)→ PermissionDecision(现有)
```

**关键简化**:审批从正文移到 footer 后,**删除 `pop_approval_prompt` 及其 `approval_block_rows` 记账**(不再擦正文行)。`▸ Tool(detail)` 正文行保留;审批面板随 phase 消失。

### footer 堆叠位置

现有堆叠:`todo 面板(顶)/ 菜单 / goal|loop / 状态 / 输入`。审批面板与菜单/todo 同族,插入一处专用槽(建议:菜单同层或紧邻,审批期间独占——审批时不会同时有斜杠菜单)。高度计入 `max_input_rows` 预留(与 todo 面板同法),绝不 overflow。

## 错误处理 / 边界

- **Ctrl+C 期间审批**:保留现有语义(首次 = Deny 当前 + 武装退出确认;窗口内再次 = 退出)。
- **bypass**(`--dangerously-skip-permissions`):不变,直接自动批准,不显面板。
- **sync/daemon vs local 决策投递**:不变(`deliver_approval` 两分支照旧)。
- **窄/矮终端**:面板走 `max_input_rows` 预留让位输入框;选项 ≤3 行 + 头 2 行 + 提示 1 行,极矮时靠现有 footer 收缩规则降级(不 overflow)。
- **审批期间被取消/切会话**:`approval_panel` 在 turn 取消/错误/会话切换/`/clear` 路径清空(与 `on_approval_resolved`/reset 一致),防残留(参照 todo 面板 `active_todos` 的清空点)。

## 测试

- 纯 `build_approval_options(tool, requirement) -> Vec<ApprovalOption>`:三选项、label(含动态 Always 标签)、accel、decision 映射。
- 选中移动:`↑/↓` 环绕、边界。
- 键映射:Enter→选中项决策、Esc→Deny、y/a/n→对应项。
- 渲染:vterm 快照(头 + 选中行 `▸`+reverse + 提示行 + 左条);ASCII 回退(`|`/无 `▸`)。
- plain 渲染:静态文本选项。
- 回归:审批从正文移走后,`pop_approval_prompt` 相关测试删除/改写;`▸ Tool` 正文行仍在;审批解决后面板消失、输入框可用。

## 受影响文件

| 文件 | 改动 |
|---|---|
| `crates/atomcode-tuix/src/state.rs` | `UiState.approval_panel: Option<ApprovalPanel>` + 清空点;`ApprovalPanel`/`ApprovalOption` 类型 |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | `ApprovalNeeded` handler 改设面板状态(不 push ApprovalPrompt);`handle_approval_key` 扩展方向键/Enter/Esc/y-a-n;`build_approval_options` 纯 fn |
| `crates/atomcode-tuix/src/render/retained.rs` | `paint_footer` 新增审批区渲染 + 高度;**删除** `UiLine::ApprovalPrompt` 渲染 + `pop_approval_prompt` + `approval_block_rows` |
| `crates/atomcode-tuix/src/render/plain.rs` | 审批文本回退(非交互) |
| `crates/atomcode-tuix/src/render/mod.rs` | 视需要删除/调整 `UiLine::ApprovalPrompt` 变体(若无其他用途) |

## 备选方案(已否决)

- **横排按钮**(opencode/DevEco 样式):用户选了竖排(选项文字可更描述性)。
- **保留在正文下方**(现状):用户选了 footer 固定位置(交互选择天生属于 footer;顺带清掉脆弱的 pop 逻辑)。
- **鼠标点选**:v1 不做(全局鼠标捕获会破坏终端原生滚轮/选中复制;键盘选项已解决核心诉求)。可作后续。
- **deny-with-feedback**(codex/opencode 有):atomcode 无此决策,YAGNI;需另铺文本输入子态 + plumbing,不在 v1。

## 未决问题

无(v1 全部拍板)。plan 阶段需核实:①`AgentEvent::ApprovalNeeded` 是否携带 scope(决定 Always 标签能否显精确 pattern);②`UiLine::ApprovalPrompt` 变体除渲染外是否有其他引用;③审批面板在 footer 堆叠的确切槽位与高度计算点。
