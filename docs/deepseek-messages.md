# DeepSeek Messages（可选）

现有 `type = "openai"` 配置保持不变。需要原生 Messages 协议时另建 provider：

```toml
[providers.deepseek-native]
type = "deepseek-messages"
api_key = "${DEEPSEEK_API_KEY}"
base_url = "https://api.deepseek.com/anthropic"
model = "deepseek-flash"
context_window = 1000000
thinking_enabled = true
reasoning_effort = "high"
reasoning_effort_levels = ["low", "high", "max"]
```

模型名称、上下文额度需以账号实际支持为准。也可使用 provider account 的 `deepseek-messages` preset。base_url 可带末尾 `/v1`。

该适配复用 Messages 流式工具调用、超时与重试；工具仍走原有执行和审批。思考签名仅回放到同一协议、同一模型。关闭 thinking 时不发送 effort。开启时支持 low/high/max，未设置时 high；不支持的强制工具选择会明确报错，不会切换协议或绕过限制。

本阶段没有接入 system in-history、增量工具声明、Files API，因此不能据此承诺缓存命中或性能提升。协议行为参考本地 deepseek-harness；离线测试不能替代实际账号/API 联调。
