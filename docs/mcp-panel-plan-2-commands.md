# /mcp 面板 —— 命令层实施计划（2/3，读路径）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让前端能拿到 `/mcp` 面板渲染列表与详情所需的全部数据——每个服务器的名字、状态、来源、工具数、传输方式、认证状态、配置文件路径。

**Architecture:** 三层贯通，**只读不写**。`atomcode-host-api` 加两个变体、四个类型、两条命令两个回包（**只增不改**）；`atomcode-coding` 加一条运行时控制与两个 handle 方法，在那里把**静态配置**（含停用项）与**实时状态**做一次 join；`atomcode-cli/src/host.rs` 实现这两条命令。

**Tech Stack:** Rust、`serde`、`tokio`（oneshot 控制通道）、`cargo nextest`。

**前置：** worktree `/Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel`、分支 `feat/mcp-panel`。

**测试命令必须带 `--features mcp`**（`mcp` 是 `atomcode-capabilities` 的 opt-in feature，`lib.rs:193`）。不加 feature 跑，整个 mcp 模块不参与编译，报告会"全绿"却一个相关用例都没执行。

**设计依据：** `docs/mcp-panel-design.md` §3（边界）、§4.1（命令形状）、§4.2（字段映射）、§4.3（加变体）、§7（判据）。

---

## 范围：这份计划只做读路径

`McpManage` 与 `McpDetail` 两条命令，以及它们需要的类型与运行时管道。**不包含 `McpAct`**（信任/认证/启停那六种动作）——写路径要碰 OAuth 登录流程、信任存储，以及"改状态前先撤下工具"那条 fail-closed 次序（`crates/atomcode-coding/src/parts.rs:1312-1321` 有明确要求），是独立的一份计划（2b）。

读路径自成一个可验收的切片：做完之后前端已经能把列表和详情页完整画出来，写路径再补上"能改"。

---

## File Structure

| 文件 | 动作 | 负责什么 |
| --- | --- | --- |
| `crates/atomcode-host-api/src/lib.rs` | 修改 | 两个 `McpServerState` 变体；`McpTransport` / `McpAuth` / `McpRow` / `McpServerDetail` 四个类型；`McpManage`、`McpDetail` 两条命令与两个回包；两张变体登记表 |
| `crates/atomcode-coding/src/runtime.rs` | 修改 | `McpRowsSnapshot` / `McpDetailSnapshot` 两个快照；两条 `CodingRuntimeControl` 及分发臂；两个 handle 方法 |
| `crates/atomcode-coding/src/parts.rs` | 修改 | 一个 `mcp_rows()`：把静态配置与实时状态 join 起来（`mcp_statuses()` 旁边） |
| `crates/atomcode-cli/src/host.rs` | 修改 | 实现并分发这两条命令 |

**一个内建闸门要记住：** `crates/atomcode-host-api/src/lib.rs:1410` 的 `crosses()` 与 `:1421` 的 `every_host_variant_crosses_the_wire_unchanged` 要求 `commands()`（:938）与 `replies()`（:1105）两张表**逐一列出每个变体**。加变体而不登记，这个测试就会红——这正是 Task 3 的 TDD 失败点。

---

## Task 1: `McpServerState` 加两个变体

面板要显示 `⚠ 需要认证` 与 `○ 已停用`，而这两个状态今天不存在（`ServerStatus` 与 `McpServerState` 都没有）。变体只在 wire 类型上加：`ServerStatus` 是能力层的，保持不动，由运行时在 join 时推导。

**Files:**
- Modify: `crates/atomcode-host-api/src/lib.rs:767-778`（`McpServerState`）
- Test: 同文件 `mod tests`（931 起）的 `replies()`（1105）

- [ ] **Step 1: 写失败的测试**

在 `replies()` 里找到 `HostReply::McpServers { servers: vec![...] }` 那一段（:1142 附近），把其中至少一个 `McpServer` 的状态换成新变体。最小的改法是在那段 `servers` 向量末尾加一个：

