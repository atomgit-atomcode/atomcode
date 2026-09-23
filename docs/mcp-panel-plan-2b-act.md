# /mcp 面板 —— 命令层写路径实施计划（2b/3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让面板能把六件事**写下去**——信任/取消信任项目、认证/登出服务器、启用/停用服务器——而不是只看得见。

**Architecture:** 与读路径同一副骨架，三层贯通，但每一步都**会改状态**：`atomcode-host-api` 加一个动作枚举与一条命令（**只增不改**）；`atomcode-coding` 在运行时执行六种动作，并遵守**先撤工具、再改状态**的次序；`atomcode-cli/src/host.rs` 实现这条命令。

**Tech Stack:** Rust、`serde`、`tokio`（oneshot 控制通道）、`cargo nextest`。

**前置：** worktree `/Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel`、分支 `feat/mcp-panel`，且**计划 2 的读路径已经落地**（`McpManage` / `McpDetail` / `McpRowFacts` 都在）。

**测试命令（计划 2 踩过两次，这里写对）：**

| crate | 命令 | 基线 |
| --- | --- | --- |
| `atomcode-host-api` | `cargo nextest run -p atomcode-host-api` | 本分支已修好；无 `mcp` feature |
| `atomcode-coding` | `cargo nextest run -p atomcode-coding` | 860 跑，**2 个既有失败**（与本次无关） |
| `atomcode`（cli，目录叫 `atomcode-cli`） | `cargo nextest run -p atomcode` | 404 passed，健康 |

**设计依据：** `docs/mcp-panel-design.md` §3（边界）、§4.1（命令形状）、§5.2（动作随状态）、§5.4（动作之后怎么刷新）、§6（失败语义）、§7（判据）。

---

## 这份计划为什么排在面板层之前

面板的"动作列表"（信任 / 认证 / 启用停用）要按下有落点，就得先有这条写路径。先把动作做通，面板才不是"能看不能改"的半成品。

---

## File Structure

| 文件 | 动作 | 负责什么 |
| --- | --- | --- |
| `crates/atomcode-host-api/src/lib.rs` | 修改 | `McpAction` 枚举；`McpAct` 命令；三处穷尽性闸门各补一臂 |
| `crates/atomcode-coding/src/parts.rs` | 修改 | 六个动作各自的实现，以及**先撤工具再改状态**的次序 |
| `crates/atomcode-coding/src/runtime.rs` | 修改 | 控制变体、分发臂、handle 方法、`reject_runtime_control` 一臂 |
| `crates/atomcode-cli/src/host.rs` | 修改 | 分发与动作映射 |
| `crates/atomcode-cli/tests/host.rs` | 修改 | 判据 |

**四处穷尽性闸门（读路径时逐一撞过，别漏）：** `commands()`、`replies()`、`HostCommand::addressed()`、以及运行时侧的 `reject_runtime_control`。加变体不补这些，编译直接不过。

---

## Task 1: `McpAction` 与 `McpAct`

**Files:**
- Modify: `crates/atomcode-host-api/src/lib.rs`（`McpDetail` 命令之后；`McpServerDetail` 之后加枚举）
- Test: 同文件 `mod tests` 的 `commands()` / `replies()` / `addressed()` 三处

- [ ] **Step 1: 写失败的测试**

在 `commands()` 的 `all` 里 `HostCommand::McpDetail { … }` 那一项之后加：

```rust
            HostCommand::McpAct {
                session: "a".into(),
                server: "fs".into(),
                action: McpAction::Disable,
            },
```

**三处闸门都要补**（`all` 的条目只是其一）：

```rust
// 1. commands() 末尾的 for…match
                | HostCommand::McpAct { .. } => {}

// 2. HostCommand::addressed() 里，Self::McpDetail { session, .. } 之后
            | Self::McpAct { session, .. }
```

`replies()` 不用动——`McpAct` 复用已有的 `HostReply::McpRows`（见 Step 3 的决定）。

- [ ] **Step 2: 跑它，确认失败**

```bash
cd /Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel
cargo nextest run -p atomcode-host-api every_host_variant_crosses_the_wire_unchanged
```

