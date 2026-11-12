# 连续思考预算

`reasoning_timeout_secs` 限制单次模型请求在没有正文或工具调用前连续思考的时长。
计时从首次思考输出开始，不包含等待首次输出的时间；持续的思考片段不会重置此预算。
它与流空闲超时独立，不会被发送为模型 API 的 `reasoning_effort` 参数。

```toml
[models."AtomGit-lumen-alpha"]
account = "AtomGit"
model = "lumen-alpha"
reasoning_timeout_secs = 600
```

旧格式的 `[providers.<名称>]` 同样支持该字段。优先级为有效的
`ATOMCODE_REASONING_TIMEOUT_SECS` 环境变量、选中模型的配置、默认 300 秒。
环境变量和模型配置的 `0` 都表示关闭连续思考预算。负数配置会被拒绝；
无效环境变量回退到模型配置或默认值。修改模型配置后需重新加载配置或重启客户端。

达到预算时使用独立错误码 `reasoning_output_timeout`，保存已有部分输出。
交互模式可选择继续恢复或单次降低思考强度；继续会发起恢复请求，
不会延长已经中断的连接。自动降低强度仍须显式启用。

关闭连续思考预算不会关闭首响应、流空闲或外部任务总时限，也不保证模型不陷入长思考。
300 秒是过渡默认值，不是经基准证明的最优值。

## DeepSWE 评测归档

评测工具保留 `events.jsonl`、`partial.patch`，并生成 `inference-diagnostics.json`
和 `candidate.json`。诊断记录错误码、实际声明的预算（若错误中存在）、配置预算来源、
任务总时限及已报告 token 用量；中断流可能没有最终用量，不能据此计算完整费用。

未完成候选标记为 `incomplete`，保留补丁 SHA256 和 base commit，正常评分文件
`model.patch` 保持为空。仅当进程和模型终态都成功时，候选才可进入官方评分。
这些归档便于诊断和后续独立评估，不会自动续跑或将中断候选计为通过。
