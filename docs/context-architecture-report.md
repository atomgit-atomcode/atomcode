# Context Architecture 深度分析报告

> Phase 4.5 Working Set Protection 设计基础
> 2026-04-06

---

## 一、当前 Context 组成与注意力布局

### System Prompt 结构（按注入顺序）

```
┌─────────────────────────────────────────────────────────┐
│ PRIMACY ZONE — 模型对开头内容注意力最高                     │
│                                                         │
│  1. Working dir + env info           (~100 tok)          │
│  2. Git branch + status              (~50 tok)           │
│  3. Recent activity (datalog)        (~30 tok)           │
│                                                         │
│  4. === PROJECT STRUCTURE ===        (~1500-3000 tok)    │
│     目录树 + 文件签名                                     │
│                                                         │
│  5. Pre-read context                 (0 tok, disabled)   │
│  6. .atomcode.md instructions        (0-500 tok)         │
│  7. Project knowledge                (~50-200 tok)       │
│  8. Previous session context         (~200-500 tok)      │
│                                                         │
│ RECENCY ZONE — 模型对结尾内容注意力次高                     │
│                                                         │
│  9. === RULES ===                    (~570 tok)           │
│ 10. Platform rules                   (0-100 tok)         │
│                                                         │
│ 合计 system prompt: ~2400-5000 tok                       │
└─────────────────────────────────────────────────────────┘
```

### Conversation 结构（按消息顺序）

```
┌─────────────────────────────────────────────────────────┐
│ COLD ZONE — 压缩后的历史                                  │
│                                                         │
│  [Session summary] turns 1-3: "用户问X，结果Y"            │
│  [User] 之前的任务描述                                    │
│  [Assistant] 之前的结果摘要                                │
│  ... (user+outcome pairs, 无 tool 细节)                  │
│                                                         │
│  预算: remaining_budget × 70% 中的剩余                    │
│  注意力: 低（lost in the middle）                          │
├─────────────────────────────────────────────────────────┤
│ HOT ZONE — 完整保留的最近 turns                            │
│                                                         │
│  保证最少 2 个 turn 完整保留                                │
│  预算: remaining_budget × 30%（但至少 2 turn 不受限）      │
│                                                         │
│  [User] 当前任务                                         │
│  [Assistant + ToolCalls] 模型的规划 + 工具调用              │
│  [ToolResult] 工具返回结果（完整内容）                      │
│  [System] PLAN BEFORE EDITING 注入                       │
│  [System] Stagnation warning（如果触发）                  │
│  ...                                                    │
│                                                         │
│  注意力: 高（recency bias）                               │
│                                                         │
│  ★ 最后一个 turn 的 ToolResult 保持完整                    │
│  ★ 更早 hot turn 的 ToolResult >500B 被 condensed        │
│  ★ create_file 的 content 被替换为 "[N lines, M bytes]"  │
├─────────────────────────────────────────────────────────┤
│ STREAMING — 当前生成中                                    │
│                                                         │
│  stream_buffer: 模型正在输出的文本                         │
│  注意力: 最高（正在生成）                                   │
└─────────────────────────────────────────────────────────┘
```

### 注意力分布问题

| 位置 | 注意力 | 放了什么 | 问题 |
|------|:------:|---------|------|
| 开头 | 高 | env/git/目录树 | ✅ 目录树在高注意力区，帮助文件定位 |
| 中间 | **低** | cold zone 历史 + 旧 tool results | ⚠️ 工作集文件内容如果在这里会被忽略 |
| 结尾 | 高 | RULES + 最近 turn | ✅ 规则在高注意力区 |

**关键洞察**：工作集文件的 ToolResult 在 hot zone 的"更早 hot turn"中，被 condensed（>500B → 摘要）。模型看到的是摘要而非全文。如果文件在 cold zone，只剩 user+outcome pair，连摘要都没有。

---

## 二、压缩机制详解

### 三级压缩

```
Level 1: Post-turn 外部化
  时机: 每个 UsedTools turn 结束后
  操作: ToolResult ≥512B → ToolResultRef（摘要 + 磁盘缓存）
  效果: conversation 对象变小，但 LLM 调用前会膨胀最近 20 条
  
Level 2: Turn-aware budgeting  
  时机: 每次 LLM 调用前（to_provider_messages_budgeted）
  操作:
    Hot zone (30% budget): 最近 ≥2 turns 完整保留
    Cold zone (70% budget): user + outcome 只保留，tool 细节丢弃
    Batch summary: 5+ cold turns → 合并为一条摘要
  效果: 长对话不会超出 context window

Level 3: Hot zone 内部压缩
  时机: 同 Level 2
  操作: 
    最后 1 turn: 完整保留（包括 ToolResult 全文）
    更早 hot turns: ToolResult >500B → condensed（摘要/骨架）
    create_file content → "[N lines, M bytes]"
  效果: 即使在 hot zone，旧 turn 的大文件内容也被压缩
```

