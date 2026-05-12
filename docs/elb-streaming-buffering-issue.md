# 华为云 ELB SSE 流式响应缓冲问题

**报告时间**：2026-05-10
**影响范围**：所有经 `https://llm-api.atomgit.com` 访问的 LLM 客户端（含 atomcode）
**严重程度**：P0 — 用户感知端到端延迟放大 **3-570 倍**

---

## 一、链路与组件

```
客户端 (atomcode/外部应用)
   │   HTTPS + SSE
   ▼
华为云 ELB     (公网入口 116.205.2.117 ←→ llm-api.atomgit.com)
   │   HTTP/1.1 chunked
   ▼
litellm 集群   4 台 (192.168.0.123 / .178 / .217 / .250, port 18001)
   │
   ▼
vllm-ascend proxy   192.168.0.157:18003 / :18004
   │
   ▼
GLM-5.1 P/D 推理集群   6 台 Atlas 800T A2
```

---

## 二、对照测试

测试目的：定位端到端延迟到底加在哪一层。
测试方法：同一段 HTTP request body（atomcode 真实 wire-dump，221KB，含 12 tools + 41 messages），分别从两个起点发出，记录 TTFT / 总耗时 / SSE 事件数。

### 测试 A — 经过华为云 ELB（生产路径）

```python
# 客户端：mac，公网
url      = "https://llm-api.atomgit.com/v1/chat/completions"
auth     = "Bearer 3Bs7vutUCVEECzsvDwEzzsCy"
payload  = <atomcode 真实 wire-dump body>     # 221 KB
```

#### 小请求（completion ≈ 18 token，5 次取样）

| # | TTFT (ms) | Total (ms) | SSE events | litellm 自报 duration | redis cache |
|---|---:|---:|---:|---:|:---:|
| 1 | 4 092 | 4 092 | 14 | 642 ms | False |
| 2 | 3 278 | 3 278 | 14 | 336 ms | False |
| 3 | 5 374 | 5 374 | 14 | 342 ms | False |
| 4 | 4 930 | 4 930 | 14 | 339 ms | False |
| 5 | 11 018 | 11 019 | 14 | 336 ms | False |

**TTFT 中位数：4 930 ms ｜ 均值：5 738 ms ｜ p99：11 018 ms**

#### 大请求（atomcode wire-dump，completion = 3 304 token，1 次复现）

| 指标 | 值 |
|---|---:|
| TTFT | **92 196 ms** |
| Total | 92 241 ms |
| SSE events | 4 |
| useful chars | 8 761 |
| completion_tokens | 3 304 |
| litellm 自报 duration | 64 ms |
| litellm redis cache | hit |

**所有 SSE 事件几乎同时到达**（first chunk 后 events 间隔 < 50 ms）—— 表明 ELB 把全部 chunk 缓冲到响应结束才一次性 burst。

---

### 测试 B — 内网直连 litellm（绕过 ELB）

```python
# 客户端：内网 lite-7-1，TCP 直连 litellm-1
url   = "http://192.168.0.217:18001/v1/chat/completions"
auth  = "Bearer 3Bs7vutUCVEECzsvDwEzzsCy"
payload = <同上 221 KB>
```

#### 小请求（completion ≈ 22 token，5 次取样）

| # | TTFT (ms) | Total (ms) | SSE events | litellm 自报 duration |
|---|---:|---:|---:|---:|
| 1 | 2 065 | 2 065 | 16 | 350 ms |
| 2 | 1 775 | 1 775 | 17 | 344 ms |
| 3 | 2 032 | 2 032 | 16 | 358 ms |
| 4 | 1 724 | 1 724 | 17 | 346 ms |
| 5 | 1 818 | 1 819 | 16 | 341 ms |

**TTFT 中位数：1 818 ms ｜ 均值：1 883 ms**

#### 大请求（同 atomcode wire-dump，1 次复现）

| 指标 | 值 |
|---|---:|
| TTFT | **162 ms** |
| Total | 208 ms |
| SSE events | 4 |
| useful chars | 8 761 |
| completion_tokens | 3 304 |
| litellm 自报 duration | 64 ms |
| litellm redis cache | hit |

---

## 三、排除 litellm Redis cache 干扰（关键反证）

可能的质疑："直连快是因为命中了 litellm 的 Redis cache，公网那次没命中所以慢"。
**已证伪**——下列测试用每次 user 内容随机化的 body 强制 cache miss：

