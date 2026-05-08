# 内置 VL 预处理器设计

- **状态**: Draft
- **日期**: 2026-05-08
- **作者**: brainstorm with theo

## 背景

AtomCode 已支持 `MessageContent::MultiPart { text, images }`：用户在 TUIX 通过 Ctrl+V 粘图，图像 base64 暂存进 `state.pending_images`，提交时随这一条用户消息一起进会话。

发送侧在 `OpenAiProvider::format_messages` 按 `supports_vision`（`ProviderConfig::accepts_images()` 派生）路由：

- 视觉模型（Claude、GPT-4o、Qwen3-VL 等）→ 走 OpenAI vision schema，`image_url` 数据 URI 块。
- 文本模型（DeepSeek-V4、Qwen3.6 Coder、Kimi-K2-Instruct 等）→ 降级成 `"[image attached]"` 占位符，**图像数据直接丢失**。

CodingPlan 用户的常见组合是「主模型 = DeepSeek-V4-flash 或 Qwen3.6 Coder」，这两类模型都不支持视觉，导致用户每次粘图都得手动切到 vision 模型再切回来。期望：在主模型不支持视觉时，自动用一个 VL 模型先把图片转成文字描述，再让主模型继续工作；而 VL 调用本身不接收主对话上下文，避免长会话被无关地灌进 VL。

## 目标与非目标

**目标**

1. 当主 provider `!accepts_images()` 且用户提交了图片时，自动调用配置的 VL provider 把图片转成文本描述。
2. VL 调用只看「当前轮 caption + 图片」，**不带主对话历史**。
3. VL 输出拼接到当前用户消息文本后，原图丢弃，主对话走纯文本路径。
4. VL 调用失败 / 超时 → 降级为占位符文本，本轮照常进行。
5. 功能默认关闭；启用方式 = 在 `Config` 顶层填一个 `vision_preprocessor_provider` 字段指向某个 provider key。

**非目标**

1. 不改 provider trait、不改 `accepts_images()`、不改 `coding_plan/setup.rs`、不改 `Conversation` 结构 —— 纯加性。
2. 不做 VL 结果缓存（同图重复识别）—— 命中率低、复杂度高。
3. 不提供 `/vl on|off` slash 命令 —— 配置项已经够用。
4. 不解决「`/codingplan` 刷新时清掉用户手加的 `AtomGit-Qwen-...` provider」的问题 —— 现有 `is_codingplan_provider_name` 行为保留；测试期手动重加。后续作为 follow-up（需要给 coding-plan setup 加"用户保留标记"机制）。
5. 不支持把图片转 OCR 后还保留原图给 vision-capable 主模型走双通路 —— 只在主模型 `!accepts_images()` 时触发，触发即替代。

## 总体架构

新增一个独立模块；其余文件最小改动：

```
crates/atomcode-core/src/
  vision_preprocessor.rs        # 新增：VL 预处理入口 + 内部一次性调用
  agent/mod.rs                  # 改：handle_send_message 里调一次预处理
  config/mod.rs                 # 改：Config 加 vision_preprocessor_provider 字段
  lib.rs                        # 改：re-export vision_preprocessor
```

`vision_preprocessor` 是唯一新增模块，其它文件都是单点小修。模块对外只暴露一个 async 函数 `maybe_preprocess` 和一个枚举 `PreprocessOutcome`。

## 公开接口

```rust
// crates/atomcode-core/src/vision_preprocessor.rs

pub enum PreprocessOutcome {
    /// 不需要做预处理：未配置、主 provider 已支持视觉、images 为空 —— 上层走原路径。
    Skipped,
    /// VL 调用成功，`text` 是 VL 原始输出（未做包裹）。上层负责把 `text` 接到 caption 后、
    /// 清空 images、决定包裹文案（如 `"\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}"`）。
    Replaced { text: String },
    /// VL 调用失败（找不到 provider、网络错误、超时、空响应）。reason 用于 Notice 日志；
    /// 上层应清空 images，按降级文案 `[图片识别失败：{reason}]` 拼接。
    Failed { reason: String },
}

pub async fn maybe_preprocess(
    config: &Config,
    active_provider: &dyn Provider,
    caption: &str,
    images: &[ImagePart],
) -> PreprocessOutcome;
```

判断顺序（每条都是短路 → Skipped）：
1. `images.is_empty()`
2. `active_provider.accepts_images()`
3. `config.vision_preprocessor_provider` 为 `None` 或 `Some("")`
4. `config.providers[vl_key]` 不存在 → **Failed**（不是 Skipped，因为这是配置错；用户配了 key 但 key 拼错时该看到错误）

通过以上四关后构造 VL provider 并发起调用。

## 数据流

