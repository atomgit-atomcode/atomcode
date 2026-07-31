# atomcode-capabilities (L1)

构建在中性内核 [`atomcode-kernel`](../atomcode-kernel)(L0)之上的 **L1 能力层**：
真实 provider 适配器、工具、MCP、skills。

---

## 分层规则（现状 + 目标）

**当前已满足（健康方向）：**

- 本 crate **不**依赖 `atomcode-core`，也不依赖任何 L2/L3 crate。
  `cargo tree -p atomcode-capabilities` 不含 `atomcode-core` / `atomcode-coding`。
- 允许的依赖：`atomcode-kernel`(L0) + 第三方 crate + **`atomcode-config`**（leaf 配置 crate，
  位于 core 之下，供各层读取配置而不引入 `atomcode-core` 依赖）。

> 注：文档里「只依赖 kernel + 第三方」的说法不严格 —— `atomcode-config` 也是
> 直接依赖（如 `notify` feature 读取 `NotificationConfig`）。但「不依赖 core / L2 / L3」
> 这条单向边界是成立的，也是内核保持中立的原因：每个*具体*能力（真实 provider、真实工具、
> MCP 客户端、skill 加载器）都住在这一层，而非下沉到内核。

**目标：** 保持上述单向边界，新增能力时不得引入 `atomcode-core`。

---

## 能力是 cargo feature 门控的

默认 features 为 `["provider", "tools"]`。下游嵌入者只拉取所需能力，例如只要
provider 的构建不会编译 MCP/skills 的传递依赖：

- `provider`（**default**）：真实 [`LlmProvider`] 适配器 —— OpenAI 兼容（GLM / DeepSeek / …）、
  Anthropic Messages（Claude）、Ollama 原生（`/api/chat`）。
- `tools`（**default**）：真实中性工具（fs read/write/edit/list + bash + grep/glob）+ 通用审批中间件。
- `web`：`web_fetch` / `web_search`（基于 `tools`，拉 HTTP 栈）。
- `atomgit`：AtomGit REST 工具（repo / pr / issue）。
- `codeintel`：代码智能（`list_symbols` / `read_symbol`，tree-sitter 符号提取）。
- `lsp`：LSP 诊断（spawn 外部语言服务器，JSON-RPC over stdio）。
- `notify`：桌面 / 终端通知（turn-finished + approval-needed），读取 `atomcode-config` 的 `NotificationConfig`。
- `skills`：markdown/frontmatter skill 加载器 + `use_skill` / `list_skills` 工具。
- `cc-hooks`：CC 兼容外部 hooks（`hooks.json`），在 kernel seam 上运行用户命令。
- `mcp`：MCP 客户端（stdio/HTTP/OAuth），把外部 server 工具暴露为内核 Tools。
- `session`：会话持久化 + 跨会话 recall（两层级 on-disk store + Snapshot/Transcript 钩子）。
- `memory`：用户持久记忆（生产 `memory.md` 存储）。
- `offline`：连接失败时自动切换 offline 模式（基于 `atomcode-config`）。
- `lsp-e2e`（default-off）：真实 LSP 端到端测试，需要 rust-analyzer 等已安装。
- `e2e`（default-off）：命中真实 provider 的集成测试。

---

## 公开模块

与 feature 无关的常驻模块：

- `hooks` —— 可复用的、provider 无关的 [`LifecycleHooks`] 实现（如 `WireLogHooks`）。
- `reminder` —— `<system-reminder>` 约定，统一包装运行时上下文注入。
- `compaction` —— 锚定压缩（anchor / focus / 近期上下文保留）。
- `pathnorm` —— 路径归一化。

feature / 平台门控模块（启用对应 feature 或仅特定平台编译）：
- `provider` / `tools` / `notify` / `atomgit` / `codeintel` / `lsp` / `skills` / `mcp` / `session` / `memory`（feature 门控）。
- `cc_hooks` —— CC 外部 hook 准备 / 注册，受 `cc-hooks` feature 门控（非常驻）。
- `askpass` —— 凭据询问（`askpass::server::start` 等），**仅 Unix 编译**，Windows 上不存在。