```rust
                    McpServer {
                        name: "needs-auth".into(),
                        state: McpServerState::NeedsAuthentication,
                    },
                    McpServer {
                        name: "off".into(),
                        state: McpServerState::Disabled,
                    },
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cd /Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel
cargo nextest run -p atomcode-host-api every_host_variant_crosses_the_wire_unchanged
```

Expected: 编译失败，`no variant named 'NeedsAuthentication' found for enum 'McpServerState'`。

- [ ] **Step 3: 加变体**

把 `lib.rs:767-778` 的 `McpServerState` 换成：

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerState {
    Connecting,
    Connected,
    /// Not started: the project is not trusted.
    Untrusted,
    /// HTTP with OAuth auth, and no usable token stored for this server. Derived
    /// by the runtime from the token store, not reported by the connection: the
    /// connection is never attempted without credentials to try.
    NeedsAuthentication,
    /// `disabled: true` in the file that defines it. The server is not started
    /// and is not in the running session's catalog — it is listed so the switch
    /// back on is reachable (`crates/atomcode-capabilities/src/mcp/config.rs:212`
    /// filters these out of the runtime's own read).
    Disabled,
    Failed {
        message: String,
    },
    Disconnected,
}
```

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-host-api --features mcp
```

Expected: 全绿。`McpServerState` 是 `#[non_exhaustive]`，仓内若有 `match` 漏了分支，编译器会在这里指出。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-host-api/src/lib.rs
git commit -F - <<'EOF'
feat(host-api): McpServerState 加"需要认证"与"已停用"两个变体

面板要显示这两个状态,而它们今天不存在。按设计 §4.3,这是本次唯一一处
改动已有公共类型的地方——纯增,不破坏兼容:枚举是 non_exhaustive,
前端本就有兜底分支。

"需要认证"是推导出来的,不是连出来的:HTTP + OAuth 且 token store 里没有
可用凭据。没有凭据时连接压根不会尝试,所以连接状态报不出这件事。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 2: 四个 wire 类型

列表要"来源 + 工具数"，详情要"传输方式 + 认证 + 配置文件路径"。传输方式**不能直接抄 `McpTransportConfig`**：它的注释（`crates/atomcode-capabilities/src/mcp/config.rs:26-30`）写明载荷带 headers 与 OAuth 材料，"只想说'这是一个 stdio 服务器'的听众不该拿到整个 config"。**headers 可能装着 `Authorization: Bearer …`，绝不能送到前端。**

**Files:**
- Modify: `crates/atomcode-host-api/src/lib.rs`（在 `McpServer`（:760）之后加）
- Test: 同文件 `mod tests`

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn a_transport_crossing_the_wire_carries_no_credentials() {
        // The config it is built from holds headers and OAuth material
        // (`caps::mcp::McpTransportConfig`). The wire type must not.
        let http = McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
            timeout_ms: Some(60_000),
        };
        let json = serde_json::to_string(&http).unwrap();
        for leaked in ["header", "authorization", "client_secret", "token"] {
            assert!(
                !json.to_lowercase().contains(leaked),
                "the wire type must not carry {leaked}: {json}"
            );
        }
        let back: McpTransport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, http);
    }

    #[test]
    fn a_row_and_a_detail_survive_the_wire() {
        let row = McpRow {
            name: "context7".into(),
            state: McpServerState::Connected,
            source: "global".into(),
            tool_count: 8,
            config_path: Some("/home/u/.atomcode/mcp.json".into()),
        };
        let detail = McpServerDetail {
            name: "figma".into(),
            state: McpServerState::NeedsAuthentication,
            source: "project".into(),
            transport: McpTransport::Http {
                url: "https://mcp.figma.com/mcp".into(),
                timeout_ms: Some(60_000),
            },
            auth: McpAuth::OAuth {
                authenticated: false,
            },
            tool_count: 0,
            config_path: None,
        };
        crosses(&row);
        crosses(&detail);
    }
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-host-api a_transport_crossing_the_wire_carries_no_credentials
```

Expected: 编译失败，`cannot find type 'McpTransport' in this scope`。

- [ ] **Step 3: 加四个类型**

紧跟 `McpServerState` 之后加：

```rust
/// How a server is reached, with nothing that could authenticate as anyone.
///
/// Deliberately not `atomcode_capabilities::mcp::McpTransportConfig`: that type's
/// payload carries headers and OAuth material, and a screen showing "this one is
/// an HTTP server" has no business holding them.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

