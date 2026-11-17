# 一键迁移其他 AI Coding Agent 的资产 —— 设计（第一期 + 第二期）

- 日期：2026-11-17
- 状态：设计已定（brainstorming 结论），待实施计划
- 参照：OpenAI Codex 的 `codex-rs/external-agent-migration`（16681 行，「检测 → 迁移 → 残留兜底」三段式）
- 关联 Issue：[Feature] 一键迁移其他 AI Coding Agent 的资产（指令/技能/配置/会话）

## 1. 目标与非目标

### 目标

给从 Claude Code / Codex 迁来的用户一条**只读、可解释**的路径，回答两个问题：

1. 我的资产有多少 AtomCode 已经能直接用？
2. 剩下的怎么办、能不能带走？

本期交付：

- **第一期**：双层扫描（用户级 + 项目级）+ 资产清单报告 + 残留兜底说明。**纯读，不写任何文件。**
- **第二期**：provider 账号（`base_url` / 协议 / 模型清单）映射进 `config.toml`；**凭据逐项确认**后才写入。

### 非目标

- **不做会话历史转换**（Claude JSONL → kernel event 格式）。信息有损风险最高，留作第三期；本期只**计数并列出**。
- **不改动运行时兼容面**。AtomCode 读不到哪些外部资产（见 §3），本期只在报告里如实标注，**不补读取层**。
- **不读 macOS Keychain**，不把明文密钥悄悄搬进我们的文件。
- 不进 runtime，不进 `describe_self`（见 §5）。

## 2. 现状核实（代码实证，2026-11-17）

Issue 正文说的「兼容基础已打好」与代码有出入。核实结果（**以代码为准**）：

| 资产 | 外部路径 | AtomCode 实际读取？ | 依据 |
|---|---|---|---|
| 指令 | `<project>/CLAUDE.md` | ✅ 直接读 | `capabilities/src/instructions.rs:17` `PROJECT_NAMES` |
| 指令 | `~/.claude/CLAUDE.md` | ❌ **读不到** | 全局层只有 `home.join("ATOMCODE.md")`（同文件 `:128`），而此 `home` 是 `atomcode_home()`（`$ATOMCODE_HOME`/`~/.atomcode`），**不是 OS 家目录** |
| 技能 | `~/.claude/skills`、`~/.claude/commands` | ✅ 直接读 | `capabilities/src/skills/registry.rs:571` `standard_skill_dirs` |
| 技能 | `~/.codex/skills` | ❌ **不在扫描路径** | 同上，列表里没有 `.codex` |
| 权限 | `<project>/.claude/settings.json`、`settings.local.json` | ✅ 直接读 | `coding/src/permission_settings.rs:288` |
| 权限 | `~/.claude/settings.json` | ❌ **读不到** | 同上，目录列表是「产品用户树 / `<project>/.claude` / 产品项目树」，无 `~/.claude` |
| MCP | `<project>/.mcp.json` | ✅ 直接读 | `capabilities/src/mcp/config.rs:221` |
| MCP | `~/.claude.json` 的 `mcpServers` | ❌ 读不到 | 该文件是 Claude 的全局状态，本仓库无任何读取点 |
| 指令 | `~/.codex/AGENTS.md` | ❌ 读不到 | 全仓库无 `.codex` 资产读取点 |
| subagent | `~/.claude/agents/*.md` | ❌ 不支持 | AtomCode 无用户自定义 subagent 定义文件（只有 `subagent.external` 委派外部 CLI） |
| hooks | `<产品树>/hooks.json`、`<project>/.hooks.json` | ✅ 直接读 | `capabilities/src/cc_hooks.rs:300,305` |

**结论**：项目级资产大体原生兼容；**用户级**外部资产（`~/.claude/CLAUDE.md`、`~/.claude/settings.json`、`~/.codex/skills`）是真空缺口。这正是报告要如实讲清的事。

## 3. 设计

### 3.1 资产状态三态

报告里每一项资产带一个状态：