```
TUIX (Ctrl+V → state.pending_images)
   │ 提交
   ▼
Agent::handle_send_message(content, images)
   │ 在「if images.is_empty()」前
   ▼
vision_preprocessor::maybe_preprocess(config, &*provider, &clean, &images)
   │
   ├── Skipped
   │     → (clean, images) 不变 → 走原 MultiPart 路径
   │
   ├── Replaced { text }
   │     → clean = format!("{clean}\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}")
   │       images = vec![]
   │       → 走纯文本路径 add_user_message(&clean)
   │
   └── Failed { reason }
         → event_tx.send(AgentEvent::Notice(format!("VL 预处理失败：{reason}")))
           clean = format!("{clean}\n\n[图片识别失败]")
           images = vec![]
           → 走纯文本路径
```

## VL 调用细节

`maybe_preprocess` 内部：

1. 从 `config.providers[vl_key]` clone 出 `ProviderConfig`，用 `OpenAiProvider::new(&cfg)` 构造一次性 provider 实例（VL 模型走 OpenAI 兼容协议，复用现有 provider 实现）。
2. 构造一条全新的、与主对话**无关**的 `Vec<Message>`：

   ```rust
   let prompt = if caption.trim().is_empty() {
       "请详细描述这张图片的内容。如果是代码、报错截图或终端输出，请逐字转录文本。".to_string()
   } else {
       format!(
           "用户的当前请求：{caption}\n\n请详细描述这张图片的内容。如果是代码、报错截图或终端输出，请逐字转录文本。"
       )
   };
   vec![Message {
       role: Role::User,
       content: MessageContent::MultiPart {
           text: Some(prompt),
           images: images.to_vec(),
       },
   }]
   ```

3. 调用 `provider.send(messages, &[])` —— 不带 tools，让 VL 纯做识别。
4. 把流式 `StreamEvent::Text` 聚合成 `String`；忽略 `StreamEvent::Reasoning`（VL 不太可能有，万一有也对识别结果无价值）。
5. 整个调用包一层 `tokio::time::timeout(Duration::from_secs(30), ...)`：30s 上限兜底。
6. 任何 `anyhow::Error` 或超时 → 转 `Failed { reason: format!("...") }`。
7. 聚合后的文本 trim；空字符串 → `Failed { reason: "VL 返回空响应".into() }`。

**关键安全保证**：传给 VL 的 `Vec<Message>` 是函数内现场构造的局部变量，跟 `agent.conversation.messages` 完全无引用关系。无论主对话有多少历史，VL 永远只看到 `[caption_prompt + images]` 这一条。

## Config 改动

`crates/atomcode-core/src/config/mod.rs` 顶层 `Config` 新增字段：

```rust
/// Provider key (matches `Config.providers` HashMap key) of a vision-language
/// model used to preprocess images before forwarding to a non-vision main
/// provider. When `None` or empty, image preprocessing is disabled — pasted
/// images either go directly to a vision-capable main provider, or get
/// degraded to `"[image attached]"` placeholder by the existing path.
///
/// Example value: `"AtomGit-Qwen-Qwen3-VL-32B-Instruct"`.
#[serde(default)]
pub vision_preprocessor_provider: Option<String>,
```

`#[serde(default)]` 保证存量 `config.toml` 不带这个字段时不会反序列化失败。

`Config::default()` 中也设为 `None`。

## Agent 集成

`agent/mod.rs::handle_send_message`，在现有 `if images.is_empty()` 分支判断之前（约 line 1266 附近）插一段：

```rust
let (clean, images) = if !images.is_empty() {
    use crate::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
    match maybe_preprocess(
        &self.config,
        self.turn_runner.provider.as_ref(),
        &clean,
        &images,
    ).await {
        PreprocessOutcome::Skipped => (clean, images),
        PreprocessOutcome::Replaced { text } => (
            format!("{clean}\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}"),
            vec![],
        ),
        PreprocessOutcome::Failed { reason } => {
            let _ = self.event_tx.send(AgentEvent::Notice(
                format!("VL 预处理失败：{reason}"),
            ));
            (format!("{clean}\n\n[图片识别失败]"), vec![])
        }
    }
} else {
    (clean, images)
};
```

注：实现期需先确认 `self.config` 与 `self.turn_runner.provider` 在 `handle_send_message` 上下文中的精确访问形式（Arc / RwLock / 直接借用），如有差异调整 `maybe_preprocess` 签名（如 `&Arc<Config>`）。其余流程不变。

## UX

VL 调用通常 1–3s，主模型在等待期间无任何输出，用户体验类似"卡死"。在 `maybe_preprocess` 入口（即将真正发起 HTTP 之前）发一条 `AgentEvent::Notice("正在用 Qwen3-VL 识别图片…")`，让 TUIX 显示一条临时状态行。VL 完成后用户消息正常落入 scrollback 时，临时状态行被覆盖。

**待实现期确认**：现有 `AgentEvent` 枚举是否已有合适的 Notice / Status 变体；若没有，在实现 PR 中新增 `AgentEvent::VisionPreprocessing { stage: VisionStage }`，`stage = Started | Completed | Failed`，TUIX 端做对应渲染。这部分细节不阻塞 design 评审，留给写 plan 时确定。

## 失败模式与降级文案

