# 异步 Webhook 和批量发送指南（已移除）

> **这套异步/批量 webhook 系统已不再触发。** `hooks.toml` 里的 `[[async_webhooks]]`
> (以及同步 `[[webhooks]]`、`[[hooks]]` 脚本钩子、内置 Rust 钩子)在当前运行时
> **不会被执行**。唯一生效的钩子系统是 JSON 版 `.hooks.json`,见
> **[Hooks 指南](./hooks.md)**。

## 想要异步 / 批量投递怎么办

见 [Webhook 指南](./webhook-guide.md) 的等价做法:用一个 `.hooks.json` 的 shell 钩子
接住 stdin 载荷,把它写进本地队列/文件,由你自己的后台进程批量投递 —— 钩子本身立即
返回,不阻塞回合,也不依赖已移除的引擎。
