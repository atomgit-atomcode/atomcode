# /mcp 面板 —— 数据层实施计划（1/3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `atomcode-capabilities` 能读出**含停用项**的 MCP 配置，并能把某个服务器的停用状态**写回它所在的配置文件**。

**Architecture:** 三件事，都在 `crates/atomcode-capabilities/src/mcp/config.rs`。① 把 `load_mcp_config` 里被过滤掉的那一步拆出来，另给一条保留停用项的读取路径；② 把"某个来源对应哪个文件"从散落的 `config_dir().join(...)` 收敛成一个函数；③ 照 `add_auto_approved_tool` 的模子加一个写入器，**沿用现有的注释守卫**——带注释的文件一律拒绝改写，且必须保持字节不变。

**Tech Stack:** Rust、`serde_json`、`anyhow`、`tempfile`（测试）、`cargo nextest`。

**前置：** 本计划在 worktree `/Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel`、分支 `feat/mcp-panel` 上执行。开工前确认 `git branch --show-current` 是 `feat/mcp-panel`。

**测试命令必须带 `--features mcp`（本计划初稿漏了，是实测抓出来的）。** `mcp` 在这个 crate 里是 opt-in feature：`lib.rs:193` 是 `#[cfg(feature = "mcp")] pub mod mcp;`，而 `default = ["provider", "tools"]`。不带它跑，**整个 `mcp` 模块根本不参与编译**——测试报告会"全绿"，但一个相关用例都没执行。实测数字：默认组合 947 个用例，带 `mcp` 是 **1066** 个，差的 119 个就是 mcp 模块的。这个坑值得记住，因为它**只会在你信任那份绿色报告时咬人**。

**设计依据：** `docs/mcp-panel-design.md` §3（边界）、§4.4（停用项今天读不到）、§6（失败语义）、§7（测试判据）。

---

## File Structure

| 文件 | 动作 | 负责什么 |
| --- | --- | --- |
| `crates/atomcode-capabilities/src/mcp/config.rs` | 修改 | 读取（含/不含停用项）、来源→路径、停用写入器；测试住在同文件的两个 `mod` 里 |
| `docs/mcp-panel-plan-1-data.md` | 本文件 | 本计划 |

不新建文件。这个模块已经有 1105 行，但三件事都是它现有的职责（配置读写），拆开反而会把"读写同一份格式"的知识劈成两半。

---

## Task 1: 一条保留停用项的读取路径

今天 `load_mcp_config` 在最后一步把停用项滤掉了（`config.rs:212`）。管理面板要显示 `○ 已停用`，就必须有另一条不滤的路径。**运行时的读取行为不能变**——已有判据 `shipped_mcp_json_example_parses_as_is` 依赖它。

**Files:**
- Modify: `crates/atomcode-capabilities/src/mcp/config.rs:188-213`（`load_mcp_config`，**含它上面那四行文档注释**——否则替换后旧注释会孤零零留在新函数上方）
- Test: `crates/atomcode-capabilities/src/mcp/config.rs` 的 `mod tests`（约 788 行起）

- [ ] **Step 1: 写失败的测试**

加进 `mod tests`（`crates/atomcode-capabilities/src/mcp/config.rs` 末尾那个 `mod tests`）。用带前缀的服务器名，避免和真实用户配置 `$ATOMCODE_HOME/mcp.json` 撞名——这条路径会读用户级配置。

```rust
    #[test]
    fn listing_shows_disabled_servers_that_loading_still_hides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{
                "panel-test-on":  {"command":"npx","args":["-y","a"]},
                "panel-test-off": {"command":"npx","args":["-y","b"],"disabled":true}
            }}"#,
        )
        .unwrap();

        let listed = load_mcp_config_including_disabled(dir.path()).unwrap();
        let names: Vec<&str> = listed.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"panel-test-off"),
            "the management list shows disabled servers: {names:?}"
        );
        assert!(listed
            .iter()
            .find(|c| c.name == "panel-test-off")
            .unwrap()
            .disabled);

        let loaded = load_mcp_config(dir.path()).unwrap();
        let names: Vec<&str> = loaded.iter().map(|c| c.name.as_str()).collect();
        assert!(
            !names.contains(&"panel-test-off"),
            "the runtime load still withholds it: {names:?}"
        );
        assert!(names.contains(&"panel-test-on"));
    }
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cd /Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel
cargo nextest run -p atomcode-capabilities --features mcp listing_shows_disabled_servers_that_loading_still_hides
```

