# 缓存友好的历史压缩(Cache-Friendly History Compaction)

- **日期**: 2026-06-09
- **分支**: `release/v4.25.1`(worktree:`atomcode-v4.25.1`)
- **状态**: 设计已评审通过,待写实现计划
- **范围**: 公共底座(①)。`summary 折叠最老轮`(②)列为后续可选,不在本次。
- **目标文件**: `crates/atomcode-core/src/ctx/render.rs`(主),`crates/atomcode-core/src/agent/mod.rs`(渲染调用点)

---

## 1. 背景与问题

### 1.1 现象(生产数据,deepseek-v4-flash,1M 窗口)

- 整体 prompt 缓存命中率 4.25.0 ≈ **97.4%**(历代最好)。
- 剩余未命中按 OBS 字节级 diff 归因(216 对 good→bad,2026-06-09):
  - **历史 tool 结果被改写/截断 ~60%** + **read_file 结果被压 ~17%** ≈ **77% 客户端可修**;
  - 上游节点切换 ~16%(不可控);system 注入 ~7%。
- 对 tool-stub 这 108 对(14.3M 未命中)按断点位置(以 `bad_hit` 为代理)再拆:

  | 断裂severity | 对数 | 未命中 | 占 tool-stub |
  |---|--:|--:|--:|
  | 断点靠前 `bad_hit<10%`(缓存几乎全塌) | 62 | 7.6M | **53%** |
  | 严重 10–30% | 29 | 5.5M | 39% |
  | 尾部小断裂 30–50% | 17 | 1.2M | 8% |

  → **92% 的未命中来自"断点靠前、大面积塌"的断裂**,只有 8% 是便宜的尾部断裂。

### 1.2 窗口不是配错的

`~/.atomcode/config.toml` 默认 provider `AtomGit-deepseek-v4-flash` 的 `context_window = 1000000`;运行时 budget 取 `ProviderConfig.context_window`(codingplan 登录时从服务端 `models-v2` 落库,`None/0` 才回落 `CONTEXT_WINDOW = 64_000`,见 `coding_plan/setup.rs:1127`)。坏 case 的会话单轮重算 0.8M token,独立证明生产客户端确实跑在 ~1M 窗口。**所以"调大窗口"不是解法。**

## 2. 根因

两条 stub 历史 tool 结果的路径都会断前缀缓存(因为一条已发给模型的历史 ToolResult 字节被改 → 供应商前缀缓存从那条起整段失效;断点越靠前,失效越多):

1. **`microcompact`**(`render.rs:925`)—— **ephemeral**(每次渲染对临时 `result` Vec 改写、**不落库**),阈值 `70% × budget × 4` 字符(≈700K token),锚定最后一条 `Role::User`(活跃轮全文),压更老的非 `read_file`、`>MIN_COLLAPSE_SIZE(500B)` 的 ToolResult。
   - **病根**:每轮从全文重推 stub 集合 + 700K 阈值上下抖动 → 同一段靠前历史在 full↔stub 间反复横跳 → 每抖一次就炸一次前缀。这是 60% 的来源。
2. **`compact_old_tool_results_in_place`**(`render.rs:869`)—— **落库、幂等**,但**只在应急路径** `emergency_compact_to_target`(`agent/mod.rs:2936`,逼近 `auto_compact_threshold(1M)≈987K` 才触发)被调用,且**不豁免 `read_file`**(压 read_file → 17%)。

**关键空隙**:700K–987K 这段,**只有 ephemeral 的 microcompact 在跑、没有任何东西落库** → 复发断裂。

## 3. 目标 / 非目标

### 目标
- 让历史 tool 结果的 stub **落库、幂等、单调冻结**:一条一旦 stub 成型,永久是同一串字节、再不回改 → 前缀逐字节稳定 → 把"每会话反复的靠前断裂"降为"每会话最多一次"。
- 不牺牲模型可见保真度(与今天 microcompact 在 wire 上的可见度一致:活跃轮全文,更老压 stub)。
- **不引入超窗被 litellm 拒绝的回归**。

