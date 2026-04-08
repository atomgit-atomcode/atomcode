# SWE-bench Verified 评测子系统

在 atomcode 上跑 SWE-bench Verified 评测，产出 patch 并用上游 docker
grader 评分。完全自包含：删除 `eval/swebench/` 这个目录 SWE-bench
功能完全消失，Form A/B 评测 (`eval/cases/` + `eval/scripts/run.sh`)
和 atomcode 核心都不受影响。

**设计文档：** [`docs/superpowers/specs/2026-04-08-swebench-integration-design.md`](../../docs/superpowers/specs/2026-04-08-swebench-integration-design.md)

## 首次运行

### 1. 装依赖（用 venv 隔离）

Homebrew Python 3.11+ 默认锁住了 system pip（PEP 668），必须用虚拟环境：

```bash
# 在仓库根建 venv（只做一次）
python3 -m venv .venv-swebench

# 激活（每次开新 shell 跑 SWE-bench 之前都要执行）
source .venv-swebench/bin/activate

# 装依赖（只做一次）
pip install datasets swebench
```

激活后 shell prompt 会出现 `(.venv-swebench)` 前缀。之后 `./run.sh` / `./grade.sh` 都在激活状态下跑，`deactivate` 退出。

`.venv-swebench/` 已经在 `.gitignore` 里，不会被追踪。

### 2. 确认 docker 在跑（grade 阶段需要）

```bash
docker info
```

### 2.5. macOS 还要装 flock

```bash
brew install flock
```

Linux 自带 flock，跳过此步。

### 3. 预热缓存

```bash
./eval/swebench/run.sh --warm-cache
```

首次运行会交互式让你确认 dataset revision（会写入 `manifest.toml`）。
之后下载所有 12 个 repo 的 bare clone 到 `~/.cache/atomcode-eval/swebench/repos/`。

### 4. 先跑 pilot

```bash
./eval/swebench/run.sh --limit 20
```

~10 分钟 + 几十 k tokens。通过后再跑全量。

### 5. 全量 predict

```bash
./eval/swebench/run.sh
```

500 个 instance，几个小时，几十块钱。中断后 `./run.sh` 自动 resume。

### 6. Grade

```bash
./eval/swebench/grade.sh eval/runs/<latest>
```

用上游 docker grader 评分，结果回写到每个 meta.json。

### 7. 看结果

```bash
open eval/runs/<latest>/index.html
```

## 双轨评分解读

每个 run 产出两组独立的分数：

- **Primary (上游 binary):** `resolved/total` 比例，和其它 agent 榜单直接对比
- **Secondary (自定义效率):** 平均 turns / tokens / 成本 / 时间，按 outcome 分组

见 [`docs/GRADING.md`](../docs/GRADING.md) 里的 "SWE-bench dual-score" 章节。

## CLI 参考

### `run.sh` (predict)

```bash
./run.sh                              # 全量 + resume
./run.sh --limit 20                   # pilot
./run.sh --instance-id sympy__sympy-1 # 单 instance 调试
./run.sh --dry-run                    # 预览，不跑
./run.sh --warm-cache                 # 预热
./run.sh --fresh                      # 重跑，丢弃之前的 predictions.jsonl
./run.sh --retry-failed               # 只重试 failed/error/timeout 的
./run.sh --concurrency 8              # 覆盖并发
./run.sh --provider kimi              # 覆盖 provider
./run.sh --prompt minimal             # 覆盖 prompt 模板
```

### `grade.sh`

```bash
./grade.sh eval/runs/<ts>             # grade 所有 predicted 的 instance
./grade.sh --regrade eval/runs/<ts>   # 全部重新 grade
./grade.sh --instance-id <id> eval/runs/<ts>  # 单 instance
```

## 文件说明

| 文件 | 作用 |
|---|---|
| `manifest.toml` | 数据集 revision + filter + concurrency + prompt 模板配置 |
| `run.sh` | Predict 阶段入口 |
| `run_one_instance.sh` | Per-instance worker (不直接调用) |
| `fetch_dataset.py` | 首次从 HF 拉数据集 + 写 manifest revision |
| `grade.sh` | Grade 阶段入口 |
| `ingest_grading.py` | Grader 输出 → meta.json 回写 |
| `render_prompt.py` | prompt 模板渲染 |
| `pricing.py` | 硬编码 provider 定价表 |
| `prompts/default.md` | V1 默认 prompt 模板 |
| `cache/dataset.json` | 数据集本地缓存 (gitignored) |
| `tests/smoke.sh` | 端到端 smoke |

## 常见问题

**Q: `pip install` 报 `error: externally-managed-environment`**
A: Homebrew Python 的 PEP 668 保护。走上面的 venv 流程（`python3 -m venv .venv-swebench` + `source .venv-swebench/bin/activate`），不要用 `--break-system-packages`。

**Q: `fetch_dataset.py` 报 ``error: `datasets` package not installed``**
A: 忘了 `source .venv-swebench/bin/activate`。激活 venv 后再跑。

**Q: `grade.sh` 报 "docker daemon is not running"**
A: 启动 Docker Desktop / `systemctl start docker`

**Q: `fetch_dataset.py` 报 401/403**
A: 数据集需要 HF token。运行 `huggingface-cli login` 或设置 `HF_TOKEN` 环境变量

**Q: predict 中途被 early abort 杀了**
A: 连续 10 个 instance 失败会触发早退止损。读 `eval/runs/<ts>/*/stderr.txt` 找 root cause（常见：provider 配错、config 路径错）

**Q: 磁盘爆了**
A: `~/.cache/atomcode-eval/swebench/repos/` 占 ~50GB，`eval/runs/<ts>/` 每 run 占 ~30GB。删旧 run 目录即可

**Q: 换了 prompt 模板，可以 resume 吗**
A: 不行。不同 prompt 跑出来的 run 不可比，换模板前应该 `--fresh` 开新 run

## V1 刻意不做的事

见 [spec §2 Non-goals](../../docs/superpowers/specs/2026-04-08-swebench-integration-design.md#2-non-goalsv1-刻意不做的事)。多 prompt A/B 框架、cross-run 回归、LLM-as-judge 等留到 V2。