### 压缩后的收益

| 收益 | 说明 |
|------|------|
| 长对话可用 | 20+ turns 不会 context 溢出 |
| 语义保留 | cold zone 保留 user+outcome，模型知道"做过什么" |
| 磁盘缓存 | ToolResultRef 的原始内容在磁盘上，理论上可恢复 |

### 压缩后的损失

| 损失 | 影响 |
|------|------|
| **工作集文件内容丢失** | 模型以为文件还在 context → 用旧内容 edit → old_string 不匹配 |
| **中间步骤丢失** | cold zone 只有 user+outcome → 模型不知道"怎么做到的" |
| **模型不知道发生了压缩** | 没有任何通知 → 模型做错误假设 |

---

## 三、缺失的能力

### 3.1 Working Set 感知（当前缺失）

**问题**：system prompt 没有"你正在编辑哪些文件"的信息。压缩后模型不知道哪些文件需要重读。

**已有但未利用的数据**：
- `files_edited_this_turn: Vec<String>` — 本 session 编辑过的文件（full path）
- `files_read_this_turn: Vec<String>` — 本 session 读过的文件
- `session_files: HashMap<String, PathBuf>` — 所有接触过的文件

这些数据在 agent loop 中维护，但从未注入到 system prompt 或 conversation 中。

### 3.2 ProjectSense — 项目配置推断（Phase 5）

**问题**：不知道项目的 build/dev/test 命令。硬编码 marker 检测已被证明不可靠。

**目标**：项目初始化时用 LLM 推断，缓存到 `.atomcode/project_config.json`。

### 3.3 压缩知识提取（全新概念）

**问题**：cold zone 的历史被压缩成 user+outcome，原始信息丢失。但这些信息里可能有有价值的知识：
- "pip install alembic 成功安装了 alembic" → 下次直接知道
- "SecurityConfig.java 改了 CORS 设置修复了 403" → 项目知识
- "npm run build 的输出显示 TypeScript 5.x" → 技术栈信息

当前这些信息在压缩时被丢弃。如果在压缩前提取关键知识存入 project knowledge，后续 session 就不用重新发现。

### 3.4 混合模型压缩接口（Phase 5 远期）

**概念**：用快模型做 context 压缩。当 conversation 需要从 30K 压缩到 15K 时，不是简单地砍消息，而是用快模型（qwen/haiku）生成一个语义压缩的摘要。

```
当前压缩: 
  30K tok → 删除 tool results → 保留 user+outcome → 15K tok
  信息损失: 60%

LLM 压缩:
  30K tok → 快模型摘要 → "用户要求统一7页面标题。读了Workflow参考样式。
  编辑了6个文件的标题区域。关键样式: px-6 py-2.5, text-base font-semibold。
  Workbench需要特殊处理（有项目卡片）。build 通过。" → 500 tok
  信息损失: 10%（语义完整，细节可重读）
```

这是 Phase 5 混合模型（per-turn 切换）的自然延伸——快模型不仅做探索 turn，也做 context 压缩 turn。

---

## 四、Working Set Protection 架构设计

### 4.1 当前方案（路径 C — 通知模型）

```
┌──────────────────────────────────────────────────────────┐
│ System Prompt                                            │
│                                                          │
│  ... (env, project structure, knowledge, rules) ...      │
│                                                          │
│  === WORKING SET ===                          [新增]      │
│  Files you're working on this session:                   │
│  - Workbench.vue (edited, 343 lines)                     │
│  - Workflow.vue (read, 374 lines)                        │
│  - schemas.py (edited, 250 lines)                        │
│  If their content is not in recent messages,             │
│  re-read before editing.                                 │
│                                                          │
│  === RULES ===                                           │
│  ...                                                     │
└──────────────────────────────────────────────────────────┘
```

**位置**：在 RULES 之前，recency zone 的边缘。模型在生成工具调用前会注意到。

**代价**：~50-100 tok（每个文件 ~10 tok：文件名 + 状态 + 行数）。10 个文件 = 100 tok。

**效果**：模型知道工作集 → 压缩后知道该重读什么 → 并行重读 → 省 1-2 turns。

### 4.2 未来演进路径

```
Phase 4.5 (现在):
  路径 C — System prompt 注入工作集文件列表
  代价: ~50 tok
  收益: 压缩后省 1-2 turns 重读
  复杂度: 30min

Phase 5.0 (混合模型):
  路径 C + ProjectSense
  快模型推断项目配置 → 注入 system prompt
  代价: 初始化 2-5s + ~100 tok
  收益: 正确的 build/dev/test 命令

Phase 5.2 (压缩知识提取):
  压缩前用快模型提取知识 → 存入 project knowledge
  "session 中安装了 alembic" → knowledge.md
  代价: 每次压缩 ~1s
  收益: 跨 session 不重复犯错

Phase 5.3 (LLM 压缩):
  用快模型做语义压缩（替代当前的 user+outcome 截断）
  代价: 每次压缩 2-5s
  收益: 信息损失从 60% 降到 10%
```

