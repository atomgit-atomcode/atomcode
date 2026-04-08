# AtomCode Eval — Case 编写指南

本目录存放批量 eval harness 的 case 集合。runner 脚本位于
`eval/scripts/run.sh`。完整设计见
[`docs/superpowers/specs/2026-04-07-batch-eval-harness-design.md`](../../docs/superpowers/specs/2026-04-07-batch-eval-harness-design.md)。

## 快速开始

```bash
# 跑全部 case
./eval/scripts/run.sh

# 只跑一个 case
./eval/scripts/run.sh --only 001-fizzbuzz

# 为所有 case 覆盖 provider（否则：case frontmatter 里钉了谁就用谁，
# 没钉就用 config.toml 里的 default_provider）
./eval/scripts/run.sh --provider siliconflow

# 查看结果
open eval/runs/<latest>/index.html
```

## 两种 case 格式

### Form A — 单文件（小体量 / 无 seed / 内联 seed）

```
eval/cases/
  001-fizzbuzz.md
```

Form A 适用场景：
- 不需要任何起始文件，或
- 只需要 1–3 个很小的文本文件（内联在 frontmatter 的 `[seed_files]` 里）

case id 必须和文件名（去掉 `.md`）完全一致。

### Form B — 目录（多文件 seed / 模拟真实项目）

```
eval/cases/
  010-rust-refactor/
    case.md
    seed/                ← 运行时会被拷贝到 cwd/
      Cargo.toml
      src/main.rs
```

Form B 适用场景：
- 需要一个多文件的起始工程
- seed 文件较大或是二进制
- 想直接在 IDE 里编辑 seed 文件而不是塞进 TOML 字符串

case id 必须和目录名完全一致。

## Frontmatter（TOML）

每个 case 的开头都是一个 `+++` 包裹的 TOML frontmatter 块：

```markdown
+++
id = "001-fizzbuzz"          # 必需，必须和文件名/目录名一致

description = "..."          # 可选，会显示在 index.html
timeout_secs = 60            # 可选，缺省 120
tags = ["code-gen", "smoke"] # 可选，V1 仅展示，不参与逻辑

# 可选 —— 把当前 case 钉到某个 provider。省略时会用 config.toml 的
# default_provider。--provider CLI 参数的优先级高于 frontmatter 钉死值
# 和 config 的默认值。
provider = "siliconflow"

# 仅 Form A 有效；Form B 用 seed/ 目录代替
[seed_files]
"hint.txt" = "useful hint"
"src/main.py" = """
print("placeholder")
"""
+++

下面是 prompt 正文，原样传给 atomcode -p
```

### 字段约束

- `id`：字符集 `[a-zA-Z0-9_-]`，必须和文件名/目录名一致（必需）
- `provider`：可选字符串。设置时不能为空字符串。省略 → 使用
  `config.toml` 的 `default_provider`。`--provider` CLI 参数优先级最高。
- `seed_files` 的 key：只能是相对路径，禁止 `..`，禁止绝对路径
- `seed/`（Form B）：禁止 symlink，软性大小限制 50 MB（仅警告）

## `-p` 模式能做什么，不能做什么

**正常工作**（95% 的 case）：
- 读文件（read_file / glob / grep / list_dir / …）
- 编辑已有文件（edit_file / search_replace）
- 创建新文件（create_file，路径不存在时）
- 跑常规 bash：`cargo build`、`pytest`、`npm install`、`git status`、
  `python script.py`、`curl` 等等
- 多步验证流程（"先写代码、再跑起来"）

**会被自动拒绝**（模型会收到一个 "denied" observation，可能会绕道，
但 case 的结果会被降级）：
- `rm -rf`、`rmdir` —— 用 edit_file 写空内容代替
- `git reset --hard`、`git push --force`、`git clean -f`
- `drop table`、`drop database`
- `mkfs`、`format`、`dd if=`、`chmod 777`
- 不带数字 PID 的 `kill -9`（`kill -9 12345` 没问题）

**完整拒绝清单见** `crates/atomcode-core/src/tool/bash.rs:430-450`。
不要写"标准答案依赖这些命令"的 case。

## 诊断与排查

当某个 case 看起来不对劲时，按以下顺序查：

1. **`runs/<ts>/<case-id>/meta.json`** —— exit_code / status / had_denial
   / wall_ms。如果 `had_denial: true`，直接跳到第 3 步。
2. **`runs/<ts>/<case-id>/cwd/`** —— 模型实际产出的文件系统。Form B
   case 推荐用
   `diff -ru eval/cases/<id>/seed/ eval/runs/<ts>/<id>/cwd/` 快速对比。
3. **`runs/<ts>/<case-id>/stderr.txt`** —— `[tool→ ...]` / `[tool← ...]`
   时间线，以及所有 `[approval-denied]` 行。按时间顺序看 agent 的工具
   调用轨迹。
4. **`runs/<ts>/<case-id>/home/logs/*.json`** —— 金矿。每一对
   (request / response) 文件都是一次完整的 LLM round-trip，含 messages、
   tool 定义、token 数、step 编号。request 文件是"我们发了什么"，对应
   的 `*_response.json` 是"模型回了什么"。

## 为什么是 TOML，不是 YAML？

runner 只用 bash + python 标准库。Python 3.11+ 自带 `tomllib`，但没有
对应的 yaml 解析器。改用 TOML 可以避免让每个使用者 `pip install pyyaml`。
TOML 的 `"""..."""` 多行字符串足以承载 prompt 和 seed 文件。这个决定
记录在
`docs/superpowers/plans/2026-04-07-batch-eval-harness.md`
的 "Deviations from spec" 段落。

## V1 的刻意限制

V1 **故意不做**以下事情（规划到 V1.5+）：
- 无 triage 徽章（`long-turn` / `repeat-tool` / `token-heavy` …）
- 无跨 run diff
- 无 `notes.md` 标注
- 无 `--rerun-failed`
- 无评分（LLM-as-judge 或硬断言）
- 无 multi-provider 矩阵
- 无 multi-turn case
- 无 `--dangerous-allow-all`（并且永远不会有，见 CLAUDE.md §3）

需要以上任何一项时，spec 里有对应的演进路径。