/// Whether a server authenticates, and whether it currently can.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpAuth {
    /// The transport authenticates by nothing the person manages here.
    None,
    OAuth {
        /// A usable token is stored for this server.
        authenticated: bool,
    },
}

/// One row of the `/mcp` list, as a screen draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRow {
    pub name: String,
    pub state: McpServerState,
    /// `McpConfigSource::as_str()` — `"global"`, `"project"` or `"driver"`.
    /// A plain string rather than the enum: the capability type is not this
    /// crate's to publish, and a screen only groups by it.
    pub source: String,
    /// Tools this server has on the session's model, by the names the model
    /// calls them by. Zero for a server that is disabled or not connected.
    pub tool_count: usize,
    /// The file it is defined in, when it is backed by one. `None` for a
    /// driver-supplied server, which never had a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}

/// Everything the `/mcp` detail page shows about one server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDetail {
    pub name: String,
    pub state: McpServerState,
    /// See [`McpRow::source`].
    pub source: String,
    pub transport: McpTransport,
    pub auth: McpAuth,
    pub tool_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}
```

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-host-api
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-host-api/src/lib.rs
git commit -F - <<'EOF'
feat(host-api): 面板要的四个 wire 类型

McpTransport / McpAuth / McpRow / McpServerDetail。

McpTransport 是刻意另立的一份,而不是复用 capabilities 的 McpTransportConfig:
后者的注释写明载荷带 headers 与 OAuth 材料,"只想说这是不是一个 stdio 服务器"
的听众不该拿到整个 config。headers 里可能有 Authorization: Bearer,送到前端
就是泄凭据。判据 a_transport_crossing_the_wire_carries_no_credentials 钉住这点。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 3: 两条命令、两个回包

**Files:**
- Modify: `crates/atomcode-host-api/src/lib.rs`
  - `HostCommand` 枚举（`McpTools`（:133）之后）
  - `HostReply` 枚举（`McpTools`（:313）之后）
  - `commands()`（:938）与 `replies()`（:1105）两张登记表
- Test: `every_host_variant_crosses_the_wire_unchanged`（:1421）

- [ ] **Step 1: 写失败的测试**

在 `commands()` 里 `HostCommand::McpTools { … }` 那一项之后加：

```rust
            HostCommand::McpManage {
                session: "a".into(),
            },
            HostCommand::McpDetail {
                session: "a".into(),
                server: "fs".into(),
            },
```

在 `replies()` 里 `HostReply::McpTools { … }` 那一项之后加：

```rust
            HostReply::McpRows {
                rows: vec![McpRow {
                    name: "fs".into(),
                    state: McpServerState::Connected,
                    source: "project".into(),
                    tool_count: 3,
                    config_path: Some("/w/.mcp.json".into()),
                }],
            },
            HostReply::McpDetail {
                detail: McpServerDetail {
                    name: "fs".into(),
                    state: McpServerState::Disabled,
                    source: "project".into(),
                    transport: McpTransport::Stdio {
                        command: "npx".into(),
                        args: vec!["-y".into(), "srv".into()],
                        timeout_ms: None,
                    },
                    auth: McpAuth::None,
                    tool_count: 0,
                    config_path: Some("/w/.mcp.json".into()),
                },
            },
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-host-api every_host_variant_crosses_the_wire_unchanged
```

Expected: 编译失败，`no variant named 'McpManage' found for enum 'HostCommand'`。

- [ ] **Step 3: 加命令与回包**

`HostCommand` 里 `McpTools` 那一项之后：

```rust
    /// Every configured MCP server for `session`, **disabled ones included**,
    /// with what a management screen groups and counts by
    /// (`docs/mcp-panel-design.md` §4.1). Distinct from `McpStatus`, which
    /// reports only what the running session actually has.
    McpManage { session: String },
    /// One configured server in full, for the detail page. `server` is the
    /// configured key, not a tool name.
    McpDetail { session: String, server: String },