| 触发 | 用户消息附加文案 | event 类型 |
|---|---|---|
| 主 provider 已支持视觉 | （无，走原路径） | — |
| `vision_preprocessor_provider` 未配置 | （无，走 `[image attached]` 老路径） | — |
| 配置的 provider key 在 `config.providers` 中找不到 | `[图片识别失败]` | `Notice("VL 预处理失败：provider 'X' not found")` |
| HTTP 错误（4xx / 5xx / 网络） | `[图片识别失败]` | `Notice("VL 预处理失败：{anyhow chain}")` |
| 30s 超时 | `[图片识别失败]` | `Notice("VL 预处理失败：timeout after 30s")` |
| VL 返回空字符串 | `[图片识别失败]` | `Notice("VL 预处理失败：empty response")` |

降级文案 `[图片识别失败]` 让主模型至少知道用户附了图但识别没成功，可以追问而不是装作没看见。

## 测试

**单元测试**（`crates/atomcode-core/src/vision_preprocessor.rs` 内 `#[cfg(test)] mod tests`）：

用 wiremock（项目其它 provider 测试已在用）启假 OpenAI 后端，覆盖以下用例：

1. `images.is_empty()` → Skipped（不应发起任何 HTTP）。
2. 主 provider `accepts_images() == true` → Skipped（同上不发请求）。
3. `vision_preprocessor_provider = None` → Skipped。
4. 配置了 key 但 `config.providers` 中不存在该 key → Failed（reason 含 "not found"）。
5. 假后端返回正常 SSE `[DONE]` 流，文本内容 `"image describes a Python stack trace"` → Replaced，text 等于聚合后的字符串。
6. 假后端返回 HTTP 500 → Failed，reason 含状态码。
7. 假后端 sleep 35s → Failed，reason 含 "timeout"（用 `tokio::time::pause` 控时）。
8. 假后端返回空 content（只有 `[DONE]`）→ Failed，reason 含 "empty"。
9. caption 非空时，验证发出去的 request body 中 prompt 字符串包含 caption（断言 wiremock 收到的 body）。
10. caption 为空时，prompt 走纯描述模板（不含"用户的当前请求："前缀）。

**集成手测**（实现 PR 描述的 Test plan 部分需列出）：

1. `cargo run -p atomcode-cli --release` 进 TUI。
2. `/codingplan` 安装 AtomGit provider 列表；手动加 `AtomGit-Qwen-Qwen3-VL-32B-Instruct` 到 `~/.atomcode/config.toml`。
3. 在 `[default]` 段加 `vision_preprocessor_provider = "AtomGit-Qwen-Qwen3-VL-32B-Instruct"`。
4. `/model AtomGit-DeepSeek-V4-flash`（或其它 `!accepts_images()` 的 entry）。
5. Ctrl+V 粘一张代码截图，附 caption "解释这段代码"，回车。
6. 期望：
   - scrollback 出现 "正在用 Qwen3-VL 识别图片…" 状态行。
   - 用户消息以 `解释这段代码\n\n[图片内容（由 Qwen3-VL 识别）]\n...` 形式落入对话。
   - DeepSeek 收到的请求体（用 `/datalog tail` 验证）只有纯文本，无 `image_url` 块。
   - 主模型回答合理。
7. 关掉 `vision_preprocessor_provider`，重复步骤 5。
   - 期望：DeepSeek 收到 `"[image attached]"` 占位符，识别能力丧失（验证 fallback path 未被破坏）。
8. 设个故意拼错的 key（`vision_preprocessor_provider = "AtomGit-NoSuchModel"`），重复步骤 5。
   - 期望：scrollback 出现 "VL 预处理失败：provider 'AtomGit-NoSuchModel' not found"，用户消息以 `[图片识别失败]` 结尾，本轮主模型仍正常应答。

## 风险与权衡

1. **临时构造 OpenAiProvider 的成本**：每次调用都新建 reqwest client + 解析 ProviderConfig。在 1–3s 的 VL 调用本身耗时面前完全可忽略；不预先缓存 provider 实例换来"配置改了立即生效"，简单。
2. **30s 超时**：图片描述应该几秒就够，30s 是兜底防止卡死。如果实测发现某些大图（4K 截图）确实需要 10s+ 才能出第一个 token，再调整。
3. **Notice event 类型未敲定**：如设计 §UX 所述，留给实现期决定是否新增 `AgentEvent::VisionPreprocessing` 还是复用现有 Notice。这影响 TUIX 渲染细节，不影响核心逻辑。
4. **`/codingplan` 刷新会清掉用户手加的 AtomGit-Qwen-VL provider**：明确不在本 design 范围解决。测试期重跑 `/codingplan` 后需要手动重加；用户也可以把 VL provider 命名为非 `AtomGit-` 前缀（如 `vl-qwen3`）规避清洗逻辑——这是临时 workaround。后续 follow-up issue 再设计「用户保留 provider 标记」机制。
5. **VL 输出可能很长**：4K 截图详细识别可能产出 1–2K token 的描述。这部分计入主对话历史，会侵占主模型的 ctx_budget。已知风险，但是用户主动选择 VL 路径的代价；如果未来发现普遍超长，可以加截断（如 2000 字符封顶）。当前不做。
