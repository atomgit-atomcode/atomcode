# Zouwu — atomcode `/login-with-sso` 接入文档

**受众：** zouwu 前端 + 后端团队
**版本：** 2026-04-14
**对应 atomcode 分支：** `feat/login-with-sso`（合并到 main 后即生效）

---

## 背景

atomcode 是终端 AI 编程助手，GitCode 内部员工通过 `/login-with-sso` 命令登录后，可以用公司内部的 LiteLLM 额度。整个流程遵循 **OAuth 2.0 Authorization Code Flow**，zouwu 作为 "broker" 承担两个角色：

1. **扫码登录前端**——拿到员工身份
2. **code ↔ LLM token 交换后端**——给每个员工签发 LiteLLM virtual key

**Atomcode 客户端已实现完毕**，release 构建只需 zouwu 侧实现下面两个端点即可跑通。

---

## 全流程

```
┌─────────────┐                ┌─────────────┐              ┌──────────┐
│  atomcode   │                │   zouwu     │              │  WeCom   │
│  (terminal) │                │   broker    │              │          │
└──────┬──────┘                └──────┬──────┘              └─────┬────┘
       │                              │                           │
       │ 1. 用户输入 /login-with-sso     │                           │
       │    atomcode 生成 state       │                           │
       │    开浏览器访问 authorize URL                            │
       │───────────────────────────▶  │                           │
       │                              │                           │
       │                              │ 2. 驱动企微扫码           │
       │                              │────────────────────────▶  │
       │                              │                           │
       │                              │ 用户扫码、确认            │
       │                              │ ◀────────────────────────│
       │                              │                           │
       │                              │ 3. zouwu 调企微 API       │
       │                              │    gettoken → userid      │
       │                              │    user/get → 用户信息    │
       │                              │                           │
       │                              │ 4. 生成一次性 code (60s) │
       │                              │    存在 zouwu DB/Redis：  │
       │                              │    code → {userid, ...}   │
       │                              │                           │
       │ ◀─ 302 redirect to           │                           │
       │    http://127.0.0.1:8766/    │                           │
       │    callback?code=...&state=..│                           │
       │                              │                           │
       │ 5. atomcode 本地 server 收到 │                           │
       │    callback，验证 state      │                           │
       │                              │                           │
       │ 6. POST {BROKER_LOGIN_URL}   │                           │
       │    { "code": "..." }         │                           │
       │───────────────────────────▶  │                           │
       │                              │                           │
       │                              │ 7. 验证 code、置无效     │
       │                              │    调 LiteLLM 签 token    │
       │                              │                           │
       │ ◀─ 200 OK                    │                           │
       │    JSON body:                │                           │
       │    { user, llm_token, ... }  │                           │
       │                              │                           │
       │ 8. atomcode 写 auth.toml     │                           │
       │    注册 provider、切换 agent │                           │
       │    用 llm_token 打 LiteLLM   │                           │
       │    做后续 LLM 请求           │                           │
```

---

## Endpoint 1: Authorize Page

### URL

```
GET https://test-zouwu.gitcode.com/login?popup=1&corp=csdn
```

（生产后改成 `https://zouwu.gitcode.com/login?popup=1&corp=csdn`，但路径与 query 保持不变）

### 客户端会在上述 URL 后**追加**两个 query 参数：

| 参数 | 示例值 | 语义 |
|---|---|---|
| `state` | `atomcode_1760432156789` | CSRF 令牌，客户端生成，zouwu **必须原样回传** |
| `parent_origin` | `http%3A%2F%2F127.0.0.1%3A8766%2Fcallback` | URL-encoded 的客户端本地回调地址，zouwu **必须解码后用作 redirect 目标** |

**完整示例**（atomcode 真正打开的 URL）：

```
https://test-zouwu.gitcode.com/login?popup=1&corp=csdn&state=atomcode_1760432156789&parent_origin=http%3A%2F%2F127.0.0.1%3A8766%2Fcallback
```

### zouwu 前端要做的事