Expected: 编译失败，`no variant named 'McpAct' found for enum 'HostCommand'`。

- [ ] **Step 3: 加枚举与命令**

`McpServerDetail` 之后：

```rust
/// What a person can do to one MCP server from the management panel.
///
/// `Enable` and `Disable` are named from the person's point of view, not the
/// file's: **`Disable` writes `disabled: true`, `Enable` removes that key.**
/// Getting this backwards silently inverts every switch in the panel, so the
/// mapping is spelled out here once.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpAction {
    /// Trust this project, so its `.mcp.json` servers may connect at all.
    Trust,
    /// Withdraw that trust. The project's tools come off the session first.
    Untrust,
    /// Run the OAuth flow for this server. Blocks until it ends — it waits on a
    /// browser, so it can take minutes.
    Login,
    /// Forget the stored token for this server. Its tools come off first.
    Logout,
    /// Let this server run again: remove `disabled` from the file that defines it.
    Enable,
    /// Switch this server off: write `disabled: true` into that file.
    Disable,
}
```

`HostCommand` 里 `McpDetail` 那一项之后：

```rust
    /// Do one thing to one configured MCP server (`docs/mcp-panel-design.md`
    /// §4.1).
    ///
    /// Answers with the refreshed list rather than this one server, because half
    /// of these change the whole project's picture — trust is project-wide. A
    /// caller that stays on a detail page re-reads that server with `McpDetail`.
    McpAct {
        session: String,
        server: String,
        action: McpAction,
    },
```

`HostReply` **不动**：`McpRows` 已经在读路径里加过了，直接复用——少一个 wire 类型，也少一张要维护的登记表。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-host-api
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-host-api/src/lib.rs
git commit -F - <<'EOF'
feat(host-api): McpAct —— 一个动作枚举,一条命令

六种动作:信任/取消信任(整项目级)、认证/登出(单服务器)、启用/停用(写配置)。

Enable 与 Disable 按人的视角命名,不按文件的:Disable 写 disabled: true,
Enable 去掉那个键。写反了会把面板里每一个开关都静默反过来,所以这层映射
在枚举上写死了一遍。

回包复用已有的 McpRows 而不是新开一个:一半的动作会改变整个项目的图景
(信任是整项目级的),刷新后的列表比单服务器的详情更有用;停在详情页的
调用方自己再读一次 McpDetail。

四处穷尽性闸门补了三处(commands() 的 all 与 match、addressed());
replies() 不用动,因为回包没新增。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 2: 运行时执行六个动作

**一条层次约束先写死**：`atomcode-coding` **不依赖** `atomcode-host-api`（读路径就是这么做的——`McpRowFacts` 住在 coding，`McpRow` 住在 host-api，由 `cli/host.rs` 映射）。所以动作枚举在 coding 侧**另立一份**，`cli/host.rs` 负责两边的映射。别图省事让 coding 依赖 host-api。

**次序纪律（源码里写明的，不是我的设计偏好）：** `crates/atomcode-coding/src/parts.rs:1312-1321` 的注释说明，**凡是要改信任或认证状态的调用方，都先撤下工具**（`/mcp reload`、`/mcp untrust`、`/mcp logout` 三个都是），读完发现状态不可用就**直接返回、不重建**——fail-closed。所以 `Untrust` 与 `Logout` 必须**先撤后改**。

**为什么"停用"不用 glob 关工具**：工具名经 `sanitize_name_segment` 处理，超长名还会带哈希后缀（`mcp/tool.rs:55-64`），拼 `mcp__<server>__*` 会漏掉那些服务器。所以用该服务器**已发布的工具名**逐个关。

