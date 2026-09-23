# /mcp 面板设计

> 2026-09-23 · 分支 `feat/mcp-panel` · 基线 `02b73ada3`
> 前端：`atomcode-tui`（row 装配的那个）

## 1. 背景与目标

今天 `/mcp` 在 `atomcode-tui` 里是一列文本：`crates/atomcode-tui/src/commands.rs:1485` 把无参调用变成一次
`HostCommand::McpStatus`，再把服务器名和状态拼成字符串打印。人看不到工具数、看不到来源，也不能改任何东西。

目标：做成**交互式管理面板**，对齐 Claude Code 的 "Manage MCP servers"——列表按来源分组、每行带状态与工具数，
`Enter` 钻进二级面板看配置与诊断，并在面板里完成**信任、认证、启用/停用**。

前端选 `atomcode-tui` 的依据：`crates/atomcode-cli/src/main.rs:2503` 的 `--tui` 路径交给
`atomcode_tui::launch::Screen`，日志写 "handing control to the row-assembled TUI"；且该 crate 的 `lib.rs`
写明"不要照 tuix 建，在这里按 row 建"。

## 2. 已定的决策

| 决策 | 选定 | 否决的 |
| --- | --- | --- |
| 作用范围 | **C：导航 + 全部操作** | 只读导航；导航 + 单服务器开关 |
| 交互 | **Enter 钻进二级面板** | Enter 弹就地动作菜单；直接热键 |
| 详情页内容 | **配置与诊断为主**（状态/认证/地址/来源） | 工具清单逐个开关；两者合一 |
| 「停用」语义 | **持久**：写配置 `disabled: true` | 仅本次会话（`WithdrawMcpTools`）；两者都要 |
| 送达方式 | **列表精简 + 钻进去取详情** | 列表自带全部数据（扩公共类型）；分两版交付 |

## 3. 边界与组件

| 层 | 加什么 | 为什么是这一层 |
| --- | --- | --- |
| `atomcode-capabilities::mcp` | ① 一条**含停用项**的读取路径 ② `set_…_disabled()` 写入器 | 配置读写是 capabilities 的活；写入器已有两个先例（`config.rs:451`、`config.rs:499`）可照抄，连注释守卫一起沿用 |
| `atomcode-coding::runtime` | 信任/取消信任、登录/登出、改停用的操作与状态 | 运行中状态归 runtime；AGENTS.md 要求"操作运行中状态的行为必须通过 runtime 定义清楚的命令、事件和终态" |
| `atomcode-cli/src/host.rs` | 实现新增的 host 命令 | host 是 driver 与 runtime 之间的实现层 |
| `atomcode-host-api` | 新增命令与回包，**只增不改** | `McpStatus` 另有消费者（`crates/atomcode-tui/src/plugin.rs:2702` 的设置页查询），动它会牵连设置页与测试 |
| `atomcode-tui` | 新面板 `src/modules/mcp.rs`；`/mcp` 无参时打开它 | 面板家族的统一住址（`crates/atomcode-tui/src/resume.rs:3`：`/config` `/provider` `/plugin` `/toolbox` `/rewind` 同一件事） |

**必须钉死的区分**：面板的「停用」是**持久**的（写 `mcp.json` 的 `disabled: true`），而已有的
`HostCommand::SwitchTool` 是**会话内**的（其文档原话"The connection is untouched either way"）。
两者语义不同、互不替代——面板不借用 `SwitchTool`，免得以后有人以为面板开关等于改了配置。

**已有可复用的命令**：`Reload`（重读 skills/MCP/配置）、`WithdrawMcpTools`（撤下全部 MCP 工具）、
`ToolCatalog`。

## 4. 数据形状

### 4.1 新命令

```
McpManage { session }                    → 列表行：name, state, 来源, 工具数, disabled
McpDetail { session, server }            → 详情：transport, 命令/URL, timeout, auth, 配置文件路径
McpAct    { session, server, action }    → action ∈ { Trust, Untrust, Login, Logout, Enable, Disable }
```

动作合成一条命令 + 一个枚举，而不是开六个命令：只多一个 wire 类型、审批与失败只有一条路径、
以后加动作不用再动接口。

### 4.2 二级面板字段映射

| 截图那一行 | AtomCode 的来源 | 现状 |
| --- | --- | --- |
| 标题 `<名> MCP Server` | `McpServerConfig.name` | ✅ 有 |
| `Status:` | `McpServerState` | ⚠ 要加变体（见 4.3） |
| `Auth:` | OAuth 登录态 | ❌ 要新带出来 |
| `URL:` / stdio 时 `Command:` | `McpTransportConfig::Http{url}` / `Stdio{command,args}` + `timeout_ms` | capabilities 有，**没进 host** |
| `Config location:` | `McpConfigSource`（`config.rs:98`：Project/User/Driver）+ 具体文件路径 | source 有、路径要新带 |
| 动作列表 | 按状态现算 | ❌ 新 |

AtomCode **没有**"Dynamically configured"这种情况——没有插件提供的 MCP 服务器。服务器要么来自某个文件，
要么根本不存在。

### 4.3 唯一一处要动已有公共类型

要显示 `⚠ needs authentication` 和 `○ disabled`，必须给 `McpServerState`（`crates/atomcode-host-api/src/lib.rs:769`）
加两个变体：`NeedsAuthentication`、`Disabled`。