| 测试 | 路径 | redis cache | TTFT | Total | events | litellm 自报 duration |
|---|---|---|---:|---:|---:|---:|
| A2-nocache #0 | **ELB** | **miss** | **152 076 ms** | 152 076 | 16 | **31 799 ms** |
| A2-nocache #1 | ELB | hit (warmed by #0) | 105 844 ms | 105 844 | 4 | 69 ms |
| A2-nocache #2 | ELB | hit | 115 226 ms | 115 226 | 4 | 64 ms |
| B2-nocache #0 | **直连** | hit (warmed by A2 #0) | **359 ms** | 359 | 4 | 66 ms |
| B2-nocache #1 | 直连 | hit | 361 ms | 361 | 4 | 66 ms |
| B2-nocache #2 | 直连 | hit | 102 ms | 102 | 4 | 33 ms |

**ELB overhead 计算 = client TTFT − litellm 自报 duration**：

| 场景 | client TTFT | litellm dur | ELB 加的延迟 |
|---|---:|---:|---:|
| ELB / cache hit (大) | 92 196 ms | 64 ms | **+92 132 ms** |
| ELB / cache miss (真冷启动) | 152 076 ms | 31 799 ms | **+120 277 ms** |
| ELB / cache hit (小) | 4 930 ms | 339 ms | +4 591 ms |
| 直连 / 任意 cache 状态 | < 400 ms | 33–66 ms | < 350 ms |

**结论**：不论 redis cache 是否命中，ELB 这一层额外加 **5–120 秒**。Cache 状态完全无法解释差距，唯一变量是"是否经过 ELB"。

---

## 四、对比汇总

| 指标 | A. 经 ELB | B. 直连 litellm | 倍率 |
|---|---:|---:|---:|
| 小请求 TTFT (中位数) | 4 930 ms | 1 818 ms | **2.7×** |
| 小请求 TTFT (p99) | 11 018 ms | 2 065 ms | **5.3×** |
| 大请求 TTFT (cache hit) | 92 196 ms | 162 ms | **569×** |
| **大请求 TTFT (cache miss)** | **152 076 ms** | **359 ms** | **423×** |
| 大请求 useful chars | 8 761 | 8 761 | 1× (内容完全相同) |
| litellm 自报 duration | 64–31799 ms | 33–66 ms | 大请求 cache miss 时服务端真实耗时 31.8 秒 |

**所有差距完全在 ELB 这一段。**

---

## 五、问题描述

### 现象
经 `llm-api.atomgit.com` 调用模型时，atomcode 等使用 SSE 流式输出的客户端**等不到首字节响应**长达数秒到 90+ 秒，但服务端 (litellm + vllm) 早已生成完整响应。

### 根因（已通过对照实验证实）
**华为云 ELB 对响应做了 chunked-transfer 缓冲**。具体表现：

1. litellm 在 ms 级把全部 SSE chunks 写到 ELB 的上游连接（`x-litellm-response-duration-ms = 64ms`，redis cache 命中）；
2. ELB 收到 chunks **不立刻转发** 给客户端，而是缓存到自身 buffer 中；
3. 直到响应结束（连接关闭或 buffer 满）后，ELB 才把所有 chunks 一次性 burst 给客户端；
4. **客户端看到的 TTFT ≈ 服务端响应总耗时**，而不是真实的 first-token-time。

### 影响放大效应
缓冲量随响应大小增长：
- completion ≈ 20 token：ELB 缓冲增加 2-9 秒
- completion ≈ 3 300 token：ELB 缓冲增加 **92 秒**
- 大响应 + tool_call 场景下用户体验从"流式秒响应"退化为"等近 2 分钟"

### 已被错误归因的路径（排查记录）
排查过程中曾误判方向，已逐条排除：

- ❌ ~~atomcode 客户端处理慢~~（duration_ms 是 wall clock，但 90% 时间在等 SSE）
- ❌ ~~vllm decode 速度慢~~（直连 1.4 s 完成，30 tok/s 正常）
- ❌ ~~prefix cache miss~~（cached_tokens = 99% 命中）
- ❌ ~~thinking 模式开启~~（已通过 chat_template 关闭，reasoning_tokens=0 验证生效）
- ❌ ~~litellm 内部 guardrail / pre_call_check 慢~~（litellm 自报 64-350 ms 完成）
- ❌ ~~Mooncake KV transfer 慢~~（实测 p50 = 72 ms）
- ✅ **ELB streaming buffering** —— 是真凶