**Files:**
- Modify: `crates/atomcode-coding/src/parts.rs`（`McpRowFacts` 之后加 `McpAction` 与执行函数）
- Modify: `crates/atomcode-coding/src/runtime.rs`（控制变体 / 分发臂 / handle 方法 / `reject_runtime_control` 一臂）
- Test: `crates/atomcode-coding/src/parts.rs` 的测试模块

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn disabling_a_server_writes_the_flag_and_enabling_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::write(
            &target,
            r#"{"mcpServers":{"srv":{"command":"npx","args":["-y","x"]}}}"#,
        )
        .unwrap();

        futures::executor::block_on(mcp_set_enabled(dir.path(), "srv", false)).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("\"disabled\": true"), "off writes the flag: {text}");

        futures::executor::block_on(mcp_set_enabled(dir.path(), "srv", true)).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(!text.contains("disabled"), "on removes the key: {text}");
    }
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-coding disabling_a_server_writes_the_flag_and_enabling_removes_it
```

Expected: 编译失败，`cannot find function 'mcp_set_enabled' in this scope`。

- [ ] **Step 3: 实现**

在 `parts.rs` 里 `mcp_row_facts` 之后加：

```rust
/// What a person can do to one MCP server. This crate's own copy, not
/// `atomcode_host_api::McpAction`: the wire type lives above this layer
/// (`cli/host.rs` maps between them, the way it maps `McpRowFacts` → `McpRow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpAction {
    Trust,
    Untrust,
    Login,
    Logout,
    /// Remove `disabled` from the file that defines it.
    Enable,
    /// Write `disabled: true` into that file.
    Disable,
}

/// Write one server's on/off flag into the file that defines it.
///
/// Resolves the file from the server's *source*, so the edit lands where a
/// reader would resolve it. Bails when the server is not configured at all, and
/// when the file carries comments (`set_mcp_server_disabled_in_json_file`'s
/// guard) — the caller surfaces that text verbatim.
pub async fn mcp_set_enabled(
    working_dir: &std::path::Path,
    server: &str,
    enabled: bool,
) -> Result<(), String> {
    use atomcode_capabilities::mcp::{
        config_path_for_source, load_mcp_config_including_disabled,
        set_mcp_server_disabled_in_json_file,
    };

    let configs = load_mcp_config_including_disabled(working_dir).map_err(|e| e.to_string())?;
    let config = configs
        .iter()
        .find(|c| c.name == server)
        .ok_or_else(|| format!("MCP server '{server}' is not configured"))?;
    let path = config_path_for_source(working_dir, config.source)
        .ok_or_else(|| format!("MCP server '{server}' has no config file to edit"))?;

    set_mcp_server_disabled_in_json_file(&path, server, !enabled).map_err(|e| format!("{e:#}"))
}
```

`!enabled` 是那处**容易写反**的映射：`enabled: false`（人按了"停用"）→ 写 `disabled: true`。枚举上的文档注释与这里的 `!` 是同一个意思的两处表述。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-coding disabling_a_server_writes_the_flag_and_enabling_removes_it
```

Expected: PASS。

- [ ] **Step 5: 接上控制通道**

`runtime.rs`：`CodingRuntimeControl` 加一条，`reject_runtime_control` 加一臂（照 `McpRows` 那两处写，各自 `done.send(Err(RuntimeError::Unavailable))`），handle 加 `mcp_act(server: String, action: crate::parts::McpAction)`。

分发臂照 `McpRows` 臂的形状，六种动作各自落到 capabilities 的入口：

