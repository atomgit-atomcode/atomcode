# 配置重载保留会话存储绑定

开始基线：`release/v5.2.4`，`662db4bf53d2fec6164e4ecd5511be235904a149`。已有 WebUI 的 CwdPicker.tsx / app.css 改动保留；工作期间另一任务已将它们提交为 `c0d592bf9`，本任务不修改这些文件。

## Owner 与错误边界

CodingRuntime 是 live agent 唯一 owner。SessionBinding 持有会话 ID、SessionManager 和共享 OS 锁的 SessionLease；SessionManager 指向的 bucket 是会话事件日志的存储位置，工作目录用于当前工具/项目能力。重配置不应把工作目录当成当前会话存储位置的第二权威。

当前 matching_session_lease 仅转交 lease，parts 又按配置工作目录创建 manager。项目标记变化或配置工作目录改变后，旧租约与新 manager 发生 LeaseMismatch。validate_active_lease 的严格路径检查正确，不修改；不扫描全局目录按 ID 猜会话、不从 meta.cwd 反推位置、不解锁重抢、不创建空会话。

## 实现

内部 SessionLeaseSource 区分 Existing（当前 binding 的 manager + lease）和 Transferred（宿主/importer 显式提供的 lease）。当前会话的重配置复用完整绑定，Resume/ExternalSnapshot 仍通过该 manager 读取权威日志。Transferred 继续根据请求配置创建 manager 并严格验证，不接受来自另一个 bucket 的同 ID 租约。Fresh 和 ChangeDirectory 不带旧绑定，继续产生新会话。

保留活动会话时，ReloadConfig 不允许改变 ProductDirs，预检阶段给出重启并显式恢复会话的提示；不静默把日志写回与新产品目录不同的位置。此拒绝发生在候选创建与 MCP 撤权前，旧 generation 和状态保留。普通候选准备失败沿用现有回滚/终态；未新增生命周期、协议、持久化格式或 legacy fallback。

## 验证

回归覆盖：不同 bucket 含相同 ID 时，配置 cwd 改变后重载仍读写原日志、不覆盖另一份会话、原锁不中断；相同 cwd 的项目 bucket 标记改变后，配置和能力重载仍使用原存储；改变产品数据目录明确拒绝且旧 runtime 可继续执行；显式转入另 bucket 的同 ID 租约仍失败。新增 async 判据使用暂停时钟。

运行 coding crate 测试与编译闸门、fmt/diff 检查。CLI/TUI/daemon 的配置与能力重载共享这一 runtime seam；没有改 driver 协议。现有 fresh、cd、prepared resume、busy/cancel、失败重配置与持久化测试随 coding crate 验证。Windows 实机问题依然需要在发布构建验收，平台无关测试不能替代它。

执行结果：coding 全套 1013 项通过，9 项按配置跳过；编译闸门与 fmt/diff 检查通过。临时将 manager 恢复为按配置目录重建后，两条存储变化判据均以与反馈一致的 LeaseMismatch 判红；已恢复修复。不同 bucket 同 ID 的判据还注入一次候选 provider 构建失败，验证旧 generation、工作目录与租约保留，再重试并在原日志写入新回合。能力重载判据显式强制重建，避免只命中轻量技能刷新路径。