```

`HostReply` 里 `McpTools` 那一项之后：

```rust
    McpRows {
        rows: Vec<McpRow>,
    },
    McpDetail {
        detail: McpServerDetail,
    },
```

**这个 crate 有四处需要补，不是两处——只加枚举变体会编译不过。** 实测撞出来的（`fix(host-api): 补上三个漏登记的变体` 那个提交就是踩了同一道门）：

1. `HostCommand` 变体（上面那条）
2. `HostReply` 变体（上面那条）
3. `commands()` 与 `replies()` 各自末尾的 `for … match`（逐变体列举，**无通配分支**）
4. **`HostCommand::addressed()`（约 :229-270）**——同样没有通配分支

第 3 处，在各自 match 的末尾分支列表追加：

```rust
                | HostCommand::McpManage { .. }
                | HostCommand::McpDetail { .. } => {}
```

```rust
                | HostReply::McpRows { .. }
                | HostReply::McpDetail { .. } => {}
```

第 4 处，在 `Self::McpTools { session, .. }` 之后追加：

```rust
            | Self::McpManage { session }
            | Self::McpDetail { session, .. }
```

`addressed()` 里取 `Some(session)` 是唯一正确的值：判据 `a_command_on_the_live_session_names_it` 要求**除 `ListSessions` 外每条命令都指向活会话**，而两个新命令都带 `session`。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-host-api
```

Expected: 全绿——`crosses()` 会把两个新变体也走一遍序列化往返。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-host-api/src/lib.rs
git commit -F - <<'EOF'
feat(host-api): McpManage 与 McpDetail 两条只读命令

列表要按来源分组、每行带工具数,详情要传输方式与认证状态——McpStatus 给不出
这些(它只报运行中会话真正拿到的东西,停用项压根不在里面)。

只增不改:两条新命令、两个新回包,老类型一个没动。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 4: 运行时把静态配置与实时状态 join 起来

本计划最缠手的一步。数据来自四处，按服务器名 join：

| 来源 | 给出 | 取自 |
| --- | --- | --- |
| 静态配置 | name / disabled / source / transport | `load_mcp_config_including_disabled(working_dir)`（计划 1 交付） |
| 配置文件路径 | 该写回哪个文件 | `config_path_for_source(working_dir, source)`（计划 1 交付） |
| 实时状态 | Connecting / Connected / Untrusted / Failed / Disconnected | `parts.mcp_statuses()` |
| 认证 | 有没有可用 token | `McpTokenStore::default().load_token(name)` |
| 工具数 | 模型实际拿到几个 | `parts.mcp_tools_for_server(name).len()` |

**两条纪律**：① 停用项不进树，它的状态就是 `Disabled`、工具数恒为 0，也**不去问**连接状态；② **不为了让面板显示就去建连接**——认证状态靠读 token store 推导。

**Files:**
- Modify: `crates/atomcode-coding/src/parts.rs`（`mcp_statuses()`（:1348）之后）
- Modify: `crates/atomcode-coding/src/runtime.rs`（快照 :179 之后 / 控制 :2571 附近 / 分发臂 :4773 附近 / handle :1626 之后）
- Modify: `crates/atomcode-coding/src/lib.rs`（把 `McpRowFacts` 重导出成 `atomcode_coding::McpRowFacts`，Task 5 要用这个名字）
- Test: `crates/atomcode-coding/src/parts.rs` 的测试模块（`mcp_tools_for_server_uses_exact_alias_ownership`（:1998）就在那儿）

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn a_disabled_server_is_listed_but_has_no_tools() {
        // A disabled entry is in the file but not in the tree. The management
        // list has to show it — otherwise the switch back on is unreachable —
        // while its tool count stays zero, because the session never got any.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{
                "off": {"command":"npx","args":["-y","x"],"disabled":true}
            }}"#,
        )
        .unwrap();

        let registry = std::sync::Arc::new(McpRegistry::new());
        let facts = futures::executor::block_on(mcp_row_facts(dir.path(), &registry, &[]));
        let row = facts.iter().find(|f| f.name == "off").expect("listed");
        assert!(row.disabled, "the flag reaches the row");
        assert_eq!(row.tool_count, 0, "a disabled server put nothing on the model");
    }