```rust
                        let outcome = match action {
                            McpAction::Trust => {
                                atomcode_capabilities::mcp::trust::trust_project(
                                    &runtime.config.working_dir,
                                )
                                .map_err(|e| format!("{e:#}"))
                            }
                            McpAction::Untrust => {
                                // Fail-closed, in this order: the tools come off
                                // BEFORE the trust that lets them connect is
                                // withdrawn (`parts.rs:1312-1321`).
                                runtime.parts.withdraw_mcp_tools().await;
                                atomcode_capabilities::mcp::trust::untrust_project(
                                    &runtime.config.working_dir,
                                )
                                .map(|_| ())
                                .map_err(|e| format!("{e:#}"))
                            }
                            McpAction::Logout => {
                                runtime.parts.withdraw_mcp_tools().await;
                                atomcode_capabilities::mcp::McpTokenStore::default()
                                    .delete_token(&server)
                                    .map(|_| ())
                                    .map_err(|e| format!("{e:#}"))
                            }
                            McpAction::Login => {
                                // Blocking: it waits on a browser. The token is
                                // saved by `login_mcp_oauth` itself.
                                let config = load_mcp_config_including_disabled(
                                    &runtime.config.working_dir,
                                )
                                .map_err(|e| format!("{e:#}"))?
                                .into_iter()
                                .find(|c| c.name == server)
                                .ok_or_else(|| format!("MCP server '{server}' is not configured"))?;
                                tokio::task::block_in_place(|| {
                                    atomcode_capabilities::mcp::login_mcp_oauth(
                                        &config,
                                        atomcode_capabilities::mcp::McpOAuthLoginOptions {
                                            client_id: None,
                                            client_secret_env: None,
                                            scopes: Vec::new(),
                                        },
                                    )
                                })
                                .map(|_| ())
                                .map_err(|e| format!("{e:#}"))
                            }
                            McpAction::Disable | McpAction::Enable => {
                                let enabled = action == McpAction::Enable;
                                crate::parts::mcp_set_enabled(
                                    &runtime.config.working_dir,
                                    &server,
                                    enabled,
                                )
                                .await?;
                                if !enabled {
                                    // Take THIS server's tools off the session —
                                    // by their published names, not by a glob
                                    // (sanitised names can carry a hash suffix).
                                    if let Some(catalog) = runtime.parts.tool_catalog() {
                                        for name in runtime.parts.mcp_tools_for_server(&server) {
                                            catalog.turn_off(&name);
                                        }
                                    }
                                }
                                Ok(())
                            }
                        };
                        let _ = done.send(match outcome {
                            Ok(()) => Ok(()),
                            Err(message) => Err(RuntimeError::ReconfigureFailed(message)),
                        });
```

`ReconfigureFailed(String)` 是 `RuntimeError` 里语义最近、且能带原文的变体——**注释保护拒绝写入时那段说明必须原样送到前端**（设计 §6），所以不能吞成一句笼统的失败。

- [ ] **Step 6: 跑测试并提交**

```bash
cargo nextest run -p atomcode-coding
git add crates/atomcode-coding/src/parts.rs crates/atomcode-coding/src/runtime.rs
git commit -F - <<'EOF'
feat(coding): 面板的六个动作在运行时执行

信任/取消信任、认证/登出、启用/停用。

次序是有出处的,不是我的偏好:parts.rs:1312-1321 写明凡是要改信任或认证状态的
调用方都先撤工具(untrust / logout / reload 三个都是),fail-closed。
所以 Untrust 与 Logout 先 withdraw_mcp_tools 再改状态。

"停用"不用 glob 关工具:工具名经 sanitize_name_segment,超长名还带哈希后缀,
拼 mcp__<server>__* 会漏。用该服务器已发布的工具名逐个关。

登录是阻塞的(等浏览器),走 block_in_place;token 由 login_mcp_oauth 自己存。

动作失败走 RuntimeError::ReconfigureFailed(原文),因为注释保护拒绝写入时那段
说明要原样到前端,不能吞成一句笼统的失败。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 3: `cli/host.rs` 实现 `McpAct`

**Files:**
- Modify: `crates/atomcode-cli/src/host.rs`
- Test: `crates/atomcode-cli/tests/host.rs`（判据：`mcp_act_disable_writes_config_and_withdraws`）

- [ ] **Step 1: 写失败的测试**

照 `crates/atomcode-cli/tests/host.rs` 里读路径那两条的结构加一条，断言：对一个已配置的服务器发 `McpAction::Disable`，回答的 `McpRows` 里该服务器 `state == Disabled`、`tool_count == 0`，且**项目根的 `.mcp.json` 里出现了 `"disabled": true`**（读文件断言，不停在接口层）。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode mcp_act_disable_writes_config_and_withdraws
```

Expected: 失败（`McpAct` 未被实现）。

- [ ] **Step 3: 实现**

映射函数（放读路径那几个映射旁边）：

```rust
fn to_mcp_action(action: McpAction) -> atomcode_coding::parts::McpAction {
    match action {
        McpAction::Trust => atomcode_coding::parts::McpAction::Trust,
        McpAction::Untrust => atomcode_coding::parts::McpAction::Untrust,
        McpAction::Login => atomcode_coding::parts::McpAction::Login,
        McpAction::Logout => atomcode_coding::parts::McpAction::Logout,
        McpAction::Enable => atomcode_coding::parts::McpAction::Enable,
        McpAction::Disable => atomcode_coding::parts::McpAction::Disable,
    }
}
```