| 状态 | 含义 | 动作 |
|---|---|---|
| `Active` | AtomCode 现在就从**这个路径**读到它 | 无 |
| `Actionable` | 有既定导入路径（复制同格式 / 转换映射） | 报告给出具体目标路径或映射结果 |
| `Blocked` | 本期搬不动（有损 / 属第三期 / 无目标格式） | 报告说明为什么 |

不给「已兼容 / 需转换」两态：三态才能区分「搬一下就好」和「搬不了」，而后者是用户最需要提前知道的。

### 3.2 模块与分层

```
crates/atomcode-capabilities/src/migration/     ← 新增，feature "migration"
    mod.rs        公共 API：scan / 报告类型 / 来源注册表
    source.rs     MigrationSource trait
    claude.rs     ClaudeSource
    codex.rs      CodexSource
    report.rs     AssetKind / AssetStatus / AssetReport / MigrationReport
    provider.rs   第二期：外部 provider 配置 → 中立 ProviderAccountDraft
```

**关键约束**（`AGENTS.md` + `gates/layers.sh` + ADR 0022 §3）：

- `migration` feature **不依赖 `atomcode-config`**。这样 `atomcode-tui` 可以直接用（UI 不许依赖产品 config crate，ADR 0022 §3；`gates/layers.sh` 守这条）。
- `capabilities` **不解析环境**。家目录、项目根由宿主解析后传进来（`AGENTS.md`：「解析环境只在宿主」）。
- feature 声明：`migration = ["dep:atomcode-i18n", "dep:toml"]`。`toml` 用来读 Codex 的 `config.toml`；`atomcode-i18n` 是零 atomcode 依赖的叶子（`gates/layers.sh` 守它），所以 capabilities 引它不破坏分层（`memory` 模块已是同样做法）。
- 报告的**结构化数据**在 capabilities，**文案**在 `atomcode-i18n` 的 **product 表**（CLI、daemon、setup 流程共用那张）。CLI 与 TUI 都读 product 表 → 一件事一种说法，不写两遍。

**i18n 归属**：`capabilities/migration` 用 `atomcode_i18n::product::{t, Msg}` 出文案（与 `setup::SetupReport::render_cli`、`memory` 模块同一做法）。TUI 面板的**边框文案**（标题、空态）走 screen 表。TUI 与 CLI 源码里**不许出现中文字面量**（`gates/tui-i18n.sh` 棘轮，扫 `crates/atomcode-tui/src` 与 `crates/atomcode-cli/src`）。

### 3.3 公共 API

```rust
// capabilities/src/migration/mod.rs

/// 宿主解析好的扫描上下文。capabilities 不读环境。
pub struct ScanContext {
    /// OS 家目录（`~/.claude` 在它下面）。
    pub home: std::path::PathBuf,
    /// 当前项目根。
    pub project: std::path::PathBuf,
}

/// 一个外部 Agent 的资产来源。
pub trait MigrationSource: Send + Sync {
    /// 稳定 id，如 "claude-code" / "codex"。
    fn id(&self) -> &'static str;
    /// 给人看的名字。
    fn display_name(&self) -> &'static str;
    /// 只读探测。返回的每一项都带状态与说明。
    fn detect(&self, ctx: &ScanContext) -> Vec<AssetReport>;
}

/// 内置来源注册表：Claude Code、Codex。
pub fn builtin_sources() -> Vec<Box<dyn MigrationSource>>;

/// 扫一遍所有来源。
pub fn scan(sources: &[Box<dyn MigrationSource>], ctx: &ScanContext) -> MigrationReport;
```

```rust
// capabilities/src/migration/report.rs

pub enum AssetKind {
    Instructions, Skills, Commands, McpServers, Permissions, Hooks,
    ProviderConfig, Agents, Sessions,
}

pub enum AssetStatus { Active, Actionable, Blocked }

pub enum AssetScope { User, Project }

pub struct AssetReport {
    pub kind: AssetKind,
    pub name: String,            // 资产名（技能名 / "settings.json" / …）
    pub path: std::path::PathBuf, // 源路径
    pub scope: AssetScope,
    pub status: AssetStatus,
    pub detail: String,           // 给人看的一句话：为什么这个状态、怎么办
}

pub struct SourceReport {
    pub source_id: &'static str,
    pub display_name: &'static str,
    pub assets: Vec<AssetReport>,
}

pub struct MigrationReport {
    pub sources: Vec<SourceReport>,
}
```