### 4.3 完整 Context Pipeline 架构图

```
用户消息
    │
    ▼
┌─────────────┐
│ Agent Loop   │
│              │
│  files_read  │──────────────────────┐
│  files_edited│──────────────────────┤
│  knowledge   │──────────────────────┤
└──────┬───────┘                      │
       │                              │
       ▼                              ▼
┌─────────────┐              ┌──────────────────┐
│ Conversation │              │ System Prompt     │
│              │              │ Builder           │
│  messages[]  │              │                   │
│  turn_tracker│              │  env + git        │
│  stream_buf  │              │  PROJECT STRUCT   │
│              │              │  .atomcode.md     │
│  ToolResult  │              │  knowledge        │
│  ToolResultRef│             │  prev session     │
│              │              │  WORKING SET [NEW]│
└──────┬───────┘              │  RULES            │
       │                      └────────┬──────────┘
       │                               │
       ▼                               ▼
┌──────────────────────────────────────────────────┐
│ to_provider_messages_budgeted()                   │
│                                                   │
│  Input: system_prompt + conversation.messages      │
│  Budget: 64K tokens                               │
│                                                   │
│  ┌─────────────────────────────────────────────┐  │
│  │ Phase 1: Hot Zone (30% budget, ≥2 turns)    │  │
│  │   最后 turn: 完整 ToolResult               │  │
│  │   更早 hot turns: condensed ToolResult      │  │
│  ├─────────────────────────────────────────────┤  │
│  │ Phase 2: Cold Zone (剩余 budget)            │  │
│  │   user + outcome pairs                      │  │
│  │   batch summary (5+ old turns)              │  │
│  │   edit tombstones（文件名保留）              │  │
│  ├─────────────────────────────────────────────┤  │
│  │ Phase 3: Assemble                           │  │
│  │   system + cold + hot                       │  │
│  │   sanitize broken pairs                     │  │
│  └─────────────────────────────────────────────┘  │
│                                                   │
│  Output: Vec<Message> + ContextStats              │
└──────────────────────┬───────────────────────────┘
                       │
                       ▼
              ┌─────────────────┐
              │ LLM Provider    │
              │ (GLM-5 / Qwen)  │
              └─────────────────┘
```

### 4.4 未来扩展点

```
用户消息
    │
    ▼
┌─────────────┐
│ Agent Loop   │
│              │
│  files_*     │──→ Working Set [Phase 4.5]
│  knowledge   │──→ Knowledge Store [existing]
│              │
│  on_compress │──→ Knowledge Extraction [Phase 5.2]
│              │    快模型从被压缩的 turns 提取知识
│              │    → project knowledge 自动增长
│              │
│  on_init     │──→ ProjectSense [Phase 5.0]
│              │    快模型推断 build/dev/test 命令
│              │    → .atomcode/project_config.json
│              │
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ Conversation  │
│               │
│  compress()   │──→ LLM Summarizer [Phase 5.3]
│               │    快模型语义压缩（替代 truncate）
│               │    30K → 500 tok 摘要
│               │    信息损失 60% → 10%
│               │
└───────────────┘
```

---

## 五、实施建议

### Phase 4.5 立即可做

**Working Set 注入**（30min）：
- 在 `build_system_prompt` 中追加 WORKING SET section
- 数据来源：`self.files_edited_this_turn` + `self.files_read_this_turn`
- 位置：RULES 之前（recency zone 边缘）
- 每个文件 ~10 tok，10 个文件 = 100 tok

### Phase 5 需要的基础设施

| 能力 | 依赖 | 实现 |
|------|------|------|
| ProjectSense | 混合模型（快模型调用） | 初始化时一次 LLM 推断 |
| Knowledge Extraction | 压缩时机 hook | `on_compress` 回调 + 快模型 |
| LLM Summarizer | 混合模型 | 替代 `build_batch_summary` |

所有三个都依赖**混合模型基础设施**（Phase 5.0）。混合模型是 Phase 5 的地基——快模型不仅做探索 turn，还做压缩、推断、知识提取。

### 架构原则

1. **System prompt 是全局知识层** — 项目结构、规则、工作集、知识库
2. **Conversation 是会话状态层** — 消息历史、工具结果、压缩策略
3. **压缩是信息变换，不是信息丢弃** — 每次压缩都应该提取知识到持久层
4. **快模型是压缩引擎** — 语义压缩 > 截断压缩
5. **注意力布局决定信息价值** — 高注意力位置放高价值信息（规则、工作集）