分发（`McpDetail` 臂之后）：

```rust
            HostCommand::McpAct {
                session,
                server,
                action,
            } => {
                self.addressed(&session)?;
                self.handle
                    .mcp_act(server, to_mcp_action(action))
                    .await
                    .map_err(refused)?;
                // Answer with the refreshed list — what happened, not what was
                // asked for. The write path's own `mcp_rows()` is the same one
                // the list uses, so the two cannot disagree.
                let rows = self.handle.mcp_rows().await.map_err(refused)?;
                Ok(HostReply::McpRows {
                    rows: rows.rows.into_iter().map(to_mcp_row).collect(),
                })
            }
```

`McpAction` 是 `#[non_exhaustive]`：`to_mcp_action` 的 `match` 会因此需要一个兜底分支。**不要**加 `_ =>`——加变体时你要的是编译错误，不是它悄悄落到某个默认动作上。改成在函数签名上收窄：接受 `&McpAction` 并让 `match` 穷尽失败即报错（`#[non_exhaustive]` 只约束**外部** crate，仓内仍可穷尽匹配，这里正是仓内）。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode mcp_act_disable_writes_config_and_withdraws
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-cli/src/host.rs crates/atomcode-cli/tests/host.rs
git commit -F - <<'EOF'
feat(host): McpAct 落地

两侧各有一份动作枚举(coding 不依赖 host-api),这一层做映射。

回答用刷新后的列表而不是"成功"三个字:发生了什么,不是被要求了什么。
用的是读路径同一个 mcp_rows(),两者不可能对同一服务器给出不同说法。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 4: 交付门

- [ ] **Step 1: 三个 crate 各跑一遍**

```bash
cargo nextest run -p atomcode-host-api
cargo nextest run -p atomcode-coding
cargo nextest run -p atomcode
```

Expected: host-api 与 cli 全绿；**coding 是 2 个既有失败**（`an_oversized_tool_result_is_shown_head_and_tail_and_saved_whole`、`criteria::a_stopped_reply_is_kept_as_far_as_it_got`），在未改动的主检出上同样失败，**不是本次引入的**。

- [ ] **Step 2: 格式门**

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: 第二条退出 0。**注意**：若同一棵工作树里还有别人未提交的改动（例如面板层在做），`--all` 会连带格式化它们的半成品——先确认工作树里只有你的文件，否则用 `cargo fmt -p <crate>`。

- [ ] **Step 3: 确认没动读路径**

```bash
git diff <读路径最后一个提交> -- crates/atomcode-host-api/src/lib.rs crates/atomcode-coding/src/parts.rs
```

人工核对：`McpManage` / `McpDetail` / `McpRowFacts` / `mcp_row_facts` 一行都没被改，只有新增。

---

## 覆盖对照（对着设计文档 §7 与 §5.2/§5.4）

| 来源 | 判据 | 落在哪 |
| --- | --- | --- |
| §7 | `mcp_act_disable_writes_config_and_withdraws` | Task 3 Step 1 |
| 计划新增 | `disabling_a_server_writes_the_flag_and_enabling_removes_it` | Task 2 Step 1（钉住那处最容易写反的 `!enabled` 映射） |
| 计划新增 | `every_host_variant_crosses_the_wire_unchanged` | Task 1 Step 1（既有判据，扩到 `McpAct`） |
| §5.2 | 动作随状态出现 | **不在本计划**——那是面板层（计划 3）的渲染职责，本计划只保证六个动作**都能执行** |
| §5.4 | 停用后回到列表 | 同上，面板层 |

## 本计划明确不做

- **不做面板**（计划 3）
- **不改读路径的任何类型或函数**
- **不做"停用"的会话内版本**——那是已有的 `SwitchTool`，语义不同（设计 §3 钉过）
- **不在面板里逐个开关工具**（`/toolbox` 的活）
