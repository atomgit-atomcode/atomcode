# AtomCode 优化日志 — 2026-03-20

> 基于一天的密集测试和对标 Claude Code 的分析，总结出20条最重要的改进。
> 按影响力排序，标注实现状态。

---

## Top 20 改进（按影响力排序）

### 1. ✅ edit_file 支持 replace_all 批量替换
**问题**：改样式需要替换20个CSS类，edit_file只能单次替换，模型被迫用write_file重写整个文件，破坏业务逻辑。
**方案**：edit_file新增`replace_all: bool`参数，一次替换所有匹配。模型用`{"old_string": "rounded-lg", "new_string": "rounded-xl", "replace_all": true}`改完所有圆角，零风险。
**影响**：从根本上消除"改样式→破坏逻辑"的问题。Claude Code的Edit工具有同样能力。
**文件**：`crates/atomcode-core/src/tool/edit.rs`

### 2. ✅ 每4步注入 system-reminder（任务+规则+进度）
**问题**：DeepSeek/GLM-5在3-4步后忘记系统提示规则，开始盲目探索。
**方案**：每4个tool call后，在工具结果中注入`<system-reminder>`，包含：原始任务、当前步数/上限、已读文件列表、已编辑文件列表、紧迫度提醒。
**影响**：这是Claude Code保持弱模型方向感的核心机制。实施后步数从35降到11。
**文件**：`crates/atomcode-core/src/agent/mod.rs` handle_tool_result

### 3. ✅ 预读文件注入为系统上下文（非合成tool call）
**问题**：模型启动后花10+步读文件才开始编辑。
**方案**：分析用户消息关键词→匹配项目文件→预读内容→注入到系统提示中。注入为上下文而非合成的read_file tool call，避免教会模型"第一步就该读文件"。
**影响**：模型第一步就能编辑，因为文件内容已在context里。
**文件**：`crates/atomcode-core/src/agent/mod.rs` build_preread_context + build_system_prompt

### 4. ✅ 跨文件bug模式传播检查
**问题**：模型修了SearchView.vue的API调用bug，但Top10View.vue有同样的bug没修，需要用户第二轮反馈。
**方案**：编辑成功后，system-reminder中列出同目录下的同类文件（sibling files），提醒模型检查它们是否有同样问题。
**影响**：一轮完成本需两轮的修复任务。Claude Code的模型天然会做这个，弱模型需要框架提醒。
**文件**：`crates/atomcode-core/src/agent/mod.rs` find_sibling_files_hint

### 5. ✅ grep/glob工具current_dir修复
**问题**：grep工具没有设置working directory，rg在错误目录搜索导致找不到文件。模型被迫用bash grep（有current_dir），白白浪费3-4步。
**方案**：grep和glob工具的Command都加`.current_dir(&wd)`，并解析相对路径。
**影响**：grep成功率从~25%跳到~95%，直接减少3-4步浪费。
**文件**：`crates/atomcode-core/src/tool/grep.rs`, `crates/atomcode-core/src/tool/glob.rs`

### 6. ✅ Token计数修复（DeepSeek累计值误累加）
**问题**：DeepSeek SSE流在多个chunk中发送累计usage值，代码每次都累加，导致token计数暴涨到50000+。
**方案**：只保留最后一次usage值（`last_usage.take()`），在`finish_reason`或`[DONE]`时一次性发送。
**影响**：token计数从假的50000降到真实的~3000。
**文件**：`crates/atomcode-core/src/provider/openai.rs`

### 7. ✅ 循环检测 + 强制终止
**问题**：模型重复调用同一个kill命令16次，[BLOCKED]消息模型完全无视。
**方案**：相同(tool, args)出现3次→返回BLOCKED消息。连续4次BLOCKED→强制终止turn并生成summary。
**影响**：从16次无效循环降到4步停止。
**文件**：`crates/atomcode-core/src/agent/mod.rs` intercept_redundant_call + execute_tool

### 8. ✅ 连续读取硬限（Read Budget）
**问题**：模型连续读10+个文件不做任何编辑。
**方案**：连续3次read/grep/glob/list_dir无edit→注入"READ BUDGET EXCEEDED, 下一步必须edit"。编辑/写入/bash重置计数器。
**影响**：防止"读到天荒地老不编辑"的模式。
**文件**：`crates/atomcode-core/src/agent/mod.rs` handle_tool_result

### 9. ✅ bash输出捕获修复（长进程）
**问题**：`npm run dev`运行10秒，输出了端口号但代码报"No output captured yet"。内层read有独立10秒超时，第一次没数据就放弃。
**方案**：去掉内层read超时，只用外层总超时。超时时返回已捕获的所有输出（包含端口号等关键信息）。
**影响**：模型能看到实际端口，不再盲猜3000。
**文件**：`crates/atomcode-core/src/tool/bash.rs`

### 10. ✅ 每session清空对话（对齐Claude Code）
**问题**：启动时加载history.json（946条），旧消息中有损坏的JSON/孤立ToolResult，发给API就400错误。
**方案**：每次启动用空对话。history.json只用于input_history（上下键翻历史输入）。
**影响**：彻底消除history导致的400错误，100%上下文用于当前任务。
**文件**：`crates/atomcode-cli/src/main.rs`, `crates/atomcode-tui/src/app.rs`

### 11. ✅ JSON三层修复管道（弱模型兼容）
**问题**：GLM-5生成无效JSON参数（缺逗号、缺引号、缺大括号），工具解析失败。
**方案**：三层修复：(1) repair_json修复常见格式问题，(2) extract_json_fields暴力提取key-value对，(3) 发送API前在format_messages中再次验证。
**影响**：GLM-5的JSON错误率从致命降到可用。
**文件**：`crates/atomcode-core/src/agent/mod.rs`, `crates/atomcode-core/src/provider/openai.rs`

