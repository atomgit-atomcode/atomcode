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
| subagent | `~/.claude/agents/*.md` | ❌ 格式不兼容 | AtomCode **有**用户自定义角色文件（`harness/src/plugins/team.rs:1889` 读 `<产品树>/agents/` 与 `<项目>/.atomcode/agents/`），但格式不同：角色文件**必须**带 `permission` 与 `difficulty` 两个 frontmatter 键（`team.rs:279,403`），Claude 的 subagent 文件没有，会被解析器拒绝 |
| hooks | `<产品树>/hooks.json`、`<project>/.hooks.json` | ✅ 直接读 | `capabilities/src/cc_hooks.rs:300,305` |

**结论**：项目级资产大体原生兼容；**用户级**外部资产（`~/.claude/CLAUDE.md`、`~/.claude/settings.json`、`~/.codex/skills`）是真空缺口。这正是报告要如实讲清的事。

## 3. 设计

### 3.1 资产状态四态

报告里每一项资产带一个状态：

| 状态 | 含义 | 动作 |
|---|---|---|
| `Active` | AtomCode 现在就从**这个路径**读到它 | 无 |
| `Imported` | 它的等价物**已经在我们的配置里**（上一轮导入过了） | 无 —— 这是「回检」那一半 |
| `Actionable` | 有既定导入路径（复制同格式 / 转换映射） | 报告给出**具体目标路径**或映射结果 |
| `Blocked` | 本期搬不动（有损 / 属第三期 / 无目标格式 / 文件损坏） | 报告说明为什么 |

不给「已兼容 / 需转换」两态：多态才能区分「搬一下就好」和「搬不了」，而后者是用户最需要提前知道的。

`Imported` 是「检测 → 迁移 → 回检」四步循环的回检半环：没有它，第二次运行会把「已经在配置里」的账号说成「可以带走」，用户分不清「还没导」和「已经导了但内容不同」。

回检比对四样：**协议**、**endpoint**、**模型清单**、**凭据的种类**。

- 协议与 endpoint 按**写入后的形态**比（写入方会归一账号 id，也会把「协议自带默认 endpoint」存成空，报告不跟着归一就永远对不上）。
- 模型名按**去空白后**比（写入方会 `trim`）。
- 凭据按**种类**比：引用 vs 引用、字面量 vs 字面量 —— **从不按值比**。拿密钥出来比对既没意义（文件里的密钥可能早就轮换了），也不该在内存里过一遍明文。

> 状态为 `Blocked` 的两种来源要分开说：**读不出来**（文件坏了）与**搬不动**（格式不兼容、有损）。前者是「你的文件有问题」，后者是「我们不支持」，用户要做的动作完全不同。

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

- `migration` feature **依赖 `atomcode-config`** —— 为了**共用写入方的归一规则**：账号 id 与 endpoint 写入时会被归一（`base_id` 清洗并可能加前缀，`endpoint_override` 把「协议自带默认 endpoint」存成空），报告不跟着归一，回检就永远对不上。规则必须只有一份，不能两份各自漂移。capabilities→config 这条边**本来就有**（`provider` / `mcp` / `instructions` / `session` 等 feature 都拉它），门禁也不禁这条 —— 门禁禁的是 `atomcode-tui → atomcode-coding / atomcode`。
- `capabilities` **不解析环境**。家目录、项目根由宿主解析后传进来（`AGENTS.md`：「解析环境只在宿主」）。
- feature 声明：`migration = ["tools", "instructions", "mcp", "skills", "dep:atomcode-i18n", "dep:atomcode-config", "dep:toml"]`。

  这些依赖不是为了拿能力，是为了让「已经生效」这句话**由真加载器说出来**：

  | feature | 复用什么 | 用来判什么 |
  |---|---|---|
  | `instructions` | `instructions::load_instructions` | 指令文件是 `Loaded` / `Empty` / `Unreadable` / `Shadowed` 中的哪一个 |
  | `skills` | `skills::SkillRegistry` | 这个技能**是不是**在生效的那一个（同名的另一个目录会覆盖它） |
  | `mcp` | `mcp::parse_mcp_servers` | 整个 MCP 文件我们的加载器会不会拒绝 |
  | `tools` | `tools::permission_rules::PermissionRules::parse` | 权限文件里的规则我们的加载器会不会拒绝 |
  | `dep:atomcode-config` | `provider_book::account_id_for` / `effective_endpoint` | 账号 id 与 endpoint **写成什么样** |

  自己再写一份校验，迟早与真加载器不一致 —— 而「说生效、其实不生效」正是这个报告最不能有的错。

  `toml` 用来读 Codex 的 `config.toml`。`atomcode-i18n` 是零 atomcode 依赖的叶子（`gates/layers.sh` 守它），所以 capabilities 引它不破坏分层（`memory` 模块已是同样做法）。`atomcode-config` 这条边 capabilities **本来就有**（`provider` / `mcp` / `instructions` / `session` 等 feature 都拉它），门禁也不禁这条 —— 门禁禁的是 `atomcode-tui → atomcode-coding / atomcode`。
