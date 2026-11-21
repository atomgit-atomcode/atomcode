# 连续思考不设时间上限

客户端不再按累计思考时长中断请求。只要模型持续产生思考内容，可以继续等待正文或工具调用，
不会弹出“思考等待上限”、增加长思考提醒或因思考时间长自动降低思考强度。

原有 `reasoning_timeout_secs` 模型/provider 配置字段会被忽略，不再序列化。
`ATOMCODE_REASONING_TIMEOUT_SECS` 和 `ATOMCODE_REASONING_RECOVERY_TIMEOUT_SECS`
环境变量不再读取；原有超时后的手动/自动恢复路径已移除。

首响应与流空闲超时仍然生效，思考内容会刷新流空闲计时。只有心跳或元数据不算内容进展。
用户取消、外部任务总时限、输出 token 上限和重复思考检测仍各按原有规则执行。
此改动不改变模型的 `reasoning_effort`，也不保证模型不会产生无效的长思考。

## DeepSWE 评测归档

评测工具保留 `events.jsonl`、`partial.patch`，并生成 `inference-diagnostics.json`
和 `candidate.json`。诊断记录错误码、实际声明的预算（若错误中存在）、配置预算来源、
任务总时限及已报告 token 用量；中断流可能没有最终用量，不能据此计算完整费用。

未完成候选标记为 `incomplete`，保留补丁 SHA256 和 base commit，正常评分文件
`model.patch` 保持为空。仅当进程和模型终态都成功时，候选才可进入官方评分。
这些归档便于诊断和后续独立评估，不会自动续跑或将中断候选计为通过。
