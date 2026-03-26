# 新四军计划 — AtomCode v2.3.0

> 核心武器：Ripgrep 找位置 + Tree-sitter 抠语义。
> 战略转向：不让模型选新工具，让旧工具自动变强。

---

## Day 1 — 建制：框架 + 9 语言接入 ✅

- [x] 新建 `crates/atomcode-core/src/semantic/` 模块
  - `mod.rs` — SemanticSearcher 入口
  - `language.rs` — LanguageRegistry，file_ext → (Language, Grammar, QuerySet)
  - `cache.rs` — ASTCache: HashMap<PathBuf, (Tree, ModTime)>
- [x] Cargo.toml 引入 tree-sitter + 8 语言 grammar
- [x] LanguageRegistry 注册 8 语言 + Vue/Svelte 支持
- [x] 8 套 `.scm` query 文件
- [x] Vue SFC 支持：提取 `<script>` 段用 TypeScript parser 解析，偏移行号自动修正
- [x] 未识别语言 fallback 到缩进级别分析

---

## Day 2 — 武装：融入现有工具（策略转向） ✅

**原计划：** 3 个独立工具（list_symbols / read_symbol / find_references）
**实际执行：** 创建后发现弱模型不会主动使用新工具（13 个工具选择负担太大）。
**战略转向：** 移除独立工具，将语义能力融入现有工具，模型无需改变行为即可受益。

- [x] `edit_file` + `symbol` 参数 — tree-sitter 自动缩小搜索范围
- [x] `edit_file` + `start_line/end_line` 行号模式 — 彻底消除 old_string 匹配失败
- [x] `edit_file` fuzzy match 自动 fallback — 缩进错误自动修复
- [x] `grep` 结果附带所在函数名 `← in function_name()`
- [x] 工具数量：13 → 10（移除 list_symbols / read_symbol / find_references）

---

## Day 3 — 作战：Agent 纪律 + 上下文优化 ✅

- [x] `search_replace` 改为 AutoApprove + system prompt 引导批量替换
- [x] 步数口径对齐 Claude Code：从 tool call 计数改为 LLM 往返计数（turns）
- [x] 重读拦截：全文重读第 3 次拦截，offset 读放行
- [x] 循环检测修复：从同名工具改为同参数工具
- [x] SCOPE 规则强化：禁止读无关文件
- [x] 禁止中途总结：summary 必须是最后一步
- [x] write_file 规则强化：大改动必须拆 edit_file
- [x] `[continuing...]` 分隔符：text + tool_call 混合响应时提示用户

---

## Day 4 — 扫荡：Token 瘦身 + 体验优化 ✅

- [x] 流式重复检测 — 模型重复输出总结时实时截断 stream 并终止
- [x] Response 去重 — finalize_stream 中标记行检测去重
- [x] 删除 `maybe_inject_summary_prompt` — 消除代码注入导致的双重总结
- [x] 重复 TextDelta 修复 — step limit / force-stop 路径统一走 push_delta
- [x] Context window 64K + 热区 40% — 保留更多上下文但不过度膨胀
- [x] UI spinner 变色（绿→黄→橙→红）+ 参数大小实时显示
- [x] `/config` macOS 用 `open -t` 打开，不阻塞 TUI
- [x] CdTool 移除 — LLM 不能切 working_dir
- [x] Stream 路径死代码清理 — 删 462 行

---

## 实战效果（devpress2.0 项目，glm-5 模型）

| 指标 | 优化前 (v2.1.0) | 优化后 (v2.3.0) | 变化 |
|------|----------------|----------------|------|
| 步数口径 | tool call 计数（虚高） | LLM 往返计数（对齐 Claude Code） | 修正统计 |
| 全局样式修改 | 43 步 → 实际 ~18 轮 | 9 轮 | **2x** |
| 功能实现（社区） | 25 步 → 实际 ~12 轮 | 9 轮 | **1.3x** |
| old_string 失败率 | 每轮 ~20% | 降低（fuzzy + 行号模式） | 减少重试浪费 |
| Response 重复 | 频繁 | 实时截断 | 基本消除 |
| 大文件等待体验 | 静止不动 | 实时显示参数大小 + 颜色变化 | 体验提升 |

## 关键教训

1. **不要给弱模型加新工具** — 它不会用。把能力融入已有工具（edit_file symbol=, 行号模式）。
2. **read_file skeleton 前缀是负优化** — 增加 token 但不改变行为。已回退。
3. **步数对比口径要一致** — tool call 数 vs LLM 往返数差 2-3 倍，之前一直在用错误口径评估。
4. **Context window 是杠杆** — 16K→64K 直接减少重读，但需要控制热区比例避免 API 变慢。
5. **重复总结要从流程控制解决** — 不是去重，是实时检测到重复就终止 stream。

## 与 Claude Code 的差距

| 维度 | 差距 | 可缩小？ |
|------|------|---------|
| 步数（同口径） | AtomCode ~9 轮 vs Claude Code ~5 轮 | 部分可（更好的上下文管理） |
| old_string 精度 | 有 fuzzy+行号兜底 | 已基本解决 |
| 工具发现能力 | 弱模型不会用新工具 | 已通过融入旧工具绕过 |
| Context window | 64K vs 200K | 配置可调，模型支持 |
| 模型推理质量 | glm-5 跑偏/重复 | 换模型（deepseek/claude）可解 |