### 3.4 第一期：探测什么

**Claude Code**（`~/.claude` + `<project>/.claude`）

| 资产 | 路径 | 状态 |
|---|---|---|
| 指令（项目） | `<project>/CLAUDE.md` | `Active`（直接读） |
| 指令（用户） | `~/.claude/CLAUDE.md` | `Actionable`（复制到产品用户树 `ATOMCODE.md`） |
| 技能 | `~/.claude/skills/*`、`<project>/.claude/skills/*` | `Active` |
| 命令 | `~/.claude/commands/*`、`<project>/.claude/commands/*` | `Active` |
| 权限（项目） | `<project>/.claude/settings.json`、`settings.local.json` | `Active` |
| 权限（用户） | `~/.claude/settings.json` | `Actionable`（复制到产品用户树 `settings.json`） |
| MCP（项目） | `<project>/.mcp.json` | `Active` |
| MCP（用户） | `~/.claude.json` 的 `mcpServers` | `Actionable`（需写成产品用户树 `mcp.json`） |
| hooks | `~/.claude/settings.json` 的 `hooks` | `Actionable`（复制到产品用户树 `hooks.json`） |
| subagent | `~/.claude/agents/*.md` | `Blocked`（AtomCode 无用户自定义 subagent 定义文件） |
| 插件 | `~/.claude/plugins/` | `Blocked`（插件清单格式不同） |
| provider | `~/.claude/settings.json` 的 `env.ANTHROPIC_*` | `Actionable`（第二期） |
| 会话 | `~/.claude/projects/**/*.jsonl` | `Blocked`（第三期，有损） |

**Codex**（`~/.codex` + `<project>/.codex`）

| 资产 | 路径 | 状态 |
|---|---|---|
| 指令（用户） | `~/.codex/AGENTS.md` | `Actionable` |
| 技能 | `~/.codex/skills/*` | `Actionable`（复制到产品用户树 `skills/`） |
| MCP（用户） | `~/.codex/config.toml` 的 `[mcp_servers.*]` | `Actionable`（需转成 `mcp.json`） |
| provider | `~/.codex/config.toml` 的 `model` / `model_providers.*` | `Actionable`（第二期） |
| 会话 | `~/.codex/sessions/**/*.jsonl` | `Blocked`（第三期） |

> 只有**存在**的路径才出现在报告里。空目录不报。

### 3.5 第二期：provider 账号导入

**映射**（`migration/provider.rs` 只产出中立草案，不写盘）：

| 外部 | 外部字段 | AtomCode 字段 |
|---|---|---|
| Claude | `settings.json` → `env.ANTHROPIC_BASE_URL` | `provider_accounts.<id>.base_url` |
| Claude | `settings.json` → `env.ANTHROPIC_API_KEY`（**环境变量名**时） | `provider_accounts.<id>.api_key = "<VAR_NAME>"` |
| Claude | `settings.json` → `env.ANTHROPIC_AUTH_TOKEN` | 同上 |
| Codex | `config.toml` → `[model_providers.<id>].base_url` | `provider_accounts.<id>.base_url` |
| Codex | `config.toml` → `[model_providers.<id>].env_key`（**环境变量名**） | `provider_accounts.<id>.api_key = "<VAR_NAME>"` |
| Codex | `config.toml` → `model` | `models.<id>.model` |
| Codex | `config.toml` → `model_providers.<id>.wire_api` | `provider_accounts.<id>.provider`（`openai-compatible` / `anthropic-compatible`） |

**凭据策略**（三条，不可绕过）：

