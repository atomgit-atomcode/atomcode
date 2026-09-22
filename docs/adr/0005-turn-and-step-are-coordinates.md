# turn 和 step 是块的坐标,不是块

状态: 提案中

## 背景

TUI 需要按 turn / step 分组与折叠(「折叠这一轮」「折叠这一步的所有工具调用」)。
最直觉的做法是让 turn 和 step 各自成为一种块,包住后面的块直到关闭。

## 决策

不设 span 块。每个块携带坐标:

```rust
pub struct Block { pub id: BlockId, pub at: Coord /* {turn, step} */, ... }
```

日志里每条事实**已经**带 `(turn, round)`,所以坐标是白拿的。
折叠目标统一为 `Target::{Block, Step, Turn, Kind}`。

## 放弃了什么

**span 块(开闭配对)。** 它更贴近「turn 包含 step 包含块」的心智模型,
也更容易表达任意嵌套。放弃它有四个理由:

- 分组变成**结构**而非**呈现**,于是换分组方式会动内容,`content_hash` 不再稳定,
  而整套冻结性判据依赖它;
- 多出「span 没关上」这个状态,以及它引发的一整类 bug(turn 被取消时怎么关、
  嵌套顺序错乱怎么办);
- 块需要知道自己属于谁,与「块只依赖事实」冲突;
- 坐标已经在日志里,span 是重复表达——**一件事两个家,必然分叉**。

代价:表达不了「跨 turn 的任意分组」(例如「把所有失败的工具调用归成一组」)。
`Target::Kind` 覆盖了按类型分组这一种;真需要任意分组时,得另设机制。

## 失效条件

- 若出现需要任意嵌套分组(不是按 turn / step / kind)的真实需求,
  坐标不够用,需要重新评估。
- 若日志的 `(turn, round)` 语义发生变化(例如 step 不再单调),
  坐标的稳定性前提失效。

## 相关

- [`docs/tui-composability.md`](../tui-composability.md) §一
- [ADR 0004](./0004-tui-stream-is-an-irreversible-block-sequence.md)