Expected: 编译失败，`cannot find function 'load_mcp_config_including_disabled' in this scope`。

- [ ] **Step 3: 把合并那一步拆出来，再加公开入口**

把 `config.rs:188-213` 整段替换为：

```rust
/// Merge the user-level and project-level configs, disabled entries included.
///
/// Project overrides user for a server of the same name. This is the whole read; whether
/// disabled servers are withheld is the caller's decision — see [`load_mcp_config`] and
/// [`load_mcp_config_including_disabled`].
fn merge_configs(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    let user_config = load_config_file(
        &crate::mcp::util::config_dir().join("mcp.json"),
        McpConfigSource::User,
    )?;

    let project_config =
        load_config_file(&project_dir.join(".mcp.json"), McpConfigSource::Project)?;

    // Merge: project overrides user
    let mut merged: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    for config in user_config {
        merged.insert(config.name.clone(), config);
    }

    for config in project_config {
        merged.insert(config.name.clone(), config);
    }

    Ok(merged.into_values().collect())
}

/// Load and merge MCP configurations from project and user levels.
///
/// Project config (`.mcp.json` in project root) overrides user config
/// (`ATOMCODE_HOME/mcp.json`) for servers with the same name.
///
/// Servers configured with `disabled: true` are withheld: they are not a tool source for a
/// running session. A surface that manages them wants the other entry point, below.
pub fn load_mcp_config(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    Ok(merge_configs(project_dir)?
        .into_iter()
        .filter(|c| !c.disabled)
        .collect())
}

/// The same merge as [`load_mcp_config`], but keeping `disabled` servers.
///
/// A management surface has to show a server it is offering to re-enable; hiding it would
/// make the switch one-way. Nothing that builds a tool catalog may use this.
pub fn load_mcp_config_including_disabled(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    merge_configs(project_dir)
}
```

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-capabilities --features mcp
```

Expected: 全绿（含既有的 `shipped_mcp_json_example_parses_as_is` 与 `load_mcp_config_reports_malformed_project_file`）。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-capabilities/src/mcp/config.rs
git commit -F - <<'EOF'
feat(mcp): 另给一条保留停用项的读取路径

管理面板要能显示 ○ 已停用,否则"启用"这个开关就是单向的——关掉之后再
也看不见它。把合并那一步从 load_mcp_config 里拆出来,运行时那条继续过滤,
新入口 load_mcp_config_including_disabled 不过滤。

运行时行为未变: load_mcp_config 的返回与改动前逐字相同。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 2: 来源 → 配置文件的路径

写入器要落到**定义该服务器的那个文件**里。今天这个映射散在两处：`load_mcp_config` 里各 `join` 一次，`add_auto_approved_tool` 里又自己判一次。收敛成一个函数，写入器与将来的调用方共用。

**Files:**
- Modify: `crates/atomcode-capabilities/src/mcp/config.rs`（在 `merge_configs` 之后加）
- Test: 同文件的 `mod tests`

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn a_driver_server_has_no_config_file_to_write() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            config_path_for_source(dir.path(), McpConfigSource::Project),
            Some(dir.path().join(".mcp.json")),
            "a project server lives in the project root"
        );
        assert!(
            config_path_for_source(dir.path(), McpConfigSource::User).is_some(),
            "a user server lives under ATOMCODE_HOME"
        );
        assert_eq!(
            config_path_for_source(dir.path(), McpConfigSource::Driver),
            None,
            "a driver-supplied server was never read from a file, so there is none to edit"
        );
    }
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-capabilities --features mcp a_driver_server_has_no_config_file_to_write
```

Expected: 编译失败，`cannot find function 'config_path_for_source' in this scope`。

- [ ] **Step 3: 实现**

紧跟在 `load_mcp_config_including_disabled` 之后加：

```rust
/// The file a server of this source is read from and written back to.
///
/// `None` for [`McpConfigSource::Driver`]: those servers arrive over the wire (an ACP client
/// injecting `mcpServers` in `session/new`) and have no file to edit. A caller that is about
/// to report "disabled" must treat `None` as "not applicable", not as "not found".
pub fn config_path_for_source(
    project_dir: &Path,
    source: McpConfigSource,
) -> Option<std::path::PathBuf> {
    match source {
        McpConfigSource::User => Some(crate::mcp::util::config_dir().join("mcp.json")),
        McpConfigSource::Project => Some(project_dir.join(".mcp.json")),
        McpConfigSource::Driver => None,
    }
}
```

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-capabilities --features mcp a_driver_server_has_no_config_file_to_write
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-capabilities/src/mcp/config.rs
git commit -F - <<'EOF'
feat(mcp): 来源到配置文件的映射收成一个函数