### 非目标(列为后续)
- **②**:消除"巨型会话首次跨阈值那一次靠前断裂"(summary 折叠最老轮进冻结的 `cold_summaries` 前缀)。本次先上线、用现有脚本量残留,痛再做。
- Tier-2 LLM summary 的"逐轮重写摘要"问题(独立 compaction 根因)。
- 上游节点切换(不可控)。

## 4. 设计

### 4.1 合并为一个落库压缩函数

改造 `compact_old_tool_results_in_place`,新增一个 `exempt_read_file: bool` 形参(不另起函数,避免两份 stub 逻辑漂移):
- **`read_file` 豁免(仅正常路径)**:正常落库压缩传 `exempt_read_file = true`,遇到 `read_file` 的 ToolResult 一律跳过——压成 `first: 205| pub async fn …` 会让模型"伪自信"反复改同一文件(5-7 atomgr 实证)。**应急路径 `emergency_compact_to_target` 仍传 `false`**(保持现状):真到 ~987K 预算压力时,释放 read_file 字节比保缓存更要紧,否则会更早跌到 Tier-3 硬截断(丢整轮,对用户更糟)。豁免逻辑同 `microcompact:989`。
- **边界 `keep_recent_turns = 1`**:只保活跃轮(最后一条 `Role::User` 之后)全文,更老的非 read_file、`>MIN_COLLAPSE_SIZE` 的 ToolResult 压成 `[<tool> ok|FAILED: N lines, first: <80c>]`(`build_compact_stub`,`render.rs:823`)。这与今天 microcompact 在 wire 上的可见保真度**完全一致**。
- **幂等**:已是 stub(`<MIN_COLLAPSE_SIZE`)的跳过,重复跑字节不变(现有 `compact_old` 已具备)。
- **同一阈值**:仍 `70% × budget × 4` 字符(≈700K token);低于阈值**完全不压** → 小会话全文 + 纯追加 = 满命中。**阈值门控放在正常渲染调用方**(`if 估算 > 阈值 { compact_old(conv, keep_recent=1, exempt_read_file=true) }`);`compact_old` 自身不含阈值,应急 Tier-1 仍无条件调用、不受此门控影响。

### 4.2 何时落库、在哪渲染

- **每次渲染前评估一次**(对齐 microcompact 原有的"每渲染检查"节奏),作用于 `self.conversation.messages`(落库)。因为**幂等 + 单调**(只会越压越多、已压字节不变),每渲染前都跑对缓存是安全的:已压老轮永远是同一串 stub,不会在 full↔stub 间抖;跨轮只新增"刚结束那一轮"被压,断点落尾部、压完即冻。
- **`build_messages` 回归纯渲染**:删除其内部 microcompact 调用(`render.rs:313`)。落库压缩作为独立的 `&mut conv` 步骤,在正常渲染路径里、`build_messages` 之前调用(估算路径如 `compression.rs:195`、各测试的 `build_messages(&conv,…)` 不受影响,签名不变)。
- **保留 `FINAL BYTE CEILING`(`render.rs:372`,80% 硬钳位)**:这是真正"发出去 ≤ 窗口"的 last-line-of-defense,独立于 microcompact、无条件执行,**不动**。
- **应急路径 `emergency_compact_to_target` 不动**:Tier-1(keep_recent=3,`exempt_read_file=false`)→Tier-2(LLM summary)→Tier-3(truncate)保留。正常轮已把 conv 压到 keep_recent=1,应急 Tier-1 多为 no-op、自然下沉到 Tier-2,行为更顺,零额外改动。
- **read_file 的 17% 间接下降**:今天 read_file 被压只来自应急路径(microcompact 本就豁免)。正常落库压缩让 `conv.messages` 永久变小 → 触及 ~987K 应急阈值的频率大幅降低 → 应急(及其 read_file 压缩)触发远少 → 17% 这块随之下降,**无需在应急路径豁免 read_file**。

