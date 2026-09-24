# Hook CLI 命令使用指南（已过时）

> **`atomcode hooks` 这组命令仍然可用**,但本文是围绕已移除的 TOML 钩子系统写的,
> 里面的配置示例(`hooks.toml`、`[[hooks]]` 等)已不再适用。当前唯一生效的是 JSON 版
> `.hooks.json`,命令的权威说明见 **[Hooks 指南](./hooks.md)** 的 CLI 一节。

现存命令(作用于生效的 `.hooks.json` 系统):

```bash
atomcode hooks list           # 已加载的钩子,按事件分组
atomcode hooks paths          # 实际读取的文件(带 ✓/✗)
atomcode hooks test <NAME>    # 用合成载荷试跑一个钩子
```

`hooks test <NAME>` 按**事件名**(如 `PreToolUse`)或 **command 子串**匹配 —— 不是
`.hooks.json` 里的键名(键名加载后不保留)。详见 [Hooks 指南](./hooks.md)。