```

> `mcp_row_facts` 是本任务抽出的**自由函数**（见 Step 3）——写成自由函数并接收 statuses/tool_counts 作参数，是为了能在测试里喂一个从没连过任何东西的 registry。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-coding a_disabled_server_is_listed_but_has_no_tools
```

Expected: 编译失败，`cannot find function 'mcp_row_facts' in this scope`。

- [ ] **Step 3: 实现 join**

在 `parts.rs` 文件作用域（`impl` 之外，`mcp_statuses` 附近）加：

```rust
/// One configured MCP server, as a management list needs it: the file's static
/// config joined with what the running session actually has.
///
/// The derives are not decoration: `McpRowsSnapshot` / `McpDetailSnapshot` derive
/// `Clone, Debug, PartialEq` over this type, so without them the snapshots do not
/// compile. No `Eq`: `McpConfigSource` has none.
#[derive(Clone, Debug, PartialEq)]
pub struct McpRowFacts {
    pub name: String,
    pub disabled: bool,
    pub source: atomcode_capabilities::mcp::McpConfigSource,
    pub config_path: Option<std::path::PathBuf>,
    pub transport: atomcode_capabilities::mcp::McpTransportKind,
    /// The stdio program and its args, when this is a stdio server. Never
    /// carries env: that is where a stdio server's own secrets live.
    pub command: Option<(String, Vec<String>)>,
    /// The endpoint, when this is an HTTP server. Never carries headers: those
    /// may hold `Authorization: Bearer …`.
    pub url: Option<String>,
    /// The server authenticates by OAuth (so `authenticated` means something).
    pub oauth: bool,
    /// A usable token is stored for it.
    pub authenticated: bool,
    pub status: atomcode_capabilities::mcp::ServerStatus,
    pub tool_count: usize,
}

/// Join the configured servers (disabled ones included) with the live session.
///
/// `statuses` and `tool_counts` are arguments rather than reads, so the join can
/// be tested against a registry that never connected to anything.
pub async fn mcp_row_facts(
    working_dir: &std::path::Path,
    registry: &atomcode_capabilities::mcp::McpRegistry,
    tool_counts: &[(String, usize)],
) -> Vec<McpRowFacts> {
    use atomcode_capabilities::mcp::{
        config_path_for_source, load_mcp_config_including_disabled, token_is_expired,
        McpHttpAuthConfig, McpTokenStore, McpTransportConfig, ServerStatus,
    };
    use std::collections::HashMap;

    // A malformed file is the connection path's to report; a management list
    // must not turn it into "no servers configured".
    let configs = load_mcp_config_including_disabled(working_dir).unwrap_or_default();
    let live: HashMap<String, ServerStatus> =
        registry.server_statuses().await.into_iter().collect();
    let counts: HashMap<&str, usize> = tool_counts.iter().map(|(n, c)| (n.as_str(), *c)).collect();
    let tokens = McpTokenStore::default();

    configs
        .into_iter()
        .map(|config| {
            let (command, url) = match &config.config {
                McpTransportConfig::Stdio { command, args, .. } => {
                    (Some((command.clone(), args.clone())), None)
                }
                McpTransportConfig::Http { url, .. } => (None, Some(url.clone())),
            };
            let oauth = matches!(
                &config.config,
                McpTransportConfig::Http {
                    auth: Some(McpHttpAuthConfig::OAuth(_)),
                    ..
                }
            );
            let authenticated = oauth
                && matches!(tokens.load_token(&config.name), Ok(Some(t)) if !token_is_expired(&t));
            let disabled = config.disabled;
            McpRowFacts {
                config_path: config_path_for_source(working_dir, config.source),
                status: if disabled {
                    // Not in the tree: the session has no status for it, and
                    // must not be asked for one.
                    ServerStatus::Disconnected
                } else {
                    live.get(&config.name)
                        .cloned()
                        .unwrap_or(ServerStatus::Disconnected)
                },
                tool_count: if disabled {
                    0
                } else {
                    counts.get(config.name.as_str()).copied().unwrap_or(0)
                },
                transport: config.config.kind(),
                command,
                url,
                disabled,
                oauth,
                authenticated,
                name: config.name,
                source: config.source,
            }
        })
        .collect()
}
```

