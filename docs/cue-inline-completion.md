# CUE — AI 行内补全

CUE 是编辑器里的"幽灵文本"补全：光标停在代码中间，编辑器用灰字显示"这里该写什么"，
按 `Tab` 接受。形态与 TRAE / Copilot 的行内补全一致。

它由三部分组成，各司其职：

| 部分 | 位置 | 职责 |
|---|---|---|
| 补全内核 | `crates/atomcode-capabilities/src/cue/` | 组装提示词、从模型回答里取出待插入文本。**与模型无关** |
| 补全服务 | `crates/atomcode-daemon/src/api_cue.rs` | 解析模型、调用模型、返回补全文本。**凭据留在这里** |
| 编辑器适配 | `extensions/vscode/src/cue/` | 取光标上下文、显示幽灵文本。**不持有任何 API Key** |

---

## 为什么走 daemon，而不是 IDE 语言服务桥接

仓库里已有一条 IDE 桥接通道（`extensions/vscode/src/languageBridge/`），但它被**刻意设计成只读**：
只接受 `hover` / `definition` / `references` / `diagnostics` / `document_symbols` 这类"向 agent 提问索引"的请求，
不接受任何调用方自定义的 IDE 命令。

补全是反方向的——它要模型**写**。把它放在那条桥接上会做两件不该做的事：

1. 让一条只读通道变成写入通道，而那条通道的连接文件会被 agent 侧发现并使用；
2. 让 IDE 持有它不该持有的东西——补全需要调用模型，而模型的凭据在 daemon。

所以 CUE 走 daemon 的 HTTP 通道（`POST /cue/complete`），和 `/chat` 同一条路：
凭据、模型选择、重试策略都在 daemon 一侧，扩展只负责把代码送过去、把文本显示出来。

---

## 开启

CUE **默认关闭**。每次建议都会消耗模型额度，没主动要求就不该为此付费。

```toml
# ~/.atomcode/config.toml
[cue]
enabled = true

# 补全专用模型。留空则用当前默认模型。
# 补全要的是"快"，agent 要的是"强"——建议配一个更快的小模型。
model = "your-fast-model"

# 单次请求最长等待秒数（1..=120）。
timeout_secs = 10
```

改完执行 `/reload` 生效（也会清空 daemon 侧的模型缓存）。

VS Code 侧还有一个开关：

```jsonc
// settings.json
{
  "atomcode.cue.enabled": true   // 默认 true
}
```

扩展侧的开关管的是"要不要向 daemon 要建议"，daemon 侧的 `[cue] enabled` 管的是"要不要真的调用模型"。
两者都开才有建议。

---

## 补全是怎么生成的

### 1. 编辑器取上下文

编辑器把光标前后的文本送给 daemon：

- **光标前**：保留**尾部**（紧挨着光标的代码才是预测它的东西）；
- **光标后**：保留**头部**（紧挨着光标的代码说明了这个块在哪里结束）。

上限是 8k / 4k 字符。数据文件（json / yaml / markdown / toml …）与空文件不参与补全——
在那里给建议没有意义，却照样花钱。

### 2. daemon 组装提示词

`atomcode_capabilities::cue::build_completion_messages` 把两块代码装进一条提示词，
并明确要求模型：**只回答该插入的文本**，不要 markdown 围栏，不要解释，不要重复光标处已有的内容。

