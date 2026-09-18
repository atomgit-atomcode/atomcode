# OpenRouter 免费模型一键接入 — Design

**Date:** 2026-08-28
**Status:** Approved design, pre-implementation
**Branch:** release/v5.1.0

## 目标

在用户"没有可用模型"的两个时刻,提供一条低摩擦漏斗把用户接入 OpenRouter 的免费模型:

1. **额度用尽** —— CodingPlan 的 5h/30d 窗口配额耗尽,或 claim 级联到 Lite 也拿不到。
2. **新用户未领到 CodingPlan** —— onboarding 结束时用户没有 CodingPlan 权益。

业务目标是提升 OpenRouter 使用量;技术抓手是"零成本、一键、立即可用"的接入体验。atomcode 已是 OpenRouter 归因合作方(见"复用"一节),流量自带 `X-OpenRouter-Title: AtomCode` 标记。

非目标(本期不做):webui / headless / ACP 接入入口;免费模型 429 时的自动轮换;付费模型接入。

## 复用现状(已探明)

| 部件 | 位置 | 复用方式 |
| --- | --- | --- |
| OpenRouter provider preset | `atomcode-config` `provider_preset.rs`(`id:"openrouter"`, OpenAI 兼容, base_url 就绪) | 直接复用,无需新 provider 类型 |
| 归因头 | `atomcode-capabilities` `provider/openai_compat.rs`(仅对 openrouter.ai 发 `X-OpenRouter-*` / `HTTP-Referer`) | 已就绪 |
| 运行时加账号/模型 | `ProviderAccountConfig` + `ModelProfileConfig`(`atomcode-config` `provider.rs`), `reload_provider()`, `Config::save()` | 装配直接复用 |
| OAuth TUI 骨架 | `atomcode-tuix` `event_loop/oauth_poll.rs`(后台线程→事件→存盘,ESC 可取消) | 复制形状,承载 OpenRouter 的本地回调 |
| 额度用尽信号 | `atomcode-codingplan` `RateLimitWindow.quota_exhausted` / `RateLimited` 事件 / `event_loop/usage_monitor.rs` | 触发 nudge 的现成信号 |
| 命令注册 | `atomcode-tuix` `commands.rs` `BUILTIN_COMMANDS[]` | 新增 `/openrouter` 一条 |

**不复用**:atomgit 自家 OAuth 是 state 轮询式,与 OpenRouter 的 localhost-callback PKCE 是不同流程,只借 TUI 集成骨架,不共用协议代码。

## 取得 API key 的两条入口(下游汇合)

拿到 key 之后的下游逻辑只写一份;取 key 有两条并列入口。

### A. `/openrouter`(无参)—— OAuth PKCE 自动获取