- 报告的**结构化数据**在 capabilities，**文案**在 `atomcode-i18n` 的 **product 表**（CLI、daemon、setup 流程共用那张）。CLI 与 TUI 都读 product 表 → 一件事一种说法，不写两遍。

**i18n 归属**：`capabilities/migration` 用 `atomcode_i18n::product::{t, Msg}` 出文案（与 `setup::SetupReport::render_cli`、`memory` 模块同一做法）。TUI 面板的**边框文案**（标题、空态）走 screen 表。TUI 与 CLI 源码里**不许出现中文字面量**（`gates/tui-i18n.sh` 棘轮，扫 `crates/atomcode-tui/src` 与 `crates/atomcode-cli/src`）。

### 3.3 公共 API

```rust
// capabilities/src/migration/mod.rs

/// 宿主解析好的扫描上下文。capabilities 不读环境。
pub struct ScanContext {
    /// OS 家目录（`~/.claude` 在它下面）。
    ///
    /// 叫 `os_home` 而不是 `home`：本 crate 里还有另一个 `home`，指的是
    /// **产品目录**（`~/.atomcode`）—— 全局指令层读的是它的 `ATOMCODE.md`
    /// （`instructions.rs:128`），权限层读的是它（`permission_settings.rs:286`）。
    /// 两个混起来，报告就会把没被读的文件说成「已经生效」。
    pub os_home: std::path::PathBuf,
    /// 当前项目根。
    pub project: std::path::PathBuf,
    /// 我们自己的树在哪 —— 报告因此能给出**具体目标路径**，而不是让人自己
    /// 猜「复制到用户级目录」到底复制到哪。
    ///
    /// `ProductDirs` 就在 capabilities 本 crate 里，带着它不增依赖。
    pub product: crate::ProductDirs,
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
    ProviderConfig, Agents, Sessions, Plugins,
}

pub enum AssetStatus { Active, Imported, Actionable, Blocked }

pub enum AssetScope { User, Project }

pub struct AssetReport {
    pub kind: AssetKind,
    pub name: String,            // 资产名（技能名 / "settings.json" / …）
    pub path: std::path::PathBuf, // 源路径
    pub scope: AssetScope,
    pub status: AssetStatus,
    pub detail: String,           // 给人看的一句话：为什么这个状态、怎么办
                                  // （可带走时，这里写出**具体目标路径**）
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
| 指令（项目） | `<project>/CLAUDE.md` | `Active`（直接读）—— 但若同目录有 `AGENTS.md` 等更高优先级文件，则是 `Blocked`（被遮蔽，不会被读） |
| 指令（用户） | `~/.claude/CLAUDE.md` | `Actionable`（复制到产品用户树 `ATOMCODE.md`） |
| 技能 | `~/.claude/skills/*`、`<project>/.claude/skills/*` | `Active` —— 但**由真注册表判定**：解析失败、被同名的另一个目录覆盖、或在被忽略的缓存目录里，都不是 `Active` |
| 命令 | `~/.claude/commands/*`、`<project>/.claude/commands/*` | `Active` —— 同上，由真注册表判定（命令模板在注册表的**另一张表**里，`slash_commands()`） |
| 权限（项目） | `<project>/.claude/settings.json`、`settings.local.json` | `Active`（文件损坏则 `Blocked`） |
| 权限（用户） | `~/.claude/settings.json` | `Actionable`（复制到产品用户树 `settings.json`） |
| MCP（项目） | `<project>/.mcp.json` | `Active`（文件损坏则 `Blocked`） |
| MCP（用户） | `~/.claude.json` 的 `mcpServers` | `Actionable`（需写成产品用户树 `mcp.json`） |
| hooks | `~/.claude/settings.json` 的 `hooks` | `Actionable`（复制到产品用户树 `hooks.json`） |
| subagent | `~/.claude/agents/*.md` | `Blocked`（**格式不兼容**：我们的角色文件必须带 `permission` 与 `difficulty` 两个 frontmatter 键，Claude 的没有，会被解析器拒绝） |
| 插件 | `~/.claude/plugins/` | `Blocked`（插件清单格式不同） |
| provider | `~/.claude/settings.json` 的 `env.ANTHROPIC_*` | `Actionable`（第二期） |
| 会话 | `~/.claude/projects/**/*.jsonl` | `Blocked`（第三期，有损） |

**Codex**（`~/.codex` + `<project>/.codex`）

| 资产 | 路径 | 状态 |
|---|---|---|
| 指令（用户） | `~/.codex/AGENTS.md` | `Actionable` |
| 指令（项目） | `<project>/AGENTS.md` | `Active`（开放标准，我们直接读） |
| 技能 | `~/.codex/skills/*` | `Actionable`（复制到产品用户树 `skills/`） |
| MCP（用户） | `~/.codex/config.toml` 的 `[mcp_servers.*]` | `Actionable`（需转成 `mcp.json`） |
| provider | `~/.codex/config.toml` 的 `model` / `model_providers.*` | `Actionable`（第二期；已在我们的配置里则是 `Imported`） |
| 会话 | `~/.codex/sessions/**/*.jsonl` | `Blocked`（第三期） |

> 只有**存在**的路径才出现在报告里。空目录不报。
>
> **目录读不出来 ≠ 目录是空的**：读不了就是 `Blocked`（附上原因），不是「没发现」。Codex 的「残留兜底」在这里对应的就是这条 —— 探测到的每一项都在报告里，一项都不静默消失。

### 3.5 第二期：provider 账号导入

**映射**（`migration/provider.rs` 只产出中立草案，不写盘）：

| 外部 | 外部字段 | AtomCode 字段 |
|---|---|---|
| Claude | `settings.json` → `env.ANTHROPIC_BASE_URL` | `provider_accounts.<id>.base_url` |
| Claude | `settings.json` → `env.ANTHROPIC_API_KEY`（**值就是密钥**） | `provider_accounts.<id>.api_key`（**逐项确认后**才写） |
| Claude | `settings.json` → `env.ANTHROPIC_AUTH_TOKEN` | 同上 |
| Codex | `config.toml` → `[model_providers.<id>].base_url` | `provider_accounts.<id>.base_url` |
| Codex | `config.toml` → `[model_providers.<id>].env_key`（**变量名**） | `provider_accounts.<id>.api_key = "${VAR_NAME}"` |
| Codex | `config.toml` → `model` | `models.<id>.model` |
| Codex | `config.toml` → `model_providers.<id>.wire_api` | `provider_accounts.<id>.provider`（`openai-responses` / `openai-compatible` / `anthropic-compatible`） |
| Codex | `config.toml` → `[model_providers.<id>].wire_api`（**不认识的**值） | **不导入** —— 报 `Blocked` 并写明是哪个协议。`create_account` 会把未知 preset 静默降级成 `openai-compatible`（`config/provider_preset.rs:381`），那就是「用错的线协议连上了」 |
| Codex | `config.toml` → `[model_providers.<id>].wire_api`（**没写**） | `openai-responses` —— Codex 的 `WireApi` 默认是 Responses（`codex-rs/model-provider-info/src/lib.rs:117`），不是 chat/completions |

**凭据策略**（四条，不可绕过）：

1. **不读 Keychain。** Claude 的 key 在 macOS Keychain、Codex 是 OAuth token —— 都搬不了，也不假装能搬。
2. **按来源各自的语义处理，不猜。** 两个来源的凭据字段**方向相反**：
   - Codex 的 `env_key` 是**变量的名字** → 写成 `api_key = "${VAR_NAME}"`。
   - Claude 的 `env` 块是**名字 → 值**，值**就是**密钥本身 → 只能是明文，走第 3 条。

   一个「长得像变量名就当引用」的启发式会把 Claude 的 `ANTHROPIC_API_KEY=MY_KEY`（值恰好像名字）误判成引用，把真密钥当名字存进文件 —— 运行时读不到密钥，而文件里还留着一个看似无害的字符串。**按来源分别处理。**
3. **明文密钥只在逐项确认后写。** 源里若是明文，报告标「检测到明文密钥」，只在用户对**该项**明确确认后才写入；**永不批量导入**。

   **`--yes` 不构成这个确认。** `--yes` 回答的是「把这些账号导进来」，不是「把你没见过的密钥写进文件」—— 带明文密钥的账号**每次都要问**，即使有 `--yes`。没有终端可问时，这类账号**一个都不写**，`config.toml` 保持逐字节不变。
4. **写得出来的形式只有两种，且不重叠。** `resolved_api_key`（`config/provider.rs:356`）对 `api_key` 有三个分支：含 `$` 的按环境变量展开；与某个活着的环境变量同名的取那个变量；其余当密钥本身。同一个字符串可能因此有两种含义，所以写出去的形式必须唯一：
   - **引用**写成 `${NAME}`。展开无歧义：变量没设置就展开成空，**不会**把字符串 `NAME` 当成密钥。写成裸名字会落到「当密钥本身」那个分支，把 `NAME` 当成密钥 —— 那是错得最离谱的一种：文件看着像引用，跑起来拿的是字符串 `NAME`。

     **但展开成空不等于「没有密钥」。** `resolved_api_key` 展开后若为空，会**继续往下找**平台凭据：`OPENAI_API_KEY`（openai / responses）、`ANTHROPIC_API_KEY`（anthropic）、`ATOMCODE_API_KEY`（`config/provider.rs:375-396`）。所以一个没设置 `${NAME}` 的账号，可能拿到**另一个**凭据去连新端点 —— 迁移来的端点收到不属于它的密钥。

     这不是说 `${NAME}` 不能用：它是**唯一**不把变量名当密钥的写法，而且这正是我们要的（明文密钥原样搬过来更糟）。但**报告与文案不能说「展开成空就没有密钥」** —— 那是不实的。真要严格绑定某个变量，得改 `resolved_api_key` 的解析语义，那是另一个改动，不在本期。
   - **明文**写成它自己，但前提是不能被读成别的东西：不含 `$`（否则会被展开），也没有同名的活环境变量（否则那个变量会盖过它）。不满足就**拒绝写**并说明原因 —— 写一个有两种含义的文件比不写更糟。

**写入**：复用 `atomcode_config::provider_book::ProviderBook::create_account`（`provider_book.rs:192`）。它自带文件锁、`toml_edit` 逐键 patch（保住用户在文件里的注释与排版）、原子替换。**不新造写入逻辑。**

**落点**：**第二期只做 CLI**（`atomcode import --apply`）。TUI 的 `/import` 只出报告，并指向这条 CLI 命令 —— 凭据写入留在 CLI 这一侧（与 `atomcode login` 同一层），TUI 不长出凭据写入流程。

**确认粒度**：**逐项**。

- **带明文密钥的账号**：每次都问。交互式逐条问；**没有终端可问时一个都不写**（管道 / CI 场景），退出码非 0。
- **只引用变量名、不含密钥的账号**：`--yes` 直接写；没有 `--yes` 但有终端时也逐条问；**既没有 `--yes` 又没有终端时也不写**（同样没人可问），退出码非 0。

也就是说：**没有终端可问时，需要回答的账号一个都不写**，其余照写。退出码非 0（有东西没导）。

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

> 为什么不直接让 TUI 调 capabilities？TUI **可以**直接调 capabilities，但**家目录只有宿主能解析**：`atomcode-tui` 没有 `dirs` 依赖，`atomcode-host-api` 里也没有家目录字段（已核实）。而扫描要扫的就是「**别人家目录**里的资产」—— 这个路径必须由宿主解析后传进来。端口形态与 `/setup` 一致，少一套心智。

### 3.7 失败语义

| 情况 | 行为 |
|---|---|
| 外部目录不存在 | 该来源不出现 / 该项不报。**不是错误。** |
| 外部文件格式损坏（坏 JSON / 坏 TOML） | 该项报 `Blocked`，`detail` 说明「文件无法解析」，**不 panic、不中断整次扫描** |
| 外部目录存在但读不了 | 报 `Blocked` 并附上原因 —— **不是**「没发现」。读不了与没有是两件事。 |
| 单个资产探测出错 | 其余资产照常报告；错误变成那一项的 `detail` |
| 扫描期间文件被删 | 视同不存在 |
| `--apply` 时配置已被别的进程改过 | `ProviderBook` 在锁内读最新文档再决策（`provider_book.rs:165` `write_deciding`），**合并**而不是拒绝；只有 id 冲突才拒绝 |
| `--apply` 遇到带明文密钥的账号 | **每次都问**（`--yes` 也不代替这个确认）；没有终端可问时**不写**，退出码非 0 |
| `--apply` 遇到写不出唯一含义的密钥 | 拒绝写并说明原因（见 §3.5 第 4 条），退出码非 0 |
| `--apply` 遇到我们不会的协议 | 报告 `Blocked` 并写明是哪个协议；**写入前拒绝**，不导入（`create_account` 会把未知 preset 静默降级成 `openai-compatible`，那就是「用错的线协议连上了」） |
| `--apply` 无终端可问（管道 / CI） | **需要回答的账号一个都不写** —— 没有终端就没有人能回答。只引用变量名、不含密钥的账号**照写**（`--yes` 或有终端时逐条问过）。退出码非 0（因为有东西没导） |

**不静默丢弃**：任何探测到的资产都必须出现在报告里（`Active` / `Imported` / `Actionable` / `Blocked` 四态之一）。这是 Codex 「残留兜底」在 AtomCode 的对应物 —— 我们的版本是「**没有一项资产可以凭空消失**」。

**回检**：`Imported` 那一态就是回检的结果 —— 已经在我们配置里的账号，第二次运行报「已导入」而不是「可以带走」。

比对四样：**协议**、**endpoint**、**模型清单**、**凭据的种类**。

- 协议与 endpoint 都按**写入后的形态**比（写入方会把 id 归一、把「协议自带默认 endpoint」存成 `None`，报告不跟着归一就永远对不上）。
- 模型名按**去空白后**比（写入方会 `trim`）。
- 密钥按**种类**比：引用 vs 引用、字面量 vs 字面量，**从不按值比** —— 文件里的密钥可能早就轮换了，读出来比对既没意义也不该让明文过一遍内存。

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
- 环境变量名映射：`env_key = "MY_KEY"` → `api_key = "${MY_KEY}"`（**变量引用**，不是密钥值本身；写成裸名字会被当成密钥）。
- 明文密钥：不确认不写；确认后只写该项。
- 无终端可问：需要回答的账号**一个都不写**。`config.toml` **字节不变** —— 但这只在**没有可写的账号**时成立。一批里既有需要回答的、又有不需要的，前者被跳过、后者照写，文件就**会变**（变的是被写的那部分）。
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