1. **不读 Keychain。** Claude 的 key 在 macOS Keychain、Codex 是 OAuth token —— 都搬不了，也不假装能搬。
2. **优先环境变量引用。** 外部配置若用**变量名**指向密钥（Codex `env_key`、Claude `env.*`），写成 `api_key = "<VAR_NAME>"`。密钥不进我们的磁盘，运行时由 `ProviderAccountConfig::resolved_api_key`（`config/provider.rs:356`）解析 —— **这条路径本来就支持变量名**。
3. **明文密钥只在逐项确认后写。** 源里若是明文，报告标「检测到明文密钥」，只在用户对**该项**明确确认后才写入；**永不批量导入**。

**写入**：复用 `atomcode_config::provider_book::ProviderBook::create_account`（`provider_book.rs:192`）。它自带文件锁、`toml_edit` 逐键 patch（保住用户在文件里的注释与排版）、原子替换。**不新造写入逻辑。**

**落点**：第二期需要 `atomcode-config`（只有 CLI 有），所以**第二期只做 CLI**（`atomcode import --apply`）。TUI 的 `/import` 只出报告，并指向这条 CLI 命令。这样凭据写入留在 CLI 这一侧（与 `atomcode login` 同一层），TUI 不长出凭据写入流程。

**确认粒度**：**逐项**。`--apply` 交互式逐条问；非交互环境（管道 / CI）缺 `--yes` 则拒绝写入并列出待确认项。

### 3.6 入口

**CLI**

```
atomcode import              # 只读扫描报告
atomcode import --json       # 同一份报告，机器可读
atomcode import --apply      # 第二期：逐项确认后写入 provider 账号
```

**TUI**

```
/import                      # 报告面板：按来源分组，逐项显示状态
```

TUI 通过 `atomcode-tui` 新增的 `migration.rs` 端口 trait（镜像 `setup.rs` / `tui_setup.rs` 的形状）拿到数据；实现在 `atomcode-cli/src/tui_migration.rs`。

端口的形状与 `Setup` 完全同形：

- `atomcode-tui/src/migration.rs` 定义**自己的**视图类型（`MigrationReportView` / `MigrationSourceView` / `MigrationAssetView`），**不复用** `capabilities::migration` 的类型 —— 与 `setup.rs` 定义自己的 `CapabilityReport` / `CapabilityEntry` 同一做法，TUI 的依赖面保持最小。
- 端口方法：`fn report(&self) -> Result<MigrationReportView, String>`（只读，无副作用）。
- 实现在 `atomcode-cli/src/tui_migration.rs`：调 `capabilities::migration::scan`（CLI 有 `atomcode-config` 与家目录解析），把结果**转成**端口的视图类型。

**文案只出一处**：`capabilities::migration` 产出的每项资产**已带本地化好的 `detail`**（用 `atomcode_i18n::product`）。两个前端都只**显示**它，不各自拼句子 → 一件事一种说法。TUI 面板的标题 / 空态等**界面文案**走 screen 表。

> 为什么不直接让 TUI 调 capabilities？因为 `migration` feature 不依赖 `atomcode-config`，TUI **可以**直接调。但家目录只有宿主能解析：`atomcode-tui` 没有 `dirs` 依赖，`atomcode-host-api` 里也没有家目录字段（已核实）。而扫描要扫的就是「**别人家目录**里的资产」—— 这个路径必须由宿主解析后传进来。端口形态与 `/setup` 一致，少一套心智。

### 3.7 失败语义

| 情况 | 行为 |
|---|---|
| 外部目录不存在 | 该来源不出现 / 该项不报。**不是错误。** |
| 外部文件格式损坏（坏 JSON / 坏 TOML） | 该项报 `Blocked`，`detail` 说明「文件无法解析」，**不 panic、不中断整次扫描** |
| 单个资产探测出错 | 其余资产照常报告；错误变成那一项的 `detail` |
| 扫描期间文件被删 | 视同不存在 |
| `--apply` 时配置已被别的进程改过 | `ProviderBook` 自带修订号，`create_account` 失败即报错，**不覆盖** |
| `--apply` 非交互且无 `--yes` | 拒绝写入，打印待确认清单，退出码非 0 |

