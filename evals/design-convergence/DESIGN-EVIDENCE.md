# 设计证据范围与复杂度候选（2026-10-27）

## 判断

未保留为生产提示词。针对“未知条件被扩大”“助手方案被当成已确认事实”的新候选，仍出现对应反例，未能建立稳定收益。候选只保留在评测文件 design_evidence_candidate.txt，persona.rs 未修改。

## 对照

从上一轮当前原生自然设计请求取相同 system persona（63017 字符），构造三个固定输入：自然初始两表设计、明确同启动可相减而跨重启未知、BootId 新证据必须允许必要扩表。每个输入 baseline/candidate 各两次，预先随机顺序，共12次。同一模型别名 deepseek-v4.1-flash-1017，reasoning_effort omitted，max_tokens=16384，单请求90秒限时。

baseline 与 candidate 各5次完成、各1次超时。超时未计通过，未选择性重跑。输入只重放 messages，不发布工具、不执行工具、不重放 private reasoning；不是完整原生 A/B，也不是用户原案。由本助手非盲检查，不能作模型优劣或统计质量结论。

## 可审查反例

- natural-start candidate 1 仍称服务端接收时刻为“唯一可比时钟”，并称“对外时长只算 received_at”。输入只有跨重启可比性未确认，不构成全面替换设备时间的依据。
- natural-start candidate 2 仍加入窗口 W、waiting/paired 状态以及 boot_id 预留与派生逻辑。候选并未可靠阻止针对未知可能性的结构扩张。
- local-uncertainty candidate 2 虽没有改用服务器耗时，仍加入派生 device_session 和 received_batch。不能把“没有违反一个时间规则”当作整体方案已简化。
- 两个 new-evidence candidate 都加入 BootId；但列集合与语义完整性仍须另审，不能据此推断候选总体有效。物理试管是否会跨重启经过两光电，仍不能靠不同 BootId 分行反推。

工件在 results/design-evidence-controls-20261027；完整输入、调度、可见输出、usage、失败均保留。模型后台权重未被证明不变。

## 确定性实现问题

并行排查 --no-tools：隔离 home + 本地假 provider 仍发布28个工具；正确 file_path 参数的 read_file 调用能把本测试 fixture 内容带回假模型。不是凭注释或 ToolStarted 猜测，且没有访问用户文件或外部模型。

因此前轮 FEEDBACK-REGRESSION.md 对 --no-tools 的限制现在有了确定复现。修复位于共用 CodingRuntime tree_inputs，用原有 tools 目录策略在全部用户/宿主层之后关闭目录，覆盖 prepare.tools=false，并覆盖重建。它改善评测隔离和 headless 契约，不宣称治愈 TUI 设计质量。验证与实现设计见 docs/plans/2026-10-27-model-only-catalog.md。

设计优化下一步仍应以自然讨论和固定失败输入验证，不把新提示词未经验证上线；已有获准设计文档的最小差异编辑工作流可作为可审查的局部改动方式，不能擅自创建第二份运行时事实状态。

无工具修复验证完成：120项运行时/挂载测试、26项脚本测试、三组原生离线探针、全仓与 feature 编译门、格式和 diff 检查均通过。工具关闭后目录为空且伪造调用不能读取 fixture；正常工具模式仍能读取。源码未提交推送。
