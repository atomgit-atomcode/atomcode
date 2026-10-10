# TUI 插件 Hook 信任操作

## 目标

让用户不离开 AtomCode TUI 即可授予或撤销插件 hook 信任：命令行支持
`/plugin trust|untrust <插件>`，插件面板在已安装且带 hook 的插件上显示信任状态，
并提供对应动作。动作直接执行，不增加二次确认；成功后重建当前 runtime，使 hook
集合立即按新信任状态生效。

## 边界与数据流

信任文件仍由 `atomcode-capabilities::plugin::hook_trust` 独占写入，TUI 不读取路径、
不解析 manifest。CLI 宿主的插件端口把 `installed_plugin_hook_trust_status` 投影为
`PluginRow::hook_trusted: Option<bool>`：`None` 表示没有 hook，`Some(false/true)` 表示
未信任/已信任。端口的 `trust` 操作始终从当前安装重新取得 hook hash，再写入信任；
因此插件更新改变 hook 后仍会自动回到未信任状态。

命令入口先按已安装插件解析名字；同名多来源必须写成 `插件@市场`，不存在、无 hook
或状态已满足时返回明确结果。面板只在 `hook_trusted` 有值时提供 Trust/Untrust，避免
展示必然失败的动作。成功结果沿既有插件 job 通道显示，并调用 `HostCommand::Reload`。

## 验证

- 纯面板测试覆盖未信任→Trust、已信任→Untrust，以及无 hook 不出现动作。
- 命令测试覆盖名称解析和端口调用。
- CLI 端口测试覆盖当前 hash 写入、撤销与状态投影。
- 运行受影响 crate 测试、格式检查及 `gates/compile.sh`。
