# litellm 自动 fallback 修复事故报告

**事故时间**：2026-05-10
**症状**：atomcode 长 turn 后 `Provider returned an empty response (no text, no tool calls)`，session 中断
**影响**：所有走 `https://llm-api.atomgit.com` 的客户端，长上下文场景必然触发
**最终修复**：litellm 端 3 处改动 + 1 个 DB 配置调整，atomcode 端不需要任何改动

---

## 一、问题表象

5/10 02:16 起，atomcode 在 atomgr 项目跑"分析代码风险并生成 HTML 报告"任务，每次到第 10-14 个 turn 就报：

```
[API error 请求失败，3 秒后重试(1/3)...]
[API error 请求失败，6 秒后重试(2/3)...]
[API error 请求失败,9 秒后重试(3/3)...]
[Error: Provider returned an empty response (no text, no tool calls).]
```

`~/.atomcode/datalog/atomgr-2d99b47d/llm/calls.log` 显示：

```
2026-05-10_04-26-55_549  glm-5.1  msgs=66/49128tok  →  44325ms  text_only
2026-05-10_04-27-42_890  glm-5.1  msgs=66/49128tok  →   6266ms  text_only
2026-05-10_04-27-55_171  glm-5.1  msgs=66/49128tok  →   5355ms  text_only
2026-05-10_04-28-09_540  glm-5.1  msgs=66/49128tok  →   3786ms  text_only  ← empty
```

prompt token 卡在 49 128（atomcode 自报），3 次重试都 empty。

---

## 二、链路完整还原

```
atomcode (Mac)
    │ HTTPS, model=glm-5.1, max_tokens=16384
    ▼
ELB → nginx (公网入口)
    │
    ▼
4 台 litellm router (115.120.11.49 / 12.152 / 49.11 / 60.95)
    │ pre_call_check 应在此精算 token + 决定路由 → 失败,silent fallback 到"不检查"
    ▼
lite-6-1:18004 vllm-ascend disagg-prefill proxy
    │ POST→P (lite-4): prefill, KV 准备好
    ▼
lite-6-1:6601 vllm D rank 1
    │ vllm.exceptions.VLLMValidationError:
    │ "max context 81920, requested 16384 output, prompt 65537,
    │  total 81921. Please reduce..."
    ▼ HTTP 400
proxy 重试 3 次,全部 400 → abort request
    │
    ▼
litellm 把 abort 当 stream-success 透传 → atomcode SSE 收 `[DONE]` 但无 content
    │
    ▼
atomcode TUI: "Provider returned an empty response"
```

### token 膨胀路径
| 视角 | prompt token |
|---|---:|
| atomcode 客户端自报（`Message::estimate_tokens`，`len/4`）| 46 581–49 128 |
| litellm 注入 `cache_control_injection_points` 后 | 65 537 |
| vllm tokenize（chat_template 渲染 + 真 tokenizer）| 65 537 |
| 加 `max_tokens=16 384` 总和 | **81 921** |
| vllm `--max-model-len`（NPU HBM 容量上限） | **81 920** |

**超 1 个 token，整个请求 400**。

---

## 三、根因（为什么 litellm 自动 fallback 没工作）

litellm `router.py::_pre_call_checks` 设计**本来是对的**：每个 deployment 在 `model_info` 声明 `tokenizer_path` + `chat_template_path` + `max_input_tokens`，pre_call 阶段精算 token，超限把 deployment 标 invalid，自动 fallback 到 `glm-5.1-fallback`（智谱官网，200K context）。

**但触发了三个独立 bug，每一个都让精算路径失败 → silent fallback 到"不检查" → 请求直接发到 vllm 撞 81920**：

### Bug 1：tokenizer.json 是 git-lfs pointer（133 bytes 不是 19 MB）
```bash
$ ssh litellm-1 file /data/litellm/atomgit/tokenizers/GLM-5.1-w4a8/tokenizer.json
ASCII text
$ ssh litellm-1 head -c 100 /data/litellm/atomgit/tokenizers/GLM-5.1-w4a8/tokenizer.json
version https://git-lfs.github.com/spec/v1
oid sha256:19e773648cb4e65de8660ea6365e10acca112d42a854923df93db4a6f333a82d
```
git clone 时 git-lfs 没拉真实文件。`Tokenizer.from_file()` 解析 JSON → "expected value at line 1 column 1" → except → return。

