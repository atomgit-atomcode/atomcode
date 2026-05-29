# `/provider` Add 流程简化设计

日期：2026-05-29
分支：feat/webui-design

## 背景与问题

`/provider` 的「add」子流程目前是 5 步串行问答（见
`crates/atomcode-tuix/src/modals/provider_wizard.rs`）：

1. Name —— 必填，用户自取，作为 providers map 的 key
2. Type —— 必填，手敲 `openai` / `claude` / `ollama`
3. Base URL —— 可空，留空用该 type 的内置默认
4. API Key —— 可空
5. Model —— 必填

用户反馈「填写内容太复杂」，核心痛点确认为两点：**步骤太多/太啰嗦** +
**希望支持一键导入（粘贴 base_url + key）**。model 仍由用户手动输入（不引入网络请求）。

## 目标

把常见场景（粘贴第三方 OpenAI 兼容端点的 base_url + key）的实际输入项从 5 项减到
**3 项：Base URL、API Key、Model**。Type 自动推断、Name 自动派生，用户只需回车确认。

## 非目标

- 不拉取远端模型列表（model 保持手动输入，无网络依赖）
- 不改 `ProviderConfig` schema 或 `config.toml` 格式
- 不动 Edit / Delete / Set-Default 子流程
- 不做厂商预设列表（本次只做推断 + 派生）

## 设计

### 新的 Add 步骤顺序

Base URL 提到第一步，由它驱动后续自动推断：

| 步骤 | 字段 | 说明 |
|---|---|---|
| 1 | **Base URL** | 粘贴，可空 |
| 2 | **Type** | 仅当 Base URL 留空时询问；否则自动推断并跳过 |
| 3 | **API Key** | 粘贴，可空 |
| 4 | **Model** | 必填，非空校验 |
| 5 | **Name** | 预填派生值，空回车接受，可改 |

常见场景（Base URL 非空）下，第 2 步被跳过，实际只需输入 Base URL、API Key、
Model 三项，Name 回车确认。

### Type 自动推断

对 base_url 小写后按顺序匹配：

- 含 `anthropic` → `claude`
- 含 `11434` 或 `ollama` → `ollama`
- 其余一律 → `openai`（绝大多数第三方端点是 OpenAI 兼容）

推断完成后向 scrollback **回显一行**（如「类型：openai（根据 Base URL 推断）」），
让用户看到结果、保持透明。

仅当 Base URL 留空（想用官方默认 URL）时，才回退到原 Type 询问步骤；该步骤校验
逻辑沿用现有 `["openai","claude","ollama"]` 白名单。

### Name 自动派生

从 base_url 的 host 提取主标签：

1. 剥协议（`http://` / `https://`）、去掉路径与端口
2. 去掉前缀 `api.` / `www.`
3. 取剩余 host 的第一个有意义标签（顶级域 `.com`/`.cn`/`.ai` 等丢弃）
4. 小写化并清洗为合法 key

示例：

- `https://api.deepseek.com/v1` → `deepseek`
- `https://api.moonshot.cn/v1` → `moonshot`
- `https://openrouter.ai/api/v1` → `openrouter`
- `http://localhost:11434` / 空 host → 用 type 名（如 `ollama`）

**冲突处理**：若派生名已存在于 `ctx.config.providers`，依次追加 `-2`、`-3`… 直到唯一。

**可改**：进入 Name 步骤前先把派生值写入 `draft.name`，Name 步骤的 prompt 以默认值
形式展示该名字（沿用现有 edit-hint 文案模式）；空回车接受派生值，非空输入覆盖。
覆盖后同样要做冲突检查。

### 控制流（advance_add 改动）

`advance_add` 的分支跳转改为：

- `BaseUrl`：存 base_url。
  - 空 → 返回 `Some(ProviderType)`
  - 非空 → `infer_type(&base_url)` 存入 draft，回显推断行，返回 `Some(ApiKey)`
- `ProviderType`：校验白名单后存入，返回 `Some(ApiKey)`
- `ApiKey`：存入，返回 `Some(Model)`
- `Model`：非空校验后存入；计算 `derive_name(...)` 写入 `draft.name` 作为默认，
  返回 `Some(Name)`
- `Name`：空 → 保留 `draft.name`（已是派生默认）；非空 → 覆盖。两种情况都做冲突
  检查（追加 `-N`）。返回 `None` 触发提交。

Add 入口起始步骤从 `WizardStep::Name` 改为 `WizardStep::BaseUrl`。

提交逻辑（插入 provider、设为 default、切换 model、`save_and_reload`）与成功文案
`Msg::ProviderAdded` 保持不变。

### 两个新 helper（纯函数）

```rust
/// 从 base_url 推断 provider type。
fn infer_type(base_url: &str) -> &'static str;

/// 从 base_url（与回退 type）派生唯一的 provider 名字。
/// `existing` 用于冲突检测。
fn derive_name(base_url: &str, provider_type: &str, existing: &HashMap<String, ProviderConfig>) -> String;
```

### i18n

新增文案，zh_cn + en 同步：

- 推断回显：`ProviderTypeInferred { type }` —— 「类型：{type}（根据 Base URL 推断）」
- Name 默认提示：`ProviderStepNameDefault { default }` —— 「Provider 名称？[{default}]（留空使用此名）」

复用现有：`ProviderStepBaseUrl`、`ProviderStepType`、`ProviderStepApiKey`、
`ProviderStepModel`、`ProviderModelEmpty`、`ProviderUnknownType`、`ProviderAdded`。

