# AtomCode Best Practices

> 从实战中总结的设计原则，每条背后都有失败的教训。

---

## 1. 不要给弱模型加新工具，让旧工具自动变强

**教训：** 新四军计划创建了 3 个独立语义工具（list_symbols / read_symbol / find_references），模型一个都没用过。13 个工具的选择负担太大，弱模型倾向于用自己"熟悉"的工具。

**正确做法：** 将新能力融入现有工具，模型无需改变行为即可受益。
- `edit_file` 加了 `symbol` 参数 → tree-sitter 自动缩小搜索范围
- `edit_file` 加了 `start_line/end_line` 行号模式 → 彻底绕过 old_string 匹配
- `edit_file` 加了 fuzzy match 自动 fallback → 缩进错误自动修复
- `grep` 结果自动附带所在函数名 → 无需独立的 find_references 工具

**原则：被动增强 > 主动选择。最好的工具升级是用户（模型）感知不到的。**

---

## 2. 步数口径必须跟对标产品一致

**教训：** AtomCode 按 tool call 计数（每个工具调用 = 1 步），Claude Code 按 LLM 往返计数（一次 LLM 调用返回 3 个并行工具 = 1 轮）。同一个任务 AtomCode 显示 "43 步" vs Claude Code "15 轮"，差距被夸大了 3 倍。

**修复：** 改为 LLM 往返计数（turns），在 `PhaseChange(Thinking)` 事件时 +1。
- UI spinner: `[turn 4]` 而非 `[step 12]`
- datalog: `### Turn 4` 下列出该轮所有工具调用
- Stats: `9 turns, 160s` 而非 `25 steps, 160s`

**原则：度量不一致会导致所有优化决策偏离方向。**

---

## 3. 模型重复输出要从流程控制解决，不是事后去重

**教训：** 弱模型（glm-5）频繁在一次响应中输出两遍总结。先尝试了 `finalize_stream` 去重 → 用户已经在屏幕上看到重复了。再尝试了按段落匹配 → 单换行分隔的重复检测不到。

**正确做法：** 在 `StreamEvent::Delta` 处理中实时检测重复。一旦发现 buffer 后半段在重复前半段，立即：
1. 截断 buffer（只保留第一遍）
2. 终止 stream（`finalize_stream` + `finish_turn` + `return`）
3. 模型后续的废 token 被丢弃（stream drop）

检测条件：任意 ≥15 字符的行在后半段重复出现且后续 2+ 行也匹配。

**原则：流式输出的问题要在流式阶段解决，不能等到结束后补救。**

---

## 4. read_file 的 skeleton 前缀是负优化

**教训：** 大文件 read_file 返回 skeleton + 全文，期望模型看到 skeleton 后用 read_symbol 精准定位。实际结果：
- 模型无视 skeleton 提示，照样重读全文
- 多出 25 行 token 开销，每次读大文件都浪费
- 曾尝试只返回 skeleton 不返回全文 → 阻断了 read → edit 流程（模型没有原文就无法构造 old_string）

**正确做法：** read_file 不做任何额外处理，返回纯内容。语义能力通过 edit_file 的 symbol 参数和行号模式体现。

**原则：不要在读取端增加开销来"教育"模型，在编辑端增加容错来"兜底"模型。**

---

## 5. 拦截策略要精确，过度拦截比不拦截更糟

**教训：**
- 重读拦截 v1：全文和 offset 读都拦 → 模型用 offset 精准定位也被拦，导致后续 edit_file 失败，反而多了 2 轮
- 循环检测 v1：连续 4 次同名工具就终止 → 对同一文件连续做不同 edit_file 被误杀

**修复：**
- 重读拦截：只拦全文重读（第 3 次+），offset/limit 读永远放行
- 循环检测：从同名工具改为同参数 hash 工具，不同参数的连续调用不算循环

**原则：拦截规则误杀一次 = 浪费 2-3 轮（重读 + 重试）。宁可漏过，不可误杀。**

---

## 6. Context window 不是越大越好

**教训：** 从 16K 调到 64K，期望减少重读。实际：
- glm-5 推理时间跟 input 长度超线性增长，64K 直接卡死
- 旧轮的 read_file 全文（几千行代码）原样保留在 context 里，大量废 token

**正确做法：**
- 32K 是 glm-5 的甜蜜点（比 16K 多保留 1-2 轮完整内容，不至于卡死）
- 热区比例 40%（不是 60%）→ 旧轮更快进入压缩区
- 热区内非最新轮的 ToolResult 自动压缩为第一行摘要
- write_file 的 content 参数在旧轮中替换为 `[N lines, M bytes]`

**原则：context 管理的核心不是"放多少进去"，而是"最新的完整 + 旧的压缩"。**

---

## 7. 大文件写入要有实时进度反馈

**教训：** 模型生成 write_file 的 content 参数（可能几百行代码）时，用户看到的是静止的 `Preparing Write File... TTFT 15.9s`，不知道是卡了还是在生成。

**修复：** `ToolCallDelta` 事件中实时更新参数大小显示：
```
⠋ [turn 3] write_file: NotebooksView.vue (5.1KB)
⠸ [turn 3] write_file: NotebooksView.vue (12.4KB)
```

**原则：用户看不到进度 = 用户以为卡了。任何超过 3 秒的操作都需要进度反馈。**

---

## 8. 等待时间要用颜色传达紧急度

**修复：** Spinner 颜色随等待时间变化：
- 绿色 (<10s)：正常
- 黄色 (10-60s)：稍慢
- 橙色 (60-120s)：较慢
- 红色 (>120s)：很慢

加上动态点号动画（`waiting 15.2s...` → `waiting 15.2s....`），用户一眼判断"在等还是卡了"。

**原则：颜色是最快的信息通道，比文字快 10 倍。**

---

## 总结：优化的优先级

从这次实战中得到的优先级排序：

1. **Context 管理** — 压缩旧内容，控制发给 API 的 token 数（直接影响速度和成本）
2. **Edit 容错** — fuzzy match + 行号模式（直接减少失败 → 减少重试轮次）
3. **流程控制** — 重复检测终止 stream、拦截重读（防止浪费）
4. **被动增强** — grep 函数标注、edit_file symbol（不改变模型行为就能受益）
5. **UI 反馈** — spinner 变色、进度显示（不影响功能但提升体验）
6. **System prompt** — SCOPE 规则、禁止中途总结（有效但依赖模型遵守）