写入器要落到定义该服务器的那个文件里。这个映射今天散在 load_mcp_config
和 add_auto_approved_tool 两处,各写一遍。收敛成 config_path_for_source,
driver 来源返回 None——它从来没有文件。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 3: 停用 / 启用的写入器

模子照 `config.rs:451` 的 `merge_stdio_mcp_server_into_json_file`（同一个文件、同一份格式）与 `config.rs:625` 的 `add_auto_approved_tool`（**就地改已有条目**，而不是整条替换）。守卫照 `read_json_for_rewrite`。

**Files:**
- Modify: `crates/atomcode-capabilities/src/mcp/config.rs`（在 `config_path_for_source` 之后加）
- Test: 同文件的 `mod tests`

- [ ] **Step 1: 写失败的测试（三条）**

```rust
    #[test]
    fn a_disabled_server_is_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::write(
            &target,
            r#"{"mcpServers":{"srv":{"command":"npx","args":["-y","a"]}}}"#,
        )
        .unwrap();

        set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert_eq!(configs.len(), 1);
        assert!(configs[0].disabled, "the flag must survive a reload");

        set_mcp_server_disabled_in_json_file(&target, "srv", false).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert!(!configs[0].disabled);
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            !text.contains("disabled"),
            "enabling removes the key instead of writing false: {text}"
        );
    }

    #[test]
    fn disabling_a_server_the_file_does_not_define_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        let original = r#"{"mcpServers":{"srv":{"command":"npx"}}}"#;
        std::fs::write(&target, original).unwrap();

        let error = set_mcp_server_disabled_in_json_file(&target, "nope", true).unwrap_err();
        assert!(
            error.to_string().contains("not defined"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "a refused write must leave the file byte-identical"
        );
    }

    #[test]
    fn a_server_under_the_legacy_servers_key_can_still_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::write(&target, r#"{"servers":{"srv":{"command":"npx"}}}"#).unwrap();

        set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert!(configs[0].disabled);
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            !text.contains("\"servers\""),
            "the legacy key is folded into mcpServers, same as the other writers: {text}"
        );
    }
```

- [ ] **Step 2: 跑它们，确认失败**

```bash
cargo nextest run -p atomcode-capabilities --features mcp a_disabled_server_is_written_and_read_back
```

Expected: 编译失败，`cannot find function 'set_mcp_server_disabled_in_json_file' in this scope`。

- [ ] **Step 3: 实现**

紧跟在 `config_path_for_source` 之后加：

```rust
/// Turn one configured server off or back on, in the file that defines it.
///
/// Turning off writes `disabled: true`. Turning on **removes** the key rather than writing
/// `false`, so an enabled entry reads exactly like one that never carried it
/// (`McpServerEntry::disabled` is `#[serde(default)]`).
///
/// Bails, leaving the file byte-identical, when the file carries JSONC comments (see
/// [`read_json_for_rewrite`]) or when it does not define `server_key`. This edits an existing
/// entry; it never adds a server.
pub fn set_mcp_server_disabled_in_json_file(
    path: &Path,
    server_key: &str,
    disabled: bool,
) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }

    let mut root: Value = read_json_for_rewrite(path)?;

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP config root must be a JSON object"))?;

    // Merge the legacy `servers` key into `mcpServers` first, so the edit lands on the entry
    // a reader would resolve — and is written back in one place.
    let mut servers = collect_merged_mcp_server_maps(root_obj);
    let entry = servers.get_mut(server_key).ok_or_else(|| {
        anyhow::anyhow!(
            "MCP server '{server_key}' is not defined in {}",
            path.display()
        )
    })?;
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP server '{server_key}' entry is not an object"))?;

    if disabled {
        entry_obj.insert("disabled".to_string(), Value::Bool(true));
    } else {
        entry_obj.remove("disabled");
    }

    root_obj.insert("mcpServers".to_string(), Value::Object(servers));
    root_obj.remove("servers");

    let text = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;

    Ok(())
}
```

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-capabilities --features mcp
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-capabilities/src/mcp/config.rs
git commit -F - <<'EOF'
feat(mcp): 只改一个服务器的停用状态,就地写回配置文件

面板的"停用"是持久的,所以要写进定义该服务器的那个文件。照 add_auto_approved_tool
的模子:读回根对象、就地改已有条目、整份写回,不动其它顶层键。

启用时删掉 disabled 键而不是写 false——McpServerEntry::disabled 是 serde(default),
两种写法读者等价,但删掉不会在文件里留一堆 false 噪音。

文件里没有这个服务器时报错而不是补一条:这个函数只改,不新增。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 4: 锁定注释守卫

写入器必须**走 `read_json_for_rewrite`**，而不是自己 `serde_json::from_str`。这条测试是那道门的凭据。

- [ ] **Step 1: 写测试**

```rust
    #[test]
    fn a_config_with_comments_refuses_the_disable_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        let original = "{\n  // 别删我\n  \"mcpServers\": {\"srv\": {\"command\": \"npx\"}}\n}";
        std::fs::write(&target, original).unwrap();

        let error = set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap_err();
        assert!(
            error.to_string().contains("contains comments"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "a refused rewrite must leave the file byte-identical"
        );
    }