`crates/atomcode-capabilities/src/mcp/mod.rs` 的 `pub use` 清单要补**四个**符号——计划初稿只提了两个，漏掉的两个会让本步的代码解析不了（Step 3 里它们都是不带路径直接用的）：

| 符号 | 出自 | 初稿提到了吗 |
| --- | --- | --- |
| `token_is_expired` | `oauth` | 提了 |
| `McpTokenStore` | `oauth` | 提了（其实早已导出） |
| `config_path_for_source` | `config` | **漏了**——计划 1 只把它加进 `config.rs`，没进导出清单 |
| `load_mcp_config_including_disabled` | `config` | **漏了**，同上 |
| `McpConfigSource` | `config` | **漏了**，同上 |

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-coding a_disabled_server_is_listed_but_has_no_tools
```

Expected: PASS。

- [ ] **Step 5: 接上控制通道**

`runtime.rs` 里 `McpStatusSnapshot`（:179）之后：

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct McpRowsSnapshot {
    pub generation: RuntimeGeneration,
    pub rows: Vec<crate::parts::McpRowFacts>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpDetailSnapshot {
    pub generation: RuntimeGeneration,
    /// `None` when no configured server has that key. Not an error: a screen
    /// says "no such server" and lists what there is.
    pub detail: Option<crate::parts::McpRowFacts>,
}
```

> `runtime.rs` 与 `parts.rs` 同属 `atomcode-coding`，所以这里是 `crate::parts::McpRowFacts`。
>
> **更正（初稿写错了）**：初稿说"`parts` 今天不是公开的"——**不对**，`crates/atomcode-coding/src/lib.rs:54` 就是 `pub mod parts;`，`atomcode_coding::parts::mcp_row_facts` 本来就能命名。根上再导出一次 `McpRowFacts` 仍然要做，理由不同：Task 5 的 `cli/host.rs` 用的是 `atomcode_coding::McpRowFacts` 这个短名字。

`CodingRuntimeControl`（:2571 附近）加两条：

```rust
    McpRows {
        generation: u64,
        done: oneshot::Sender<Result<McpRowsSnapshot, RuntimeError>>,
    },
    McpDetail {
        generation: u64,
        server: String,
        done: oneshot::Sender<Result<McpDetailSnapshot, RuntimeError>>,
    },
```

分发臂照 `McpStatus` 臂（:4773）的形状加两条——先 `request_generation != generation → Err(RuntimeError::Busy)`，再 `resources` 缺席 → `Err(RuntimeError::Unavailable)`，然后：

```rust
                        let counts: Vec<(String, usize)> = runtime
                            .parts
                            .mcp_statuses()
                            .await
                            .into_iter()
                            .map(|(name, _)| {
                                let n = runtime.parts.mcp_tools_for_server(&name).len();
                                (name, n)
                            })
                            .collect();
                        let rows = match &runtime.parts.mcp_registry {
                            Some(registry) => {
                                mcp_row_facts(&runtime.config.working_dir, registry, &counts).await
                            }
                            None => Vec::new(),
                        };
                        let _ = done.send(Ok(McpRowsSnapshot {
                            generation: RuntimeGeneration(generation),
                            rows,
                        }));
```

`McpDetail` 臂同上，末尾 `rows.into_iter().find(|r| r.name == server)` 填 `detail`。

handle 方法照 `mcp_status()`（:1626）的形状加 `mcp_rows()` 与 `mcp_detail(server: String)`。

**还有一处闸门，计划初稿完全没提——执行时撞出来的（E0004）：**

`reject_runtime_control`（`runtime.rs:7811`）是又一个**没有通配分支**的 `CodingRuntimeControl` 穷尽 match（关机／持久化失败时统一拒绝控制命令）。两条新命令必须在这里也各加一臂，照 `McpStatus`/`McpTools` 那两臂的写法，各自 `done.send(Err(RuntimeError::Unavailable))`——**fail-closed**，不是放行。