## 受影响文件

- `crates/atomcode-tuix/src/modals/provider_wizard.rs`
  - Add 入口起始步骤 `Name` → `BaseUrl`
  - `advance_add` 分支跳转重写
  - 新增 `infer_type`、`derive_name`
  - Name 步骤 prompt 显示派生默认值
- `crates/atomcode-core/src/i18n/messages.rs` —— 新增两个 Msg 变体
- `crates/atomcode-core/src/i18n/zh_cn.rs`、`en.rs` —— 对应文案

**不改动**：`advance_edit`、Edit/Delete/Set-Default 各状态、`DraftProvider::into_config`、
`ProviderConfig`、`config.toml` 格式。

## 测试

为两个纯函数加单元测试：

- `infer_type`：anthropic url → claude；含 11434 / ollama → ollama；任意第三方 url
  → openai；空串 → openai
- `derive_name`：`api.deepseek.com` → `deepseek`；`api.moonshot.cn` → `moonshot`；
  `openrouter.ai` → `openrouter`；`localhost:11434` → `ollama`（回退 type）；
  已存在 `deepseek` 时 → `deepseek-2`

## 兼容性

- 配置文件格式不变，旧 provider 不受影响
- Edit 流程行为完全不变
- 想添加官方 OpenAI/Claude/Ollama（用默认 URL）的用户：Base URL 留空 → 仍会被问
  Type，路径与今天一致，只是少了手敲 Name（改为派生默认 + 回车）

---

## 增补（最终实现）：模板导入并入 Add + 步骤进度（2026-05-29）

最终没有单列「模板导入」菜单项，而是**把导入并进 add**：进来第一步就推荐粘贴模板，
留空则手动填写。菜单保持 4 项（add / edit / delete / set-default）。

### 统一的 Add 流程

新增 `WizardStep::Template` 作为 Add 的第一步，提示：
「粘贴模板自动识别（curl / JSON / TOML 或 Base URL），或直接回车手动填写：」

- **粘贴内容**（`advance_template`）：
  - `parse_template` 抽到 URL → 填 draft：`infer_type` 推断 type、`normalize_base_url`
    归一化 base_url、占位符 key 丢弃、model 带入、name 用解析值或 `derive_name`；回显
    `ProviderImportParsed`「已识别：name · type · model」；进入「补缺」（缺 key/model 才问）。
  - 像模板但抠不出 URL（`looks_like_template`）→ `ProviderImportFailed`，重贴。
  - 非空且不像模板、也无 URL → 当作裸 base_url 处理（推断 type → 问 API Key）。
- **留空** → 手动：问 类型 → API Key → Model → Name（base_url 留空用 provider 默认）。

### 解析（纯函数，逐个 TDD）

- `parse_template(input) -> ParsedTemplate { url, api_key, model, name }`：
  - 含 `curl`/`authorization`/`x-api-key`/`-h `/`-d ` → curl 模式：URL = 第一个
    `http(s)://` token；api_key = `Authorization: Bearer X` 或 `x-api-key: X`；
    model = body 里 `"model"` 值
  - 否则 key-value 模式（JSON/TOML 通用）：`quoted_value_after` 抓
    `base_url`/`api_key`/`model`；`toml_provider_name` 抓 `[providers.NAME]` → name
- `normalize_base_url`：openai 剥结尾端点保留 `/v1`；claude/ollama 只留 authority
- `is_placeholder`：`<...>`、`YOUR_API_KEY`、`sk-xxxx`、`****` → 占位符

### 步骤进度

- `remaining_add_steps(draft)`：按已填情况返回剩余问题序列（Type 若 type 空、ApiKey 若
  key 空、Model 若 model 空、末尾恒含 Name）。
- 离开 Template 步时把 `remaining_add_steps(draft).len()` 冻结为 `Add.total`；
  各 gap 步显示 `(current/total)`，`current = total − 当前 remaining 长度 + 1`。
- Template 步本身不计数（此时还不知道要几步）。
- 新增 i18n `ProviderStepProgress { current, total }`。

### 衔接细节

- `advance_add` 跳转「已填则跳过」：ApiKey 之后若 model 已填则直接到 Name；普通手动
  路径此刻 model 必空，行为不变。
- 转入 Name 统一走 `to_name_step`（name 空则派生），Model→Name 与 ApiKey→Name 共用。
- `Add` 增加 `total: usize` 字段；`BaseUrl` 步在 Add 中不再出现（URL 经 Template 进入），
  仍保留给 Edit。

### i18n

新增 `ProviderImportPrompt`、`ProviderImportParsed { name, type_name, model }`、
`ProviderImportFailed`、`ProviderTypeInferred { type_name }`、
`ProviderStepNameDefault { default }`、`ProviderStepProgress { current, total }`。

### 测试（均纯函数 / 轻量集成）

- `parse_template`：taotoken curl（OpenAI 风）、Anthropic 风 curl、JSON 片段、TOML 块
- `normalize_base_url`：openai 剥端点 / 保留、claude 去路径、ollama 留 host:port
- `is_placeholder`：占位符 → true；真实 key → false
- `remaining_add_steps`：手动 4 步、裸 URL 3 步、全填只剩 Name、跳过已填 model
- `advance_template`：留空走手动、curl 填充并问 key、裸 URL 当 base_url

### 兼容性

并入 add 单一入口；`advance_add` 的 skip 对手动 add 行为零变化；Edit 完全不变；
不引入网络、不改 schema 与配置格式。
