# AtomCode Architecture — Module Map

## Crate Overview

| Crate | Role |
|-------|------|
| **atomcode-core** | Agent 引擎 — 工具、上下文、语义分析、LLM 通信 |
| **atomcode-tuix** | 终端 UI — retained-mode 渲染、input、modal 选择器、命令分发 |
| **atomcode-cli** | CLI 入口 — 参数解析、OAuth 登录 |
| **atomcode-daemon** | 后台服务 — HTTP API 服务（独立部署） |

---

## atomcode-core 模块

### Agent 层 — 决策与编排

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `agent/mod.rs` | 2549 | **核心主循环** — start_turn、run_turn_loop、auto_compile、auto_diagnose、system prompt 构建、纪律检查、步数限制。（待拆分） |
| `agent/task_classifier.rs` | 197 | **意图分类** — 用户消息分类为 BugFix/FollowUp/FeatureDev/Question/Command/SingleFileEdit，驱动 read-only 限制和 Analyze 前缀 |
| `agent/knowledge.rs` | 179 | **跨 session 知识持久化** — 从 tool result 自动提取端口/密码，"记住这个"手动记录，存入 .atomcode/knowledge.md |
| `agent/git_checkpoint.rs` | 71 | **Git 检查点** — git stash create 创建轻量备份，/undo 恢复 |
| `agent/subtask_driver.rs` | ~100 | **子任务驱动** — 将计划拆为文件级子任务，auto_compile 通过后推进下一步 |

### 工具层 — 执行与安全

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `tool/mod.rs` | 372 | **工具注册表** — Tool trait、ToolContext、ToolRegistry、权限系统（RequireApproval 不可被 session grant 绕过） |
| `tool/edit.rs` | 952 | **编辑文件** — text-match / line-number / symbol-scoped / fuzzy 四种模式，edit 后附 surrounding context（recency bias），空 old_string 拒绝 |
| `tool/bash.rs` | 380 | **执行命令** — 超时管理、nohup 包装（devserver 自动检测）、破坏性命令拦截（18 种模式）、30s idle 检测、输出附 [cwd:] |
| `tool/read.rs` | ~150 | **读文件** — 大文件截断 1500 行 + tree-sitter skeleton |
| `tool/write.rs` | ~150 | **写文件** — 新文件创建、file_history 备份 |
| `tool/grep.rs` | ~230 | **搜索内容** — regex + smart-case + 自动 literal 降级 + tree-sitter 函数标注 + 3 行默认上下文 |
| `tool/glob.rs` | ~90 | **搜索文件名** — 通配符匹配 |
| `tool/search_replace.rs` | ~170 | **跨文件替换** — 正则批量替换 + file_history 备份 |
| `tool/find_references.rs` | 155 | **查找引用** — ripgrep + tree-sitter 分类（定义 vs 调用） |
| `tool/list_symbols.rs` | ~60 | **列出符号** — tree-sitter 提取函数/类签名 |
| `tool/read_symbol.rs` | ~60 | **读取符号** — 按函数名提取完整代码 |
| `tool/file_history.rs` | ~200 | **文件快照** — edit/write 前自动 copyFile 备份，按 session 隔离，每文件最多 50 版本 |
| `tool/result_store.rs` | ~100 | **结果外部化** — >512B 的 tool result 写磁盘，ToolResultRef 只保留摘要+hash |
| `tool/web_search.rs` | 308 | **Web 搜索** — DuckDuckGo HTML 解析 |
| `tool/web_fetch.rs` | ~80 | **抓取网页** — HTML → 文本提取 |
| `tool/use_skill.rs` | ~80 | **技能调用** — 加载 .atomcode/skills/ 下的 markdown 技能 |
| `tool/cd.rs` | ~50 | **切换目录** |
| `tool/list_dir.rs` | ~50 | **列出目录** |
| `tool/devserver/` | ~600 | **Dev Server 检测** — java/javascript/python/rust 四个语言模块，检测服务命令 + nohup 包装 + 端口探测 + 编译错误增强 |

### 上下文层 — 对话与预算

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `conversation/mod.rs` | 986 | **上下文管理** — hot/cold 分区（30/70）、batch summary、ToolResult 浓缩、消息清理（孤儿 result 删除） |
| `conversation/message.rs` | ~110 | **消息结构** — Text/AssistantWithToolCalls/ToolResult/ToolResultRef，token 估算（byte_size 真实估算） |
| `conversation/turn.rs` | ~80 | **Turn 跟踪** — 轮次边界检测、完成计数 |