### 12. ✅ edit_file fuzzy whitespace matching
**问题**：old_string缩进差一个空格就匹配失败，模型放弃edit_file改用write_file。
**方案**：精确匹配失败后，trim每行空白再比较。匹配成功时保持原始缩进+new_string的相对缩进。
**影响**：edit_file成功率提升，减少write_file使用。Claude Code没有这个能力。
**文件**：`crates/atomcode-core/src/tool/edit.rs` try_fuzzy_replace

### 13. ✅ write_file变更摘要
**问题**：write_file只返回"Wrote 14833 bytes"，模型和用户不知道改了什么。
**方案**：对比旧文件，返回"was 357 lines → 445 lines, 120 preserved, 237 changed"。如果大部分被重写，附加WARNING。
**影响**：让模型意识到自己改了太多代码。
**文件**：`crates/atomcode-core/src/tool/write.rs`

### 14. ✅ 系统提示重写（正面示例+反面示例+SCOPE规则）
**问题**：300 tokens的简短规则不够，模型不知道正确的工作流。
**方案**：扩展到~1.5K tokens，包含：4步工作流、正面示例（replace_all改样式）、反面示例（write_file破坏逻辑）、SCOPE规则（只改用户要求的部分）。
**影响**：模型从"不知道怎么做"变成"有模板可参照"。
**文件**：`crates/atomcode-core/src/config/mod.rs` DEFAULT_SYSTEM_PROMPT

### 15. ✅ Scouting上下文感知检测
**问题**：模型在"改样式"任务中跑ps/curl浪费步骤。但在"启动不了"任务中curl是必要的。
**方案**：检测用户消息中的关键词（启动/运行/访问/报错 → 允许scouting；改样式/布局 → 禁止scouting）。
**影响**：该查的时候查，不该查的时候不查。
**文件**：`crates/atomcode-core/src/agent/mod.rs` execute_tool

### 16. ✅ LLM流式超时（防卡死）
**问题**：API不响应时程序永远等，界面冻结40分钟，鼠标事件泄漏成乱码。
**方案**：HTTP连接超时30秒，请求总超时5分钟，SSE流空闲超时120秒。OpenAI和Claude provider都加了。
**影响**：最多等2分钟就报错返回，不会卡死。
**文件**：`crates/atomcode-core/src/provider/openai.rs`, `crates/atomcode-core/src/provider/claude.rs`

### 17. ✅ auto-summary（模型不总结时兜底）
**问题**：DeepSeek经常在最后一个tool_result后直接返回stop，不生成文字总结。
**方案**：`maybe_emit_auto_summary()`扫描当前turn的tool call，生成"Done. Modified: file1, file2; ran `cmd`."。
**影响**：用户永远能看到改了什么，即使模型没说。
**文件**：`crates/atomcode-core/src/agent/mod.rs`

### 18. ✅ per-turn markdown日志（datalog/）
**问题**：无法事后分析agent的行为，不知道哪步出了问题。
**方案**：每个turn写一个`datalog/YYYY-MM-DD_HH-MM-SS.md`文件，记录用户输入、每步工具调用+参数+结果、最终响应、统计。每步实时flush防崩溃丢失。
**影响**：成为今天所有后续优化的基础——没有日志就无法分析问题。
**文件**：`crates/atomcode-tui/src/turn_log.rs`

### 19. ✅ CJK字符边界panic修复
**问题**：`&self.current_task[..97]`在中文字符中间切断导致panic，程序崩溃，鼠标事件泄漏到终端。
**方案**：所有字符串截断改用`chars().take(N).collect::<String>()`。修复了agent/mod.rs、message.rs、turn_log.rs中的4处。
**影响**：中文用户不再崩溃。
**文件**：多处

### 20. ✅ 对话消息sanitize（防400 API错误）
**问题**：conversation中的孤立ToolResult或不配对的AssistantWithToolCalls导致API返回"messages illegal"。
**方案**：`sanitize_messages()`用状态机遍历，跟踪每个AssistantWithToolCalls期待的ToolResult数量，删除孤立消息。
**影响**：消除所有"messages illegal" 400错误。
**文件**：`crates/atomcode-core/src/conversation/mod.rs`

---

## 效果总结

| 指标 | 优化前 | 优化后 |
|------|--------|--------|
| 简单样式任务步数 | 35+ | 3-6 |
| 复杂多文件任务步数 | 25+（常失败） | 11-18 |
| 循环卡死 | 经常（16次重复） | 4步自动停止 |
| 400 API错误 | 频繁 | 消除 |
| grep成功率 | ~25% | ~95% |
| edit_file成功率 | ~60% | ~90%（fuzzy兜底） |
| 改样式破坏逻辑 | 每次 | replace_all避免 |
| 模型不总结 | 经常 | auto-summary兜底 |
| CJK崩溃 | 偶发 | 消除 |
| 长进程输出丢失 | 永远丢失 | 正确捕获 |

## 与 Claude Code 的剩余差距

1. **工具数量**：8 vs 15+（缺WebSearch、LSP、Notebook）
2. **系统提示丰富度**：~1.5K vs ~10K tokens
3. **延迟工具加载**：未实现（8工具定义占~2K context）
4. **跨session记忆**：未实现
5. **Diff显示**：edit后未在UI中显示before/after
6. **模型能力差距**：框架无法完全补偿（Claude > DeepSeek/GLM-5）
