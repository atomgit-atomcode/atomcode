# AtomCode Eval

批量评测 atomcode 在不同任务上的表现。`atomcode -p` 单 prompt 跑每个 case，全程
归档（cwd / stdout / stderr / 多轮 LLM trace），生成 HTML 报告供人工回看。
V1 不自动评分，评分留给 V1.5 的评分 agent（见 [docs/GRADING.md](docs/GRADING.md)）。

## 快速开始

```bash
# 跑全部 case
./eval/scripts/run.sh

# 只跑一个
./eval/scripts/run.sh --only 001-fizzbuzz

# 看结果
open eval/runs/<latest>/index.html
```

## 目录

- [`scripts/`](scripts/) — runner、解析器、渲染器、harness 自身的测试
- [`cases/`](cases/) — 31 个手写 case（详见 [docs/CASES.md](docs/CASES.md)）
- [`docs/`](docs/) — 设计文档：
  - [`AUTHORING.md`](docs/AUTHORING.md) — 怎么写一个 case
  - [`CASES.md`](docs/CASES.md) — 现有 case 索引和能力覆盖图
  - [`GRADING.md`](docs/GRADING.md) — 给未来评分 agent 的指南
- `runs/` — gitignored 的运行产物
