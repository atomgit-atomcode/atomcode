# Reasoning Effort

`reasoning_effort` 控制 DeepSeek V4 系列模型的思维链深度。该参数直接传递给 DeepSeek API，影响模型在给出最终答案前的内部推理强度。

## 适用模型

仅对以下模型生效：

- DeepSeek 官方 API（`api.deepseek.com`）
- 模型名包含 `deepseek-v4` 的第三方代理
- 模型名包含 `deepseek-reasoner` 的第三方代理

其他模型（OpenAI、Claude、Kimi、GLM 等）上操作此参数不会生效，AtomCode 会给出 5 秒自动消失的提示。

## 有效值

| 值 | 含义 |
|----|------|
| `"high"` | 标准思维链深度（普通请求的默认值） |
| `"max"` | 最大思维链深度（Agent 类工具如 AtomCode 自动使用） |

> 注意：DeepSeek API 文档中 `"low"` 和 `"medium"` 会映射为 `"high"`，`"xhigh"` 映射为 `"max"`。因此实际有效值只有 `high` 和 `max` 两档。

## 配置方式

### config.toml

```toml
[providers.deepseek]
type = "openai"
api_key = "sk-..."
model = "deepseek-v4-pro"
base_url = "https://api.deepseek.com/v1"
context_window = 128000
reasoning_effort = "high"      # "high" | "max"
```

未设置（省略该字段）时，AtomCode 不发送 `reasoning_effort` 参数，由 API 自行选择默认值（Agent 类请求默认为 `max`）。

### 快捷键

**`Ctrl+T`** — 循环切换 reasoning_effort 值：

```
None (API 默认) → high → max → None
```

在 Idle、Streaming、Approval 三个阶段均可使用。切换后自动持久化到 `config.toml`。

### 斜杠命令

```
/effort              # 显示当前 reasoning_effort 值
/effort high         # 设置为 high
/effort max          # 设置为 max
/effort off          # 关闭（不发送该字段，API 使用默认值）
```

## 界面显示

设置 reasoning_effort 后，状态栏会显示当前值：

```
deepseek-v4-pro :max    ~/project    12.3k/128k tok
```

切换到非 DeepSeek 模型后，effort 后缀自动消失。切回 DeepSeek 模型后自动恢复为上次设置的值。

## 智能检测

AtomCode 仅在以下两种情况向 API 发送 `reasoning_effort` 字段：

1. **API 地址检测**：`base_url` 包含 `api.deepseek.com`
2. **模型名检测**：model 字段包含 `deepseek-v4` 或 `deepseek-reasoner`

同时满足"检测通过"和"用户已设置值"两个条件时，才会在请求体中包含该字段。向非 DeepSeek 网关发送该字段可能导致 400 错误，智能检测可避免此问题。

## 技术细节

- 每个 provider 的 `reasoning_effort` 独立持久化在 `config.toml` 中
- 切换 provider 时自动从配置文件读取/清除对应的值
- Ctrl+T 在不适用的模型上不改变 effort 值，仅提示无效果
- 非适用模型上的提示自动 5 秒消失（状态栏 transient hint 机制）
- 支持中英文 i18n 翻译