同理，详情那条臂计划只写了"同上"：**不要把 join 抄两遍**，把它提成一个 `mcp_rows_of(&RuntimeResources)` 私有函数供两条臂共用，列表与详情才不会对同一个服务器给出不同说法。

- [ ] **Step 6: 跑测试并提交**

```bash
cargo nextest run -p atomcode-coding
git add crates/atomcode-coding/src/parts.rs crates/atomcode-coding/src/runtime.rs crates/atomcode-capabilities/src/mcp/mod.rs
git commit -F - <<'EOF'
feat(coding): 面板要的列表与详情,在运行时做一次 join

数据来自四处:静态配置(含停用项)、实时状态、token store、会话工具目录,
按服务器名 join。

两条纪律写进了代码:停用项不去问状态、tool_count 恒为 0——它不在树里;
认证状态靠读 token store 推导,不为了让面板显示就去建连接。

join 抽成自由函数 mcp_row_facts,statuses/tool_counts 走参数,测试才能喂
一个从没连过任何东西的 registry。

McpRowFacts 只带 command/args 与 url,不带 env 与 headers:后两者是凭据住的地方。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 5: `cli/host.rs` 实现这两条命令

**Files:**
- Modify: `crates/atomcode-cli/src/host.rs`（映射函数放 `McpStatus` 臂（:1211）附近，分发加在该臂之后）
- Test: `crates/atomcode-cli/tests/host.rs`（`HostCommand::McpStatus` 的用例在 :1930，先整份读它再照抄脚手架）

- [ ] **Step 1: 写失败的测试**

照 `crates/atomcode-cli/tests/host.rs:1930` 那份的结构加两条，判据名分别取
`mcp_manage_lists_servers_with_source_and_tool_count` 与 `mcp_detail_reports_transport_and_auth`，断言：

- 列表：一条已连接的服务器，`source` 非空、`tool_count` 与 `McpTools` 报的一致、`config_path` 指向项目根的 `.mcp.json`
- 详情：同一个服务器，`transport` 是 `Stdio` 且 `command` 非空；`auth` 为 `McpAuth::None`（没配 OAuth）；`state` 不是 `Disabled`
- 未知服务器名：`McpDetail` 回 `HostError::NotFound`

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-cli --features mcp mcp_manage_lists_servers_with_source_and_tool_count mcp_detail_reports_transport_and_auth
```

Expected: 失败——`McpManage` 未被实现（`host.rs` 的 match 不穷尽或落到兜底）。

- [ ] **Step 3: 实现**

`McpStatus` 臂之后加：

```rust
            HostCommand::McpManage { session } => {
                self.addressed(&session)?;
                let rows = self.handle.mcp_rows().await.map_err(refused)?;
                Ok(HostReply::McpRows {
                    rows: rows.rows.into_iter().map(to_mcp_row).collect(),
                })
            }
            HostCommand::McpDetail { session, server } => {
                self.addressed(&session)?;
                let snapshot = self.handle.mcp_detail(server).await.map_err(refused)?;
                snapshot
                    .detail
                    .map(|facts| HostReply::McpDetail {
                        detail: to_mcp_detail(facts),
                    })
                    .ok_or(HostError::NotFound)
            }
```

文件作用域加三个映射函数：

