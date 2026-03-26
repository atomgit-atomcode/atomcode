# TODO v2.3.0 — Tree-sitter 语义检索（上下文工程）

> 核心目标：Agent 步数从 6-8 步降到 2-3 步，Token 消耗降低 100-200 倍。
> 方案：Ripgrep 找位置 + Tree-sitter 抠语义，三级漏斗模型。

---

## 实施计划（4 天）

### P1 — 全量实施（3 天）

- [ ] **Day 1: 框架 + 8 语言接入**
  - 新建 `crates/atomcode-core/src/semantic/` 模块
  - `mod.rs` — SemanticSearcher 入口
  - `language.rs` — LanguageRegistry，file_ext → (Language, Grammar, QuerySet)
  - `cache.rs` — ASTCache: HashMap<PathBuf, (Tree, ModTime)>，支持增量解析
  - Cargo.toml 引入 tree-sitter + 8 语言 grammar crate
  - 8 语言全部注册：Rust/Python/JS/TS/Go/Java/C/C++
  - 未识别语言 fallback 到缩进级别分析

- [ ] **Day 2: 三个新工具 + 8 语言 query**
  - `read_symbol` — 精准提取函数/类/结构体完整体
  - `list_symbols` — 返回文件符号表（签名 + 行号范围）
  - `find_references` — grep 找文件 + tree-sitter 区分 definition/call/import
  - 8 套 `.scm` query 文件（symbols.scm + skeleton.scm per language）
  - 注册到 ToolRegistry

- [ ] **Day 3: 现有工具增强 + 联调**
  - `read_file` 大文件（>500行）自动返回 skeleton + 提示用 read_symbol
  - `grep` 结果附带所在函数名
  - 未覆盖语言 fallback 到缩进分析兜底
  - 用 atomcode 开发 atomcode 验证步数下降

### P2 — Token 优化（1 天）

- [ ] **Day 4: edit 回传 + 文件去重**
  - edit_file 回传从全文改为 diff + 10 行上下文
  - 对话级文件去重：同文件只保留最新版本

---

## 步数预期

| 场景 | v2.1.0 | v2.3.0 |
|------|--------|--------|
| 修复单函数 bug | 8 步 / 80K tokens | 3 步 / 5K tokens |
| 理解文件结构 | 3 步 / 40K tokens | 1 步 / 2K tokens |
| 跨 3 文件重构 | 12 步 / 120K tokens | 4 步 / 8K tokens |
| 追踪调用链 | 5 步 / 50K tokens | 1 步 / 2K tokens |

## 架构图

见 `doc/architecture-v2.3.0.md`

## 关键约束

1. **Tech-stack agnostic** — tree-sitter 是语言无关层，per-language query 是数据配置
2. **渐进式增强** — 新增工具不改现有接口，无 grammar 时 fallback
3. **8 语言内嵌** — Rust/Python/JS/TS/Go/Java/C/C++，覆盖 90%+