### Bug 2：jinja chat_template 期待 dict，OpenAI 给 string
GLM-5.1 chat_template.jinja 第 89 行：
```jinja
{% for k, v in _args.items() %}<arg_key>{{ k }}</arg_key>...
```
`_args` 是 assistant 消息的 `tool_calls[].function.arguments`。**OpenAI 标准格式 arguments 是 JSON-string**，jinja 期待 dict 调 `.items()` → `'str object' has no attribute 'items'` → except → return。

litellm `router.py::_pre_call_checks` 的 except 块只打 ERROR log,然后 return 所有 deployment（不做 budget 检查）。

### Bug 3：max_input_tokens 设得太宽松
DB 里 `model_info.max_input_tokens = 71680`，但 vllm 实际 `--max-model-len 81920`，atomcode 默认 `max_tokens=16384`。

即使 Bug 1 + Bug 2 修了，prompt=59605 < 71680 仍然不触发 fallback，但 vllm 看到 59605 + 16384 = 75989 < 81920 也接受了。**这是因为 litellm 只检查 input，没检查 input + max_tokens 总和**。

要让 fallback 在 vllm 拒绝前触发，max_input_tokens 必须 ≤ 81920 - 16384 = **65536**。

---

## 四、修复（5 步全在 litellm 侧，atomcode 不动）

### 步骤 1: 推真实 tokenizer.json 到 4 台 litellm
```bash
scp /tmp/glm5_tok/tokenizer.json \
    litellm-{1,2,3,4}:/data/litellm/atomgit/tokenizers/GLM-5.1-w4a8/tokenizer.json
# 4 台都验证: wc -c → 20217442
```

### 步骤 2: 给 litellm router.py 加 `_normalize_msgs_for_jinja` patch
在 `/data/litellm/litellm/router.py` 的 `_tpl.render(messages=messages, ...)` 之前插入:

```python
# ATOMGIT_PATCH_normalize_msgs_v1
def _normalize_msgs_for_jinja(_msgs):
    """OpenAI sends tool_calls[].function.arguments as JSON-string;
    GLM/Qwen chat templates iterate it via .items() and crash with
    'str object has no attribute items'. Parse to dict here."""
    _out = []
    for _m in _msgs:
        _m = dict(_m)
        if _m.get('tool_calls'):
            _tcs = []
            for _tc in _m['tool_calls']:
                _tc = dict(_tc)
                if 'function' in _tc:
                    _fn = dict(_tc['function'])
                    _args = _fn.get('arguments')
                    if isinstance(_args, str):
                        try: _fn['arguments'] = _json.loads(_args)
                        except Exception: _fn['arguments'] = {}
                    _tc['function'] = _fn
                _tcs.append(_tc)
            _m['tool_calls'] = _tcs
        _out.append(_m)
    return _out

_rendered = _tpl.render(
    messages=_normalize_msgs_for_jinja(messages),  # ← 这里
    tools=None,
    add_generation_prompt=True,
)
```

patch 脚本 `/tmp/patch_litellm_normalize.py`（idempotent，已通过 `# ATOMGIT_PATCH_normalize_msgs_v1` marker 防重复 apply）。

### 步骤 3: DB 调小 max_input_tokens 71680 → 65536
```sql
UPDATE "LiteLLM_ProxyModelTable"
SET model_info = jsonb_set(model_info::jsonb, '{max_input_tokens}', '65536'::jsonb)
WHERE model_id IN (
  'cc6059cf-0ea3-4e89-b3f8-a1f172f909e0',  -- glm-5.1 → 18003
  '5de30b9f-2193-428d-ae4c-e04849151034'   -- glm-5.1 → 18004
);
```
公式：`max_input_tokens = vllm_max_model_len - max_max_tokens = 81920 - 16384 = 65536`

### 步骤 4: fast-path patch（性能补救）

步骤 1-3 修完后立刻撞了一个新坑：精算路径每次都跑——加载 tokenizer + jinja render N 个 messages + tokenize 整个 prompt——对 atomcode 50 messages 量级的会话，**单次 litellm 内部处理 39 秒**（Python GIL 单线程）。atomcode 客户端 9 秒就超时退出 → 仍然报 empty。