兼容性依据：该枚举是 `#[non_exhaustive]`；`atomcode-tui` 的 `/mcp` 匹配本就有兜底分支
（`_ => Msg::McpUnknownState`）。但**仓内自己的 `match` 该补还得补**，所以受影响 crate 要跑全测（见 §7）。

### 4.4 一条前提：停用项今天根本读不到

`load_mcp_config` 在 `crates/atomcode-capabilities/src/mcp/config.rs:212` 把 disabled 的服务器过滤掉了：

```rust
Ok(merged.into_values().filter(|c| !c.disabled).collect())
```

`disabled` 字段本身早就存在（`config.rs:83`），配置写 `disabled: true` 是**已支持**的格式。但列表要显示
`○ 已停用`，就必须新开一条"含停用项"的读取路径——这是 §3 里那一条改动的全部理由。

## 5. 面板行为

### 5.1 列表层

- 标题 `管理 MCP 服务器`，右侧 `N 个服务器`
- 按来源分组，AtomCode 只有三种来源，对应三个组：`全局`（`~/.atomcode/mcp.json`）、
  `项目`（`./.mcp.json`）、`driver`（有才出）
- 每行：光标 + 名字 + 状态词 + 工具数，例如 `context7   ✓ 已连接   8 个工具`
- **停用的服务器也列出来**（`○ 已停用`）——见 §4.4
- **未信任的项目**服务器显示 `未信任`，并带一行提示（对应已有的 `/mcp trust` 可发现性文案）
- 空状态：一台都没有时，一行说明 + 一句"怎么写配置"

### 5.2 详情层

- 标题 `<名> MCP Server`
- 值表四行：状态、认证、地址、来源文件。**地址随传输方式变**：http 显示 `URL`，stdio 显示 `Command`
- 动作列表**按状态现算**：

| 状态 | 动作 |
| --- | --- |
| 未认证 | `认证` / `停用` |
| 已连接 | `停用` |
| 未信任（项目） | `信任` / `停用` |
| 已停用 | `启用` |

### 5.3 按键

- 列表层：`↑↓` 移动、`Enter` 进详情、`Esc` 关闭面板
- 详情层：`↑↓` 在动作间移动、`Enter` 执行、`Esc` 返回列表

### 5.4 动作之后怎么刷新

- **`停用`**：写配置 → 会话内撤下该服务器的工具 → **回到列表层**（状态变了，停在"已停用"的详情页没意义）
- **`认证`**：OAuth 要跳浏览器，TUI 里无法内联完成 → 显示"等待浏览器完成…"中间态，完成后刷新详情
- **`信任`**：整项目级，执行后项目内服务器开始连接 → 显示"连接中"，不阻塞界面

## 6. 失败语义

**一律说出来，不静默**：

- 写配置被拒绝（注释守卫）→ 详情页显示守卫原文 + "请手工编辑或删掉注释后重试"
- 认证失败 / 用户取消 → 显示原因
- 连接失败 → 用已有的 `Failed { message }` 状态词

注释守卫在 `crates/atomcode-capabilities/src/mcp/config.rs:292-316`，原文说明：

> A rewrite serialises the parsed `Value` back out, which would silently delete every comment in the
> file. Refuse instead… Losing someone's annotations without telling them is worse than making them
> edit by hand.

## 7. 测试判据

一律 `cargo nextest run -p <crate>`，不用 `cargo test`。

| 层 | 判据 |
| --- | --- |
| `atomcode-capabilities` | `a_disabled_server_is_written_and_read_back`<br>`a_config_with_comments_refuses_the_disable_rewrite`<br>`listing_shows_disabled_servers_that_loading_still_hides` |
| `atomcode-host-api` | 三条新命令/回包的序列化往返 |
| `atomcode-cli/tests/host.rs` | `mcp_manage_lists_servers_with_source_and_tool_count`<br>`mcp_detail_reports_transport_and_auth`<br>`mcp_act_disable_writes_config_and_withdraws`<br>（已有 `HostCommand::McpStatus` 的测试在 `tests/host.rs:1930`，可照抄结构） |
| `atomcode-tui` | `the_panel_groups_servers_by_source`<br>`a_disabled_server_is_listed`<br>`the_detail_actions_follow_the_state`<br>`escape_from_detail_returns_to_the_list` |

**两条额外的**

1. 加了 `McpServerState` 的两个变体 → 受影响 crate 跑**全测**，专门确认没有 `match` 漏分支。
2. 交付前 `cargo fmt --all`；`--check` 必须退出 0——`check.yml` 里唯一的阻塞门（clippy 仍是 report-only）。

## 8. 明确不做

- **不做插件提供的 MCP 服务器**（截图里那组 "Built-in MCPs"）——AtomCode 没有这个概念，registry 里搜不到
- **不在面板里逐个开关工具**——那是 `/toolbox` 的活，面板只报工具数
- **不改 `McpStatus` / `McpServers` 现有类型**——另有消费者
- **不做会话内启停**——`SwitchTool` 已经存在且语义不同（§3）

## 9. 风险与未决

| 风险 | 处置 |
| --- | --- |
| `McpServerState` 加变体会牵动仓内 `match` | 受影响 crate 跑全测；这是本次唯一"改已有类型"的动作 |
| 面板写配置会碰到用户的 `mcp.json` | 沿用已有注释守卫；失败必须显示，不静默 |
| 认证要跳浏览器，TUI 无法内联 | 用"等待浏览器完成…"中间态；不改认证实现 |
| 工具数是新数据 | 从会话工具目录按 `mcp__<server>__*` 前缀数；不新增连接 |