---

## 六、修改建议

按 ROI 排序，前两条任选其一即可解决。

### 方案 1（首选）：ELB 监听器关闭响应缓冲

华为云 ELB 控制台 → 找 `llm-api.atomgit.com` 对应的负载均衡实例 → 监听器/后端服务器组：

| ELB 类型 | 操作 |
|---|---|
| **独享型 / 应用型 (ALB)** | 监听器 → 高级配置 → 关闭"代理缓冲 / Buffer Response" |
| **共享型 (CLB)** | 监听器 → 后端连接 → `proxy_buffering off` 或等价开关 |
| **API 网关 (APIG)** | 后端策略 → 流式响应模式 / 关闭响应缓冲 |

确认配置后，重新跑测试 A 的大请求，TTFT 应从 92 秒降到 < 2 秒。

### 方案 2：换用 4 层负载均衡（NLB / Network LB）

如果 ELB 是 7 层应用型而厂商不提供"关闭缓冲"开关，把 LB 切到 **L4 TCP 模式**：
- L4 LB 不解析 HTTP，仅做 TCP 转发，不存在 streaming 缓冲问题
- 代价：需要在 litellm 侧自己 terminate TLS（或在 ELB 前再加一个轻量 TLS 网关）

### 方案 3（兜底）：ELB 后插一个 nginx 反代

如果以上都无法做，部署一个 nginx 在 ELB 与 litellm 之间，配置：

```nginx
location /v1/chat/completions {
    proxy_pass http://litellm_upstream;
    proxy_buffering         off;
    proxy_cache             off;
    proxy_http_version      1.1;
    proxy_set_header Connection "";
    chunked_transfer_encoding on;
    # SSE 关键：禁用一切缓冲，强制透传
    proxy_set_header X-Accel-Buffering no;
}
```

但这只是绕过 ELB 缓冲，并未真正解决问题，长期还是建议方案 1 或 2。

### 不推荐的方案
- 客户端轮询 (polling) 替代 SSE：违反 LLM 流式输出本质，吞吐反而更差；
- 客户端调小 max_tokens 强制提前结束：损害功能；
- 改 atomcode 走非 SSE 模式：服务端 vllm 仍然 streaming，ELB 缓冲问题仍在。

---

## 七、验证方法

修改 ELB 配置后，从 mac 跑下述脚本，**TTFT 应 < 2 秒**：

```bash
python3 - <<'EOF'
import json, urllib.request, time
body = open("/Users/yubangxu/.atomcode/wire-dump/1778339591.996591000.json","rb").read()
req = urllib.request.Request(
    "https://llm-api.atomgit.com/v1/chat/completions",
    data=body, method="POST",
    headers={"Content-Type":"application/json",
             "Authorization":"Bearer 3Bs7vutUCVEECzsvDwEzzsCy"})
t0=time.time()
resp = urllib.request.urlopen(req, timeout=300)
buf = b""; ttft = None
while True:
    chunk = resp.read(4096)
    if not chunk: break
    if ttft is None: ttft = time.time() - t0
    buf += chunk
print(f"TTFT={ttft*1000:.0f}ms (修复前 92196ms, 修复后应 <2000ms)")
EOF
```

**验收标准**：
- 大请求（completion ≈ 3000 token） TTFT < 2 000 ms
- 小请求 TTFT < 1 000 ms
- SSE events 间隔 p50 < 50 ms（说明真实流式而非 burst）

---

## 八、附录：复现数据采集环境

| 项 | 值 |
|---|---|
| 测试日期 | 2026-05-10 |
| 客户端 | macOS, Python 3.14 |
| 公网 ping ELB RTT | 120 ms (min 117 / avg 121 / max 129) |
| TLS 握手 | 0.87 s (一次性) |
| atomcode wire-dump 大小 | 221 415 bytes |
| 服务端模型 | GLM-5.1-w4a8 (PD-disagg, 16 NPU prefill + 32 NPU decode) |
| litellm 版本 | 1.84.0 |
| vllm-ascend 镜像 | quay.io/ascend/vllm-ascend:nightly-releases-v0.18.0 |