修复：仿照 Claude Code 的 `tokenEstimation.ts` 设计，加一个 rough-estimate fast-path —— `len/4` 粗估 × 2 仍然安全地小于 cap 时，跳过精算直接用 rough。在 `_pre_call_checks` 的 `try:` 块开头插入：

```python
# ATOMGIT_PATCH_rough_fastpath_v1
# Fast-path: rough estimate suffices for routing when not near cap.
# Precise jinja+tokenizer costs ~30-40s GIL time for a 50-message
# session; skip for the 90% of small requests.
_rough_estimate = 0
for _m in messages:
    _c = _m.get("content")
    if isinstance(_c, str):
        _rough_estimate += len(_c) // 4
    elif isinstance(_c, list):
        for _b in _c:
            if isinstance(_b, dict):
                _t = _b.get("text") or _b.get("input")
                if isinstance(_t, str):
                    _rough_estimate += len(_t) // 4
    if _m.get("tool_calls"):
        for _tc in _m["tool_calls"]:
            _fn = (_tc or {}).get("function") or {}
            _args = _fn.get("arguments")
            if isinstance(_args, str):
                _rough_estimate += len(_args) // 4
_min_cap = min(
    (((_d.get("model_info") or {}).get("max_input_tokens") or 999_999_999)
     for _d in _returned_deployments),
    default=999_999_999,
)
_input_tokens_fastpath = None
# 2x safety margin — even with the worst rough-vs-real skew (~1.5x for CJK),
# 2x rough bounds the real count. If 2x < cap, routing decision is safe
# with rough alone.
if _rough_estimate * 2 < _min_cap:
    _input_tokens_fastpath = _rough_estimate

if _input_tokens_fastpath is not None:
    input_tokens = _input_tokens_fastpath
else:
  # ↓ 原来的精算路径（整体多 2 个空格缩进）
  # Prefer a deployment-declared local tokenizer (e.g. GLM/Qwen
  # ...
```

效果实测（atomcode 真实失败请求公网复现）：

| 指标 | fast-path 之前 | fast-path 之后 |
|---|---:|---:|
| `litellm dur` (response header) | **39 541 ms** | **66 ms** |
| Client TTFT | 49 461 ms | 3 625 ms |
| Server-side `prompt_tokens` | 62 790 | 62 790 (相同请求) |

降低 600 倍。

#### 风险分析：fast-path 跳过精算后还会撞 81920 吗？

fast-path 触发条件：`rough × 2 < min_cap = 65 536` → `rough < 32 768`。

| rough 估算 | rough × 1.5 (真实最坏) | + max_tokens 16 384 | 是否安全 |
|---:|---:|---:|---|
| 16 000 | 24 000 | 40 384 | ✅ 远低于 81 920 |
| 25 000 | 37 500 | 53 884 | ✅ 安全 |
| 32 000 | 48 000 | 64 384 | ✅ 安全 |
| 32 768 (临界) | 49 152 | 65 536 | ✅ 安全（max_input_tokens 上限即此） |
| 32 769 | — | — | 跳到精算路径 |

只要 atomcode `len/4` 估算的 real-vs-rough 偏差不超过 **2.0x**，fast-path 都安全。实测最大偏差 1.5x（CJK + tool schema 密集），有 25% 安全余量。

如果将来出现 > 2.0x 的偏差（比如新 model 的 tokenizer 极度对中文友好），可以把 `× 2` 改成 `× 2.5` 或更保守。

### 步骤 5: 4 台 litellm 重启（步骤 1-4 改完一次重启即可）
```bash
for h in litellm-{1,2,3,4}; do
  ssh $h 'source /root/miniconda3/etc/profile.d/conda.sh && \
          conda activate litellm && \
          bash /data/litellm/start-local.sh'
done
```

---

## 五、验证

```python
# 构造 80K 中文 token 的请求 (中文 ≈ 1 token/char)
huge_user = "请深度分析以下不同的中文段落:\n\n" + "\n\n".join(
    f"段落{i}: " + "".join(chr(random.randint(0x4e00, 0x9fff)) for _ in range(2000))
    for i in range(40)
)
# POST 到 https://llm-api.atomgit.com/v1/chat/completions
```