1. 读取 `state` 和 `parent_origin`（后者记得 URL-decode）
2. 驱动企微扫码流程（使用 `CORP_ID` + `AGENT_ID`——这两个 zouwu 本身就有）
3. 扫码成功 → 调 zouwu 自家后端生成一次性 `code`（见 [Code 生成规范](#code-生成规范)）
4. 302 重定向浏览器到：

```
{parent_origin}?code=<生成的 code>&state=<原样 echo>
```

**例如**：

```
http://127.0.0.1:8766/callback?code=9f8e7d6c5b4a3210fedcba&state=atomcode_1760432156789
```

### 不允许在 redirect URL 里塞的字段

**⚠️ 绝对不要**把以下字段塞到 callback URL 的 query 里：

- `llm_token`（长期 bearer token，泄漏就是事故）
- `userid` / `name` / `email` / `avatar_url`（身份信息，暴露到浏览器历史、server log 不合规）
- `expires_at`

这些字段**全部通过 Endpoint 2 的 JSON response body 返回**。

### 错误情况

扫码失败、用户不在企业通讯录、超时——任何错误场景，302 重定向时用 `error` 字段：

```
http://127.0.0.1:8766/callback?state=atomcode_xxx&error=<human-readable message>
```

客户端会把 `error` 的值当做 `Login failed: <error>` 显示给用户。推荐错误文案：

- 非企业成员：`WeCom account is not a member of GitCode org`
- 扫码超时：`Scan timed out, please retry`
- 用户拒绝授权：`Authorization cancelled by user`

---

## Endpoint 2: Code Exchange API

### URL

```
POST https://test-zouwu.gitcode.com/exchange
```

（生产后改成 `https://zouwu.gitcode.com/exchange`）

### Request

```http
POST /exchange HTTP/1.1
Host: test-zouwu.gitcode.com
Content-Type: application/json

{"code": "9f8e7d6c5b4a3210fedcba"}
```

- Body 永远是 `{"code": "..."}` 单字段 JSON
- 无 `Authorization` header（code 本身是凭证）
- 无 `state`（已在 callback 阶段验过）

### Response（成功）

HTTP `200 OK` + JSON body：

```json
{
  "user": {
    "userid": "ZhangSan",
    "name": "张三",
    "email": "zhangsan@gitcode.com",
    "avatar_url": "https://cdn.zouwu.com/avatar/zhangsan.png"
  },
  "llm_token": "sk-litellm-abcdef1234567890",
  "expires_at": 1761036956
}
```

### 字段规范

| 字段 | 必填? | 类型 | 说明 |
|---|---|---|---|
| `user.userid` | ✅ 必填 | string | 企业微信 UserId，例如 `ZhangSan` |
| `user.name` | 选填 | string | 员工中文名；没有的话 atomcode 用 `userid` 代替 |
| `user.email` | 选填 | string | 邮箱；`/status` 显示用 |
| `user.avatar_url` | 选填 | string | 头像 URL；当前 UI 未使用，为未来扩展预留 |
| `llm_token` | ⚠️ 强烈建议 | string | LiteLLM virtual key，形如 `sk-litellm-...`。缺省则客户端会记下身份但无法调 LLM |
| `expires_at` | 选填 | integer | Unix 秒时间戳，token 过期时刻。atomcode 用这个显示"30 天后过期"、启动时提示重登录。缺省视为永不过期 |
| `error` | —— | string | 仅在失败时出现；见下 |

### Response（失败）

任何非 2xx 响应都视为失败。推荐：

```http
HTTP/1.1 400 Bad Request
Content-Type: application/json

{"error": "Code already used"}
```

或直接 `Content-Type: text/plain`：

```http
HTTP/1.1 401 Unauthorized
Content-Type: text/plain

Invalid or expired code
```

**客户端行为**：会把 HTTP status + body 的前 500 字符渲染成 `Broker login failed ({status}): {body}`。所以 body 尽量写成人类可读的一句话。

**常见失败场景**：

| HTTP | Body（建议） | 场景 |
|---|---|---|
| 400 | `Invalid code format` | code 字段缺失或格式错误 |
| 401 | `Code expired` | code 过期（超过 60s） |
| 409 | `Code already used` | code 已被使用过（one-shot 被重放） |
| 500 | `LiteLLM unavailable: <upstream err>` | mint token 时 LiteLLM 侧挂了 |

### 无 `error` 但也无 `user` 字段

视为协议违反，客户端会报 `Broker response missing 'user' field`。

---

## Code 生成规范

### 生命周期

- **过期时间**：60 秒（推荐）。过短用户来不及反应，过长增加泄漏风险
- **一次性**：被 `/exchange` 成功换取后**立即失效**；再次提交应返回 409
- **不可预测**：用 CSPRNG 生成，≥128 bit 熵；推荐 base64url 编码 22 字符以上

### 存储

建议用 Redis 或同等 KV 存储：

```
SET code:<code> <json payload> EX 60
```

payload 示例：

```json
{
  "userid": "ZhangSan",
  "user": {"name": "张三", "email": "..."},
  "issued_at": 1760432156
}
```

`/exchange` 处理流程：

```python
# 伪代码
def exchange(code):
    payload = redis.getdel(f"code:{code}")  # 原子取值+删除（one-shot）
    if not payload:
        return 401, "Invalid or expired code"
    
    userid = payload["userid"]
    llm_token = litellm.create_key(
        user_id=f"wecom:{userid}",
        max_budget=100,       # 按公司政策调
        duration="30d",
        models=["gpt-4o", "claude-sonnet-4-5", "qwen-turbo"],
    )
    
    return 200, {
        "user": payload["user"],
        "llm_token": llm_token.key,
        "expires_at": int(llm_token.expires.timestamp()),
    }
```

---

## LiteLLM 集成建议

### Mint virtual key

atomcode 用返回的 `llm_token` 直接打 `INTERNAL_LLM_BASE_URL`（目前配置为 `https://api-atomcode.atomgit.com`）。LiteLLM 代理那边认这个 token 就行。

```python
# zouwu 后端调 LiteLLM
import requests
resp = requests.post(
    "https://litellm.internal.gitcode.com/key/generate",
    headers={"Authorization": f"Bearer {LITELLM_MASTER_KEY}"},
    json={
        "user_id": f"wecom:{userid}",
        "models": [...],
        "max_budget": 100,
        "duration": "30d",
        "metadata": {
            "email": email,
            "wecom_userid": userid,
        },
    },
)
return resp.json()["key"]  # "sk-litellm-xxx"
```

### Token 撤销

员工离职或安全事件时，直接调 LiteLLM `DELETE /key/delete`：

```bash
curl -X POST https://litellm.internal.gitcode.com/key/delete \
  -H "Authorization: Bearer $LITELLM_MASTER_KEY" \
  -d '{"keys": ["sk-litellm-xxx"]}'
```

atomcode 那边**不需要**做任何事——下次该员工打 LLM 请求会收到 401，自己处理即可。

---

## 环境规划

| | Test | Prod |
|---|---|---|
| `BROKER_AUTHORIZE_URL` | `https://test-zouwu.gitcode.com/login?popup=1&corp=csdn` | `https://zouwu.gitcode.com/login?popup=1&corp=csdn` |
| `BROKER_LOGIN_URL` | `https://test-zouwu.gitcode.com/exchange` | `https://zouwu.gitcode.com/exchange` |
| atomcode binary | debug build（跑 zouwu test 环境） | release build，发正式版 |

Atomcode 客户端目前三个常量已填入 test 环境值，release 构建能出二进制。切生产时**只需改 `wecom.rs` 的两个 URL**（去掉 `test-` 前缀）后 rebuild 即可。

---

## 安全 Checklist

- [ ] `CORP_SECRET`、`LITELLM_MASTER_KEY` 只存在 zouwu 服务端环境变量里，绝不下发客户端
- [ ] code 过期 60s、一次性、≥128 bit 熵
- [ ] `/exchange` 响应不回 code（已用过）
- [ ] `/exchange` access log 不记录 response body（避免 llm_token 落盘）
- [ ] HTTPS-only——redirect URL 除外（`http://127.0.0.1` 是允许的 loopback 例外）
- [ ] 企微错误码 → 人类可读消息映射（不要把 `errcode=40056` 原封返回）
- [ ] 非企业成员拒绝：仅返回 `error` 字段，不进入 code 生成阶段

---

## 联调步骤

### 1. 客户端先跑通 mock

atomcode 仓库里 `crates/atomcode-tui/tests/wecom_login_test.rs` 有一个基于 `httpmock` 的集成测试，可以本地验证客户端的 POST `/exchange` 流程：

```bash
cd crates/atomcode-tui
cargo test --test wecom_login_test -- --nocapture
```

这个测试不打真实 zouwu，只验证客户端请求/响应解析是否正确。

### 2. zouwu 搭 staging

建议先做一个"mock zouwu"：

- `/login` 页面：固定返回一个 HTML，加 JS 在 3 秒后 302 到 `{parent_origin}?code=fake-code&state=<echoed>`（跳过企微扫码）
- `/exchange` 接口：看到 `code=fake-code` 就返回固定的 `{user, llm_token}`

客户端用 `ATOMCODE_CONFIG_DIR_OVERRIDE=/tmp/atomcode-test cargo run --bin atomcode` 连 staging 跑一次完整 `/login-with-sso`，验证端到端打通。

### 3. 接入真实企微

Mock 跑通后替换成真企微调用。

### 4. 灰度

挑 3-5 位内部员工做真机测试，验证：

- 扫码后浏览器正确跳回 `127.0.0.1:8766/callback`
- atomcode 正确拿到 `llm_token`
- `/status` 命令显示员工信息 + 过期时间
- 后续对话正确打到 `INTERNAL_LLM_BASE_URL`（`api-atomcode.atomgit.com`）并用 `llm_token` 鉴权

---

## 调试技巧

### 看客户端发出去的 URL

atomcode 在 `/login-with-sso` 启动后会在终端打印完整的 authorize URL：

```
  AtomCode Login (WeCom / GitCode Internal)
  =========================================

  Opening browser for WeCom QR login...
  If browser doesn't open, visit:
    https://test-zouwu.gitcode.com/login?popup=1&corp=csdn&state=atomcode_1760432156789&parent_origin=http%3A%2F%2F127.0.0.1%3A8766%2Fcallback

  Waiting for callback on port 8766...
```

把这个 URL 贴到浏览器就能调试前端页面。

### 客户端卡在 "Waiting for callback on port 8766..."

意味着 zouwu 没有把浏览器 302 回 `127.0.0.1:8766/callback`。检查：

- `parent_origin` 是否被 URL-decode 了
- redirect 的 `Location` header 是否完整
- 浏览器 devtools Network 面板看 zouwu 响应

### `/exchange` 返回了但 atomcode 报 "Broker response missing 'user' field"

JSON 结构不对。客户端期望的完整 shape 参考 [Response（成功）](#response成功)。

### 想手动测 `/exchange`

```bash
# 先在浏览器里手动触发 authorize 流程，拿到 code
curl -X POST https://test-zouwu.gitcode.com/exchange \
  -H "Content-Type: application/json" \
  -d '{"code": "<从浏览器 URL 里复制的 code>"}' \
  | jq
```

---

## 联系人

- atomcode 客户端代码：`/crates/atomcode-tui/src/auth/wecom.rs`
- Spec：`/docs/superpowers/specs/2026-04-14-wecom-login-design.md`
- 问题反馈：GitCode Issue / 内部 IM

zouwu 后端有任何协议字段需要调整，直接告诉 atomcode 团队，在 `BrokerResponse` / `parse_broker_response` 那一层适配即可，成本很低。