### 4.3 删除 microcompact

- 删除 `microcompact` 函数(`render.rs:925`)及其调用(`render.rs:313`)。其独特行为(read_file 豁免、活跃轮全文)已迁入落库压缩,无独立存在价值,且它正是字节漂移之源。
- 迁移/改写其测试到落库压缩上(见 §6)。

## 5. 超窗安全性分析(回应 litellm 拦截顾虑)

**结论:不新增超窗风险。**

1. 真正保证 wire ≤ 窗口的是 `FINAL BYTE CEILING`(`render.rs:372`):每渲染对 `result` 做"最老优先、逐条 `condensed()`、压到 ≤80% budget 即停",last-4 / Text / AssistantWithToolCalls 永不动。**独立于 microcompact、无条件执行、保留不动。**
2. microcompact 的 old-turn 压缩职责由落库压缩接管,**触发点一致(70%)**,且按"每渲染前"评估,一比一保留原有的每渲染超窗保护。
3. 落库后预算估算(`estimate_tokens` over `conv.messages`,`agent/mod.rs:2928`)与真实 wire 对齐(今天 microcompact 只压 wire、conv 仍全文,两者偏差),超窗判断**更准**。
4. **唯一不变的残留风险**:活跃轮 last-4/Text/ATC 这些永不压内容本身超窗(如本轮读超大文件)→ 80% 钳位压不下去。**今天就存在**(microcompact 同样不碰当前轮),最终由应急 Tier-3 硬截断兜底,**非回归**。

## 6. 测试策略

### 新增(核心验收 —— 今天缺这条,所以 bug 能溜)
- **前缀字节稳定回归测试**:构造跨 700K 阈值的多轮会话,渲染第 N 轮与第 N+1 轮,断言**两轮渲染结果的公共前缀到"压缩边界"为止逐字节相同**,新增内容只在边界之后追加。

### 迁移/保留
- `read_file` 在落库路径被豁免(从 `microcompact_skips_read_file_to_preserve_long_session_context` 迁移)。
- 低于阈值 no-op、全文(从 microcompact 阈值测试迁移)。
- 活跃轮(最后一条 User 之后)保持全文。
- 幂等:重复跑不二次改写(已有 `microcompact_is_idempotent_no_double_stub` / `compact_old` 幂等测试)。
- `call_id` / `success` 标志在 stub 后保留(已有 `collapse_preserves_call_id_and_success_flag`)。
- 80% `FINAL BYTE CEILING` 行为不变(已有相关测试)。

## 7. 影响面

| 文件 | 改动 |
|---|---|
| `ctx/render.rs` | 删 `microcompact`(:925)及调用(:313);`compact_old_tool_results_in_place` 加 `exempt_read_file: bool` 形参(豁免逻辑同 :989);保留 `FINAL BYTE CEILING`(:372);迁移测试 |
| `agent/mod.rs` | 正常渲染路径每渲染前:`if 估算 > 70%×budget×4 { compact_old(&mut conv, keep_recent=1, exempt_read_file=true) }`,再调 `build_messages`(纯渲染);应急路径 Tier-1 改为显式传 `exempt_read_file=false`(行为不变) |

## 8. 上线与度量

- 上线后用 `cache-hit-rca` skill 的 `find_pairs` → `classify_pairs` 复测 4.25.x:确认 tool-stub 类未命中占比下降、`bad_hit<10%` 的靠前断裂大幅减少。
- 若"首次靠前断裂"残留仍显著,再评估 **②**(summary 折叠)。

## 9. 关键不变量(实现必须守住)

1. **单调**:落库压缩只把 ToolResult full→stub,从不反向;已 stub 的不再变字节。
2. **冻结边界**:活跃轮(最后一条 User 之后)永远全文且不被压;边界之前一旦 stub 即冻结。
3. **read_file 永不被落库压缩压**。
4. **wire ≤ 窗口由 `FINAL BYTE CEILING` 保证**,与压缩策略解耦。