**不静默丢弃**：任何探测到的资产都必须出现在报告里（`Active` / `Actionable` / `Blocked` 三态之一）。这是 Codex「残留兜底」在 AtomCode 的对应物 —— 我们的版本是「**没有一项资产可以凭空消失**」。

### 3.8 文案（i18n）

新增文案进 `crates/atomcode-i18n/src/product/`（messages.rs + en.rs + zh_cn.rs 三处同改，穷尽 match，漏译是编译错误）。

CLI 与 TUI 都读 product 表 → 一件事一种说法。TUI 源码里**不许出现中文字面量**（`gates/tui-i18n.sh` 棘轮）。

### 3.9 测试判据

**第一期**

- 预置样例 `~/.claude` / `~/.codex` 目录 → 扫描快照判据：发现的资产**类型 / 数量 / 路径 / 状态**逐一断言。
- 无外来资产时报告为空，**不是报错**。
- 坏 JSON / 坏 TOML → 该项 `Blocked`，其余项照常，扫描不 panic。
- 状态判定判据：`~/.claude/skills` → `Active`；`~/.claude/CLAUDE.md` → `Actionable`；`~/.claude/agents/x.md` → `Blocked`。
- 报告渲染判据：CLI 文本含关键资产名与状态词。

**第二期**

- 映射往返：外部配置 → 草案 → 写 `config.toml` → 重新加载，**语义等价**；不含真实密钥（用哨兵值）。
- 环境变量名映射：`env_key = "MY_KEY"` → `api_key = "MY_KEY"`（**不是**密钥值本身）。
- 明文密钥：不确认不写；确认后只写该项。
- 非交互无 `--yes`：拒绝写入且 `config.toml` **字节不变**。
- 已有同 id 账号：不覆盖，报告冲突。

**门**

- `bash gates/compile.sh`（阻塞）
- `cargo nextest run -p atomcode-capabilities -p atomcode-cli -p atomcode-tui`
- `cargo fmt --all` 且 `cargo fmt --all -- --check` 退出 0
- `bash gates/layers.sh`、`bash gates/product-dir.sh`、`bash gates/tui-i18n.sh`、`bash gates/tui-layers.sh`

### 3.10 与 Codex 的差异（为什么我们的第一期更轻）

Codex 要**复制 + 改写**外部资产进 `~/.codex/`（所以有 rewrite profile）。AtomCode 的大部分资产**原地就能读**，所以第一期只是「说清楚」；真正要搬的只有用户级那几处。差距在报告的诚实度，不在代码量。

## 4. 范围与交付边界

- **本期做**：第一期全部 + 第二期全部（provider 账号导入，含逐项确认）。
- **本期不做**：会话转换（第三期）、改运行时兼容面（补 `~/.claude/CLAUDE.md` 等读取层）、插件迁移、subagent 定义迁移、MCP 实际导入（只在报告里标 `Actionable` 并给出目标路径，不写盘）。
- **MCP 为何只标不搬**：MCP 配置含 `command` / `env`，搬过去等于搬一段可执行的东西，风险与凭据同级，应另起一期单独审。

## 5. 不进 runtime 的理由

`AGENTS.md`：「不需要运行中 conversation/provider/session 的本地查询或副作用，应留在 driver/local service」。扫描是本地只读诊断，属 driver 层。

`describe_self` 也不登记它：登记规则是「实现那件事的行（或读 patch/flag 的启动器）经 `self_knowledge::describes` 登记」。扫描不是一行能力，是个本地命令 —— 与 `config-file` 行不同（那个确实在读配置）。

## 6. 开放问题（留待实施时按需定，不阻塞）

- 报告是否需要 `--json` 稳定 schema 承诺？（倾向：不承诺 v1，只保证结构可解析。）
- 第二期是否需要支持 `.codex/config.toml` 的 `wire_api = "responses"` → `openai-responses` 预置？（倾向：支持，`ProviderType::Responses` 已有。）
- `~/.claude.json` 很大（实测 184 KB），探测 `mcpServers` 时只解析需要的键还是整文件？（倾向：整文件反序列化一次，只取 `mcpServers` 与 `projects.*.mcpServers`。）
