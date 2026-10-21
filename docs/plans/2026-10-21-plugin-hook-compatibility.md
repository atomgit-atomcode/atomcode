# Plugin hook compatibility implementation plan

**Goal:** 修复 channel 多段路径包装，避免不兼容 hook 的无效授权与冗长错误。
**Architecture:** capabilities 是入口校验及授权 owner；CLI 投影状态，TUI 仅展示与选择动作。保持 fail-closed，不求值 JavaScript，不绕过目录、符号链接、内容哈希校验。不修改插件安装或自动授权。
**Tech Stack:** Rust，现有插件与 TUI 数据模型。

基线：release/v5.2.4，1df62c40a57390ac286e9ad1810a035ef7603b6f；已有 staged live-share 改动，保留。

1. cc_hooks.rs 抽固定 import 包装解析：逗号分隔的单引号字面量路径，拼为相对路径，交原有校验。测试多段、空格、逃逸和表达式拒绝。
2. hook_trust.rs 授权失败只返回插件身份和修复指引，不输出内部 invalid hash/整段命令；hooks list 保留诊断详情。
3. TUI plugins.rs 用明确三态代替布尔投影，CLI tui_plugins.rs 映射 invalid 为不兼容；不兼容不提供 Trust 动作，列表给出对应本地化标签；更新所有消费者。
4. 跑 capabilities（cc-hooks）、TUI、CLI 最小相关及受影响 crate 测试；fmt 和跨 crate compile gate。用户本轮未要求提交。