### Turn 层 — LLM 通信

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `turn/runner.rs` | 368 | **TurnRunner** — 构建 messages + inflate refs + 调 LLM + 执行 tool + 权限检查，context stats post-inflate 记录 |
| `ctx/truncate.rs` | 521 | **输出截断** — 按工具类型截断（bash 保错误行、read 出 skeleton）、>512B 外部化到 result_store (2026-04 从 turn/truncation.rs 迁入 ctx 模块) |
| `turn/permission.rs` | ~100 | **权限决策** — InteractivePermissionDecider（弹确认框）、AutoBypass/AutoDeny |
| `turn/json_repair.rs` | 439 | **JSON 修复** — 自动修复 JSON 语法错误（缺逗号、单引号、未闭合） |
| `turn/log.rs` | ~160 | **LLM 请求日志** — 每次 LLM 调用写 JSON 到 ~/.atomcode/logs/ |

### 语义层 — 代码理解

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `semantic/mod.rs` | 793 | **语义分析** — list_symbols / extract_symbol / skeleton / find_similar_calls / count_syntax_errors，支持 9 种语言 |
| `semantic/language.rs` | ~70 | **语言注册** — Lang 枚举 + grammar + symbols_query(.scm 文件) |
| `semantic/cache.rs` | ~80 | **AST 缓存** — 按 mtime 缓存 tree-sitter 解析树 |

### 基础设施

| 模块 | 行数 | 职责 |
|------|:----:|------|
| `config/mod.rs` | 286 | **配置** — Config/ProviderConfig 加载、DEFAULT_SYSTEM_PROMPT（124 行规则） |
| `config/prompt_sections.rs` | ~60 | **动态 prompt** — UNIFIED_PROMPT 构建 |
| `config/provider.rs` | 40 | **Provider 配置** — type/api_key/model/base_url/context_window |
| `project_context.rs` | 487 | **项目上下文** — 目录树扫描（3 层）+ tree-sitter 符号标注 + tech stack 探测 + 配置文件摘要 |
| `provider/openai.rs` | 401 | **OpenAI 兼容 Provider** — OpenRouter/硅基流动/本地 API 通信 |
| `provider/claude.rs` | 325 | **Anthropic Provider** — Claude API 通信 |
| `provider/ollama.rs` | ~150 | **Ollama Provider** — 本地模型通信 |
| `session.rs` | 280 | **会话管理** — 保存/加载/恢复对话历史 |
| `skill.rs` | 599 | **技能系统** — Markdown 技能文件加载、参数展开 |
| `stream/mod.rs` | ~80 | **SSE 流解析** — LLM 响应流处理 |

---

## atomcode-tuix 模块

> CC 风格的 normal-mode TUI（不进 alternate screen），retained-mode 渲染器（cell-level diff + 16ms tick）。详细设计见 `docs/superpowers/plans/2026-04-19-tuix-retained-mode-rewrite.md`。

| 模块 | 职责 |
|------|------|
| `lib.rs` | TUI 入口 — `run()` 启动事件循环、初始化 renderer |
| `event_loop/` | 主循环 — 事件分发、命令处理、buffer 管理、Modal 调度 |
| `render/` | 渲染层 — Cell/Screen buffer、diff、retained-mode 帧循环、theme |
| `modals/` | Modal 选择器 — dir/model/session/provider/issue/welcome 向导 |
| `input/` | 输入处理 — key action 映射、history、reader |
| `markdown.rs` | Markdown 渲染 — pulldown-cmark + syntect + CJK 间距 |
| `commands.rs` | 斜杠命令注册与分发 |
| `think.rs` | reasoning_content 流式渲染 |
| `state.rs` | UiState — 全局 UI 状态机 |
| `trace.rs` | datalog 记录 — 每轮对话写 Markdown |

---

## 数据流

```
用户输入
    ↓
task_classifier → 分类（BugFix/FeatureDev/...）
    ↓
"Analyze first" 前缀注入
    ↓
auto_diagnose → 扫描日志、提取异常、tree-sitter 代码定位
    ↓
conversation.to_provider_messages_budgeted() → hot/cold 分区
    ↓
inflate ToolResultRef → 恢复最近 20 个大 tool result
    ↓
TurnRunner.run() → 发给 LLM
    ↓
LLM 返回 tool_use → 执行工具
    ↓
file_history.backup() → edit/write 前备份
    ↓
tool 执行 → surrounding_context 附在结果末尾
    ↓
post_process → 截断 + 外部化到 result_store
    ↓
auto_compile_verify → 编译检查
    ↓
syntax_check_edited_files → tree-sitter 语法检查
    ↓
apply_post_turn_discipline → 步数提醒、重读检测
    ↓
继续下一轮 or finish_turn
```
