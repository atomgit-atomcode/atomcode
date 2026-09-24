# Hook 系统完整时机列表（已过时）

> **本文描述的是已移除的 TOML/多引擎钩子系统的时机表。** 当前运行时只有一套生效的
> 钩子系统(JSON 版 `.hooks.json` / `hooks.json`),它有 **8 个事件**:
> `PreToolUse` / `PostToolUse` / `PostToolUseFailure` / `UserPromptSubmit` /
> `SessionStart` / `SessionEnd` / `Stop` / `StopFailure`。
>
> 完整的事件列表、触发时机、stdin 载荷与输出协议,请看 **[Hooks 指南](./hooks.md)**。