```rust
/// The capability state → the wire state.
///
/// `Disabled` and `NeedsAuthentication` are derived here, because neither is a
/// connection outcome: one comes from the file's `disabled: true`, the other
/// from whether the token store holds a usable token.
fn to_mcp_state(facts: &atomcode_coding::McpRowFacts) -> McpServerState {
    use atomcode_capabilities::mcp::ServerStatus;
    if facts.disabled {
        return McpServerState::Disabled;
    }
    if facts.oauth && !facts.authenticated {
        return McpServerState::NeedsAuthentication;
    }
    match &facts.status {
        ServerStatus::Connecting => McpServerState::Connecting,
        ServerStatus::Connected => McpServerState::Connected,
        ServerStatus::BlockedUntrusted => McpServerState::Untrusted,
        ServerStatus::Failed(message) => McpServerState::Failed {
            message: message.clone(),
        },
        ServerStatus::Disconnected => McpServerState::Disconnected,
    }
}

fn to_mcp_transport(facts: &atomcode_coding::McpRowFacts) -> McpTransport {
    use atomcode_capabilities::mcp::McpTransportKind;
    match facts.transport {
        McpTransportKind::Stdio => {
            let (command, args) = facts.command.clone().unwrap_or_default();
            McpTransport::Stdio {
                command,
                args,
                timeout_ms: None,
            }
        }
        McpTransportKind::Http => McpTransport::Http {
            url: facts.url.clone().unwrap_or_default(),
            timeout_ms: None,
        },
    }
}

fn to_mcp_auth(facts: &atomcode_coding::McpRowFacts) -> McpAuth {
    if facts.oauth {
        McpAuth::OAuth {
            authenticated: facts.authenticated,
        }
    } else {
        McpAuth::None
    }
}
```

`to_mcp_row` / `to_mcp_detail` 只是把 `McpRowFacts` 的字段按上面的映射拼成 `McpRow` / `McpServerDetail`，`config_path` 用 `facts.config_path.as_ref().map(|p| p.display().to_string())`。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-cli --features mcp mcp_manage_lists_servers_with_source_and_tool_count mcp_detail_reports_transport_and_auth
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-cli/src/host.rs crates/atomcode-cli/tests/host.rs
git commit -F - <<'EOF'
feat(host): McpManage 与 McpDetail 落地

Disabled 与 NeedsAuthentication 在 host 这层推导,因为两者都不是连接结果。

传输方式只带 command/args 与 url,不带 headers 与 env——headers 里可能有
Authorization: Bearer,送到前端就是泄凭据。这是设计 §4.2 与 capabilities
config.rs:26-30 那句注释的共同要求。

未知服务器名回 NotFound,前端据此提示可用名字。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 6: 交付门

- [ ] **Step 1: 三个 crate 各跑一遍**

```bash
cargo nextest run -p atomcode-host-api
cargo nextest run -p atomcode-coding
cargo nextest run -p atomcode-cli --features mcp
```

Expected: 全绿。**不要 `--workspace`**——9 个 consumer 各开不同的 feature 子集，会把这个 95k 行的库编 19 次。

- [ ] **Step 2: 格式门**

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: 第二条退出 0。改动单独成一个 commit。

- [ ] **Step 3: 确认没动老命令**

```bash
git diff 02b73ada3 -- crates/atomcode-host-api/src/lib.rs
```

人工核对：`McpStatus` / `McpTools` / `WithdrawMcpTools` 与它们的回包**一行都没被改**，只有新增。

---

## 覆盖对照（对着设计文档 §7 数）

| 设计 §7 的判据 | 落在哪 |
| --- | --- |
| `mcp_manage_lists_servers_with_source_and_tool_count` | Task 5 Step 1 |
| `mcp_detail_reports_transport_and_auth` | Task 5 Step 1 |
| `a_transport_crossing_the_wire_carries_no_credentials` | Task 2 Step 1（新增，守 `McpTransport` 不泄凭据） |
| `a_row_and_a_detail_survive_the_wire` | Task 2 Step 1（新增） |
| `every_host_variant_crosses_the_wire_unchanged` | Task 3 Step 1（既有判据，扩到新变体） |
| `a_disabled_server_is_listed_but_has_no_tools` | Task 4 Step 1（新增，钉 join 的两条纪律） |

设计 §7 里 `atomcode-tui` 那一行**不在本计划内**，属于计划 3。

## 本计划明确不做

- **不做 `McpAct`**（信任/取消信任、登录/登出、启用/停用）——写路径是计划 2b，要碰 OAuth 流程与 fail-closed 的"改状态前先撤下工具"次序
- **不改 `McpStatus` / `McpTools` / `WithdrawMcpTools`** 与它们的回包
- **不碰 TUI**
- **不把 headers / env / OAuth 材料放进任何 wire 类型**