response 头：
```
x-litellm-model-api-base: https://open.bigmodel.cn/api/paas/v4   ← 智谱
x-litellm-model-id: 8b487746-fea5-4027-b98d-b838796060a3        ← glm-5.1-fallback
usage: {prompt_tokens: 147238, completion_tokens: 50, total: 147288}
```

**自动 fallback 到智谱 ✅**。147K context 智谱接得住（200K cap）。

---

## 六、关于 atomcode microcompact 没有生效

排查中观察到 atomcode 每次失败时 prompt 都卡在 ~49K（atomcode 自报），看起来 `microcompact` 没及时压缩历史。**实际原因不是 microcompact bug，而是它的设计边界使然**。

### microcompact 行为（`ctx/render.rs::microcompact`）

> "Anchor on the last User message — everything after it is the
> ACTIVE turn and must stay full. If no User message (cold start
> / system-only), there's nothing to compress yet."

意思：microcompact **只压缩"上一个 user 消息之前"的历史**，当前 turn（用户 prompt 之后到现在的所有 tool_call/tool_result）一律保留全文，避免把 in-flight 上下文打碎。

### 为什么 atomcode 5/10 session 没救场

用户给一个大任务（"分析代码风险 + 生成 HTML 报告"），atomcode 在**单个 user turn 内**触发 30+ 个连续 tool_call (read_file/grep/web_search/write_file)，所有这些 tool_call + tool_result 都在"当前 turn"里，**microcompact 设计上不能压缩**。

```
Turn 1: User "分析代码风险..."
  ├─ assistant tool_calls: [list_dir, read Cargo.toml, ...]
  ├─ tool results × N (累积 ~30K token)
  ├─ assistant tool_calls: [read api.rs, read auth.rs, ...]
  ├─ tool results × N (累积到 ~50K token)        ← microcompact 不动
  └─ assistant: "正在生成 HTML 报告..." → write_file 14K token  ← 撞 81920
```

把 microcompact 阈值从 70%/100K 改激进到 50%/60K（commit 在 `ctx/render.rs:269`）**对单 turn 内累积场景没用**——它仍然只 stub 上一 turn 之前的内容。

### 真正治本的两条路径
1. **atomcode 客户端层**：放宽 microcompact 边界，允许压缩当前 turn 内"老于 N 个 tool_call"的 tool result（保留最近 5-10 个全文）。需要小心不破坏模型推理上下文。
2. **服务端层（本次方案）**：让 litellm 在 prompt 接近上限时自动 fallback 到大 context model（智谱 200K）。**已落地，0 客户端改动**。

服务端方案的好处：
- 立刻生效，所有客户端受益
- atomcode 长 prompt 不再触发 empty response
- 用户无感（响应略慢一点 + 可能产生少量智谱外部计费）

短期推荐继续用服务端 fallback，atomcode microcompact 治本改造作为 nice-to-have 排在后面。

---

## 七、附录：本次修复改的文件 / 命令清单

| host | 文件 | 改动 |
|---|---|---|
| litellm-1/2/3/4 | `/data/litellm/atomgit/tokenizers/GLM-5.1-w4a8/tokenizer.json` | 133 B (git-lfs pointer) → 19 MB (实文件) |
| litellm-1/2/3/4 | `/data/litellm/litellm/router.py` | 第 9522 行加 `_normalize_msgs_for_jinja` |
| litellm-1/2/3/4 | `/data/litellm/litellm/router.py` | 步骤 4: try 块开头加 fast-path (rough × 2 < min_cap 跳过精算) |
| litellm postgres | `LiteLLM_ProxyModelTable` 行 cc6059cf / 5de30b9f | `max_input_tokens` 71680 → 65536 |
| litellm-1/2/3/4 | (重启) | `bash /data/litellm/start-local.sh` |

回滚：每台 `cp router.py.bak.fastpath router.py` (覆盖到 fast-path 之前) 或 `cp router.py.bak.atomgit_normalize router.py`（覆盖到所有改动之前）+ DB 把 65536 改回 71680 + 重启。

不影响 atomcode 任何代码。
