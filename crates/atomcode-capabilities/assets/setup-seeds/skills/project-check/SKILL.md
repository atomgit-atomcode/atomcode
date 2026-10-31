---
name: project-check
description: 在手机上跑一遍项目的自检（格式 / lint / 测试）并给结论。用户说「检查一下」「跑测试」「提交前确认」时使用。
allowed-tools: [bash, read_file, grep, glob]
---

# 项目自检

目标是用**最少**的命令拿到一个可信的结论，不是把整条 CI 搬到手机上。

## 步骤

1. **认项目**：先看顶层有什么构建入口 —— `package.json`、`Cargo.toml`、`pyproject.toml`、
   `Makefile`、`go.mod`。只跑项目自己声明过的检查命令，不要自己发明。
2. **从小到大**：先跑受影响的单个测试或单个包，通过了再决定要不要全量。
   手机上跑几分钟的大项目本来就吃力，别把时间花在等上。
3. **失败就停在第一现场**：读那条命令的真实输出，改到通过，再往下走。

## 这台设备（Android）的已知限制

- 工作目录 `/storage/emulated/0` 挂的是 `noexec`：`./script.sh` 会 `Permission denied`，
  用 `sh script.sh`。
- 没有 `node` / `npm`；`python3` 是 RustPython（标准库可用，装不了第三方包），
  `awk` / `jq` / `sed` 在。
- 真要跑大项目构建，优先考虑放到配对机器上跑，而不是硬扛。

## 汇报格式

一行结论 + 失败项的命令与关键错误行。不要贴整段日志——用户看的是结论。