提示词的围栏长度是按代码里最长的反引号串算的——所以文件里本身含有 ``` 的代码块
不会"逃出"围栏，把后面的提示词伪装成正文。

### 3. daemon 调用模型

走的是和 `/chat` 完全相同的 provider 装配路径（`coding_provider_factory().build(...)`），
所以模型选择、签名账号、代理、超时、重试全部一致。

补全复用副请求的采样策略。请求低推理强度，禁用工具调用，并在超时或回答被截断时放弃建议。

### 4. 从回答里取出待插入文本

模型应只返回光标处需要插入的文本。补全会保留回答里的字符和空白，例如光标前是
`const n = +` 时，回答 `+counter;` 会原样插入，得到 `const n = ++counter;`。

模型明确拒绝时（例如 `<<<NO_COMPLETION>>>`）不显示建议。完整的 Markdown 代码围栏会被去掉，围栏内的文本保留。

---

## 模型缓存在 daemon 侧

行内补全在每次停顿都会触发一次。如果每次都重建 provider，就意味着每次停顿都要
重新建 TLS 连接——对签名账号来说还要重新走一次鉴权。

所以 daemon 缓存 provider，键是**凭据与端点的指纹**（选择 id、协议、base_url、model、
api_key、user_agent 的哈希）。改了模型、换了端点或换了 key，指纹就变了，下次请求自动重建。

`/reload` 会直接清空缓存。

指纹里**不含**明文 key——它必须是 key 的哈希，不是 key 本身。

---

## 迟到的建议没有意义

用户一直在打字，模型还在算。等回答回来时，光标早就挪走了。

因此扩展侧对每个请求打一个"号"，回答回来时先核对号：

- 用户又打字了 → 旧请求被取消；
- 同一文档发起了新请求 → 旧请求作废；
- 编辑器取消请求或关闭 CUE → 中止发给 daemon 的 HTTP 请求。

只有号还是当前号的回答才会显示。

daemon 用 `timeout_secs` 限制选择模型和等待回答的总时间。超时和 provider 失败时，编辑器不显示建议。扩展的连接兜底为 125 秒，覆盖 daemon 可配置的 120 秒上限。

---

## 编辑器侧的设置

CUE 用的是 VS Code **自带的**行内补全界面，所以相关的编辑器设置对它同样生效：

| 设置项 | 默认值 | 说明 |
|---|---|---|
| `editor.inlineSuggest.enabled` | `true` | 行内补全总开关 |
| `editor.inlineSuggest.delay` | `100` | 停止输入后多久请求补全（毫秒） |

**接受建议**用 VS Code 默认的 `Tab`（有建议显示时 `Tab` 接受建议，否则照常缩进）。
这意味着 CUE 不需要注册任何按键，也不会抢占 `Tab`。

防抖也由 `editor.inlineSuggest.delay` 控制，扩展**不再加一层计时器**——
多一层只会增加延迟，且让"什么时候请求"这件事有两个答案。

CUE 自己的开关是 `atomcode.cue.enabled`（扩展侧）与 `[cue] enabled`（daemon 侧），
两者都开才有建议。

---

## 接口

### `POST /cue/complete`

请求：

```json
{
  "file_path": "src/main.rs",
  "language_id": "rust",
  "prefix": "fn main() {\n    let x = ",
  "suffix": ";\n}"
}
```

响应：

```json
{
  "completion": "println!();",
  "model": "your-fast-model"
}
```

`completion` 为 `null` 表示没有建议（模型拒绝、请求失败、超时，或 CUE 未启用）。
这**不是**错误状态——没有建议就是没有建议。

请求体上限 64 KiB（前后缀合计），超过直接 413。

---

## 测试

```bash
# 补全内核（提示词组装 + 回答提取）
cargo nextest run -p atomcode-capabilities cue

# daemon 侧：模型缓存指纹 + 端到端（mock provider → 路由 → 补全文本）
cargo nextest run -p atomcode-daemon cue

# 编辑器侧：上下文规则
cd extensions/vscode && npm run test:webview
```

端到端测试用 mock SSE provider 跑完整链路：路由 → 配置边界 → provider 工厂 →
提示词 → 回答提取，不依赖网络和真实凭据。

---

## 已知边界

- **补全模型不区分"该不该补"**：CUE 问模型"光标处该写什么"，模型自己决定回不回答。
  它不知道你正在写注释还是写代码。
- **不跨文件**：上下文只有当前文件的前后文。没有打开的文件、没有选中的符号。
- **不读 LSP 索引**：与 IDE 语言服务桥接是两回事，那条通道是 agent 读 IDE 索引用的。
- **补全模型与 agent 模型可以不同**：这是有意的——`[cue] model` 单独配。
