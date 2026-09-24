# Hook 系统技术架构（已过时）

> **本文描述的是已移除的架构**("`HookEngine` 统一调度、三种 Hook 实现共存")。
> 那套 TOML ScriptHook / Webhook / 内置 Rust 钩子在当前运行时**不再触发**。
>
> 现在只有一套生效的钩子系统:JSON 版 `cc_hooks`,读取 `$ATOMCODE_HOME/hooks.json`
> 与 `<项目>/.hooks.json`,通过 stdin 传 JSON 载荷、按 CC 退出码契约执行 shell 命令。
> 用法与协议见 **[Hooks 指南](./hooks.md)**;实现见
> `crates/atomcode-capabilities/src/cc_hooks.rs`。
