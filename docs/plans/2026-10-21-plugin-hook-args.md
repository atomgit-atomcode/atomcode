# Plugin hook args implementation plan

**Goal:** 支持 planning-with-files 的 command + args hook 配置。
**Architecture:** manifest 保留 argv，loader / PluginCcHook 传给 HookConfig，daemon 的唯一宿主适配器转交，不加入 shell 字符串。插件脚本仍 canonicalize 并限制于内容已授权的插件目录；参数参与去重和授权哈希。旧无参数配置保持行为和哈希。
**Tech Stack:** Rust / serde / tokio::process。

基线 release/v5.2.4 a5b4e3a505e9bc9c0220c7a029514efec5f6237c，无 tracked dirty。

1. manifest、loader 增加默认空 args，去重区分 argv；hook trust 按参数数量/长度/顺序散列，有参数使用独立域，无参数保持旧格式。
2. HookConfig 保留独立 args；校验与执行走相同路径，先解析既有 command 再加入字面量参数，不拼 shell。拒绝 NUL、解释器选项入口和目录逃逸。
3. runtime_host 传递 args，更新 struct 初始化调用方；不改持久化协议，不修改已安装插件或自动授权。
4. 回归 manifest -> loader -> validation -> execution、空格/引号/元字符字面量、参数授权失效、去重与逃逸。运行受影响 crate 测试和 compile gate / fmt。

验证结果：capabilities（cc-hooks）1344 项、daemon 357 项、coding runtime hook 回归 1 项通过，compile gate 与格式检查通过。实际执行了自建参数回显脚本验证字面量 argv，未运行用户安装的 planning-with-files 插件。