```

- [ ] **Step 2: 跑它**

```bash
cargo nextest run -p atomcode-capabilities --features mcp a_config_with_comments_refuses_the_disable_rewrite
```

Expected: **PASS，且不需要写任何新代码。**

这条测试与 Task 3 的其余测试不同：它**一写就该通过**，因为它锁的是继承来的行为。**如果它失败，说明写入器没真的走 `read_json_for_rewrite`**（比如自己调了 `serde_json::from_str`），那是必须修的 bug，不是测试的问题。

- [ ] **Step 3: 提交**

```bash
git add crates/atomcode-capabilities/src/mcp/config.rs
git commit -F - <<'EOF'
test(mcp): 钉住注释守卫——带注释的配置拒绝被改写

面板会写用户的 mcp.json,而那份文件允许注释。写入器继承了
read_json_for_rewrite 的那道门:带注释就拒绝,且文件保持字节不变。

这条测试一写就通过,它锁的是继承来的行为——失败即说明写入器绕过了守卫。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 5: 交付门

- [ ] **Step 1: 整个 crate 跑一遍**

```bash
cargo nextest run -p atomcode-capabilities --features mcp
```

Expected: 全绿。**不要用 `--workspace`**——9 个 consumer 各开不同的 feature 子集，全量构建会编 19 份 capabilities。

- [ ] **Step 2: 格式门（`check.yml` 里唯一的阻塞门）**

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: 第二条退出 0。若 `cargo fmt --all` 改动了文件，**单独成一个 commit**，不要混进功能 commit。

- [ ] **Step 3: 确认运行时读取未被影响**

```bash
git stash list
git diff 02b73ada3 -- crates/atomcode-capabilities/src/mcp/config.rs
```

人工核对这份 diff：`load_mcp_config` 的**返回语义**必须与改动前逐字一致，只是内部改成了 `merge_configs` + 过滤。

- [ ] **Step 4: 提交（若上两步有改动）**

```bash
git add -A crates/atomcode-capabilities
git commit -F - <<'EOF'
style: cargo fmt --all

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## 覆盖对照（对着设计文档 §7 数）

| 设计文档 §7 的判据 | 落在哪 |
| --- | --- |
| `a_disabled_server_is_written_and_read_back` | Task 3 Step 1 |
| `a_config_with_comments_refuses_the_disable_rewrite` | Task 4 Step 1 |
| `listing_shows_disabled_servers_that_loading_still_hides` | Task 1 Step 1 |
| `a_driver_server_has_no_config_file_to_write` | Task 2 Step 1（计划新增的判据，服务 §5.1 的 `driver` 组） |
| `a_server_under_the_legacy_servers_key_can_still_be_disabled` | Task 3 Step 1（计划新增，锁住 `servers` 别名被折叠） |

设计 §7 里 `atomcode-host-api` / `atomcode-cli` / `atomcode-tui` 那三行**不在本计划内**，属于计划 2（命令层）与计划 3（面板层）。

## 本计划明确不做

- 不碰 `McpServerState`（加变体是计划 2 的事）
- 不碰 host / runtime / TUI
- 不做"新增服务器到配置文件"（`merge_*` 那两个已经能做，面板也不需要）
- 不做删除服务器
