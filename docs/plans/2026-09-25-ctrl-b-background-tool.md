# Ctrl+B：把正在跑的耗时工具转到后台（待办，未开工）

2026-09-25 记。用户要的是 Claude Code 的 Ctrl+B：**一个正在执行的耗时工具（典型是长
bash 命令）转到后台继续跑，模型不再等它、接着往下做，之后再取输出。** 它和 `/bg`
（把整个会话放到后台）不是一回事，别再混为一谈。

## 现状

- 底座已有：`crates/atomcode-capabilities/src/tools/bash/background.rs` 的
  `bash_start` / `bash_poll` / `bash_kill`——脱离前台跑、最多 64 个作业、走与前台 bash
  相同的审批与安全门（`permission_rules.rs` 的 `background_shell_tool_matches_command_rules`）。
- **产品没挂**：coding 运行时与 `tests/golden/differential/` 里都没有这三个工具，模型用不了，
  人也没有键能把正在跑的命令转后台。
- Ctrl+B 在新 TUI 里已空出来（「中断并发出排队的话」2026-09-25 改绑 Ctrl+X）。

## 要做的三步

1. **挂工具**：至少 `bash_poll`、`bash_kill`（转后台后模型要能取输出、能停）；`bash_start`
   一并挂上，让模型预判到长命令时一开始就放后台。挂行要登记 `describe_self`（见 AGENTS.md），
   差分 golden 的工具目录随之变化，按 AGENTS.md 说明理由。
2. **前台 bash 转后台**（新能力，要先写设计）：按 Ctrl+B 时不杀子进程，把它移交给后台作业表；
   这次工具调用立即返回「已转到后台，作业 X，用 bash_poll 取输出」，回合继续。难点是子进程
   现在归前台工具调用的 future 所有，要在不中断进程的前提下交接所有权；取消、回合结束、
   会话退出时作业的终态要写清楚（AGENTS.md「Runtime 生命周期不变量」）。
3. **TUI**：Ctrl+B 只在有前台 bash 在跑时生效；状态栏显示后台作业数（与「后台 N 个会话」
   分开）；一个查看 / 停止作业的入口。

## 判据（草拟）

- 一个 `sleep 30` 在跑时按 Ctrl+B：工具调用立刻结束并给出作业号，模型下一步照常进行；
  `bash_poll` 最终拿到命令的输出；进程从未被杀。
- 转后台的作业在会话退出时的处理有判据（停掉，或按设计保留）。