OpenRouter OAuth PKCE 事实(已核实 https://openrouter.ai/docs/use-cases/oauth-pkce):

1. 生成 `code_verifier`(随机)与 `code_challenge = base64(sha256(code_verifier))`(S256)。
2. 绑定本地任意空闲端口,打开浏览器:
   `https://openrouter.ai/auth?callback_url=http://localhost:<port>/callback&code_challenge=<challenge>&code_challenge_method=S256`
3. 本地极小 HTTP listener(后台线程)收 `GET /callback?code=<code>`。**ESC 随时取消**:卡死请求最多泄漏一个线程,UI 不阻塞(复用 atomgit 登录 ESC 卡死修复的教训)。
4. 换 key:`POST https://openrouter.ai/api/v1/auth/keys`,body `{code, code_verifier, code_challenge_method}` → `{ "key": "<用户级 API key>" }`。无需 `client_id`,回调式。

### B. `/openrouter <key>`(带参)—— 用户直接传

用户把已有的 OpenRouter API key(如从 openrouter.ai/keys 拿的 `sk-or-v1-...`)贴在命令后,**跳过整个 OAuth**,直接进入下游。

### 兜底策略(YAGNI)

不做 OpenRouter 的 headless "粘贴一次性 code" 模式。无浏览器 / OAuth 失败时,一律回落到 `/openrouter <key>`(更简单、结果持久)。OAuth 失败的提示语补一句"或 `/openrouter <你的key>` 直接接入"。

## 下游:发现 + 装配(两条入口共用)

拿到 key 后:

1. **发现免费模型**:带 key `GET https://openrouter.ai/api/v1/models`。
   - free 判定:模型 id 带 `:free` 后缀,或 `pricing.prompt` 与 `pricing.completion` 均为 0。
   - 排序:按 `context_length` 降序。
   - 取前 **5** 个。
2. **装配**:
   - 建 1 个**持久** `ProviderAccountConfig { provider: "openrouter", api_key: <key> }`。
   - 每个 top 模型建一条 `ModelProfileConfig`(account 引用该账号,model = 该 free 模型 wire 名)。
   - **幂等**:已存在 openrouter 账号则原地更新 key、刷新模型列表,不重复添加账号。
   - `reload_provider()` 激活第一个模型,`Config::save()` 落盘。
3. **确认**:`已接入 OpenRouter,添加 5 个免费模型,已切到 <model>。/model 可切换。`

## 触发(nudge)

两个入口条件收敛到 TUI 一条**可忽略**提示,不自动切换,需用户确认:

- 额度用尽:`quota_exhausted` / claim 级联到 Lite 失败 / `RateLimited` 事件。
- 新用户未领到 CodingPlan:onboarding 结束且无 CodingPlan 权益。

文案示意:`额度已用尽(或:尚未领取 CodingPlan)—— 一键接入 OpenRouter 免费模型?[Enter 接入 / Esc 忽略]`。

**去重**:每会话只弹一次;用户忽略后本会话不再弹(呼应对 nudge/todo 不打扰的既有偏好)。nudge 的"接入"动作默认触发 A(无参 OAuth);OAuth 失败时提示回落到 B。

## 组件划分(各自独立、可单测)

- **`openrouter_oauth`**(置于 `atomcode-auth`):PKCE 生成 / 起手 URL 构造 / 本地回调 listener / code→key 交换。纯逻辑(challenge 计算、URL 编码、`{key}` 响应解析)无需联网可测。
- **OpenRouter 发现客户端**:`list_free_models_by_context(key) -> Vec<ModelEntry>`,fixture JSON 可测。
- **装配 helper**:key + models → 账号 + profiles;已有账号幂等更新;调用 reload + save。对 config 结构体可测。
- **TUI 接线**:nudge 状态;`/openrouter [key]` 命令(注册进 `BUILTIN_COMMANDS`,`acp:false`);后台接入任务(复用 oauth_poll 模式:线程→事件→apply)。

## 数据流

```
nudge / /openrouter[ key]
        │
        ├─(无参)→ OAuth：browser + 本地 listener → code → 换 key ─┐
        └─(带参)→ 直接用 <key> ───────────────────────────────────┤
                                                                     ▼
                                          GET /models（free 过滤 + context 排序 + top5）
                                                                     ▼
                                     建账号 + 5 模型（幂等）→ reload_provider + Config::save
                                                                     ▼
                                                             UI 确认 + 切到首个免费模型
```

全部网络在后台线程;UI 不卡;任何等待处 ESC 可取消。

## 错误处理

- 打不开浏览器 / headless → 提示 `/openrouter <key>`。
- 换 key 网络卡死 → 可取消 + 中文友好错误(复用 `friendly_http_error`)。
- `/models` 返回 0 个免费模型(OpenRouter 侧变动)→ 回退到一个内置已知 free id,或提示 `/model` 自选。
- 免费不足 5 个 → 有几个装几个。
- **key 安全**:这是能花用户 OpenRouter 账户钱的真实 key。**只添加免费模型、默认切到免费模型,绝不静默启用付费模型**。key 持久化与现有 provider `api_key` 同等对待。

## 持久化与范围

- **持久**保存(契合"提升使用量",重启免重连)。
- **v1 = 交互式 TUI**;webui / headless / ACP 之后再评估。

## 测试

- **PKCE**:`code_verifier` → S256 `code_challenge` 已知向量;auth URL 参数编码;localhost 端口占位。
- **发现**:fixture `/models` JSON → free 过滤 + context 降序 + top5;边界(并列 context、免费 < 5、pricing 字段缺失/非零)。
- **装配**:账号 + profiles 结构正确;已有账号幂等;reload / save 被调用。
- **nudge**:触发谓词(`quota_exhausted` / onboarding 无 CodingPlan)→ 只弹一次;忽略后抑制。
- **取消**:等待中 ESC 不阻塞 UI(镜像 atomgit ESC 测试)。
- **命令解析**:`/openrouter` vs `/openrouter <key>` 分派正确;key 参数不落入日志。

## 风险 / 备注

- "提升使用量"是增长目标;诚实抓手就是在用户没有可用模型的两个时刻做低摩擦漏斗。归因头已把流量标记为 AtomCode。
- 免费模型有 OpenRouter 侧自己的限流(如每日请求数);某 free 模型 429 时轮换到 top5 下一个 —— **列为 v2 可选**,v1 不做。
- 真机验证点:浏览器回调、ESC 取消、`Config::save` 后重启仍在、切到免费模型后可正常对话。
