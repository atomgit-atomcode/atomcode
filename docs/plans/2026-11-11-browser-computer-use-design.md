# 浏览器与跨平台 Computer Use 设计

状态：设计完成，新增能力尚未实现。现有 macOS Computer Use 不因本文扩大支持范围。

## 基线与范围

- AtomCode：`release/v5.2.7`，`5760f5e25e10f2ee66567a4d47481b811699299f`。
- 本地 Codex：`/Users/theo/Documents/workspace/codex`，`806d9732c9`。
- AtomCode 当前 provider 诊断相关改动属于其他工作，保留且不纳入本设计提交。
- 用户要求参考本地 Codex 实现，采用外部后端、按需安装，控制主程序和仓库体积。

本轮目标是明确可实施的浏览器接入方案和后续边界，不实现新的桌面后端，不扩展版本号，不改模型 provider，不提交浏览器、扩展、虚拟机或后端二进制。

## 本地 Codex 的证据

以下路径相对于上述 Codex 仓库；它们是设计参考，不是 AtomCode 的依赖。

| 源码 | 可确认的事实 | 不能据此推断的事实 |
| --- | --- | --- |
| `codex-rs/config/src/computer_use.rs` | macOS bundle ID、Windows AUMID 和 EXE 标识的权限配置 | Windows 桌面执行后端已经在此仓库完整实现或可直接复用 |
| `codex-rs/config/src/browser_use.rs` | 按 origin 配置访问、上传、下载和完整 CDP 权限 | 普通 MCP 工具审批自然具备这些权限粒度 |
| `codex-rs/core-plugins/src/loader.rs::load_plugin_mcp_servers_from_manifest_with_format` | 从插件清单或配置文件加载 MCP，应用插件策略 | 应把插件后端编译到 coding runtime |
| `codex-rs/plugin/src/bundled_hooks.rs` | 特定插件在 Stop、Interrupt、SubagentStop 时调用 `turn_ended` | 该调用能撤销已经执行的 GUI 操作，或任意 MCP 都支持同名工具 |
| `codex-rs/tui/src/history_cell/computer_activity.rs`、`mcp.rs` | 聚合 `cua_repl` 操作，保留失败、截图、详情及顺序 | AtomCode 应按这个服务名硬编码所有浏览器工具 |
| `codex-rs/core-plugins/src/marketplace_policy.rs` | 能识别外部 primary runtime 缓存中的插件目录 | Codex 安装包在所有发行渠道都不携带相关运行时 |

未在这次检查中找到完整原生桌面执行后端。配置结构不能当作平台实测结果，也不能推断 Linux 或 HarmonyOS PC 的支持情况。

## AtomCode 当前能力与缺口

已有 `atomcode mcp add`、`/mcp` 和 Playwright 配置示例，见 `crates/atomcode-cli/src/main.rs` 和 `site/docs/{en,zh}/mcp.html`。因此浏览器 MCP 的一般工具调用不需要新建 agent、第二运行时或第二 MCP 协议栈。

`crates/atomcode-capabilities/src/mcp/tool.rs` 根据 `readOnlyHint`、服务器 trust、autoApprove 和已有授权决定审批；“始终允许”按工具授权，不按站点授权。读取网页也可能发送请求、携带登录态，因此不能把 read-only 标记当作站点访问许可。第一阶段明确沿用这些语义，不声称提供按站点强制授权。

`transport_stdio.rs::send_request_with_reconnect` 已阻止结果未知的已发送工具调用自动重放；`StdioClient::drop` 已做宿主进程的尽力清理。保留这些保护。`McpClient` 当前没有通用 `turn_ended`、浏览器关闭或撤销接口；杀死 MCP 进程也不能证明远端浏览器或所有子进程均已关闭。

macOS Computer Use 的本地配置服务位于 `crates/atomcode-cli/src/computer_cmd.rs`，TUI 经 HostCommand 调用，显式 reload 复用既有 runtime 重载。浏览器遵循相同所有权边界。

## 方案选择

1. **外部 MCP 后端加轻量配置入口（采用）**：复用工具发现、图像返回和审批，提供可解释的启停、检查和浏览器指导。主程序仅增加 Rust 接入代码、技能及文档，外部软件由用户安装。
2. **仅维护手工 MCP 示例**：改动最小，适合已有用户；缺少一致的检查、隔离模式和故障说明，保留为高级入口。
3. **内置浏览器或移植完整 CUA REPL**：安装依赖、升级和平台测试范围较大，暂不采用。Codex 插件接口不能证明其运行时可供第三方直接复用。

Playwright MCP 是第一阶段候选，优先独立隔离浏览器会话。实施前核对并固定实际验证的后端版本、Node 要求、浏览器安装方式和开关；不使用浮动 latest 作为可复现验收基线。连接现有登录态浏览器、扩展模式和远程 CDP 不在第一阶段默认配置中。

## 第一阶段：浏览器接入

### 入口与所有权

- 拟新增 `atomcode browser status|doctor|enable|disable`；TUI 提供对应 `/browser` 和显式 `reload`。这些是拟实现接口，目前不可当作已发布命令使用。
- 配置服务由 CLI host 持有，接收 `ProductDirs`。浏览器与 MCP 子进程由现有 MCP 客户端/后端持有，live agent 继续由 `CodingRuntime` 持有。
- `status` 只读配置；`doctor` 不联网安装、不启动用户浏览器、不读取历史或登录态。若只能确认后端可执行和版本，输出必须说明浏览器运行状态尚未验证。
- `enable` 只管理带自身 ownership 标记的用户 MCP 项和原版指导技能。同名自定义项、用户技能修改、自动加载 opt-out 均保留；缺依赖时失败，不自动安装。
- 默认 trust=false、autoApprove=[]；不自动勾选“始终允许”。项目同名覆盖和 MCP pause 仍按既有机制处理。
- 设置更改不冒充实时连接变化。`reload` 走既有重载命令，沿用 busy、失败和 session addressing 语义。

### 模型与会话行为

- 初版通过显式 `$browser-use` 调用技能，避免直接挪用 desktop 自动分类规则。后续若需要自动选择，再扩展现有 skills-host 分类并覆盖中文、英文、代码和拒绝操作场景。
- 页面结构操作优先后端的快照/元素工具；截图任务要求视觉模型。页面内容、弹窗和工具返回均不能授予新权限。
- 操作后重新观察，使用当前 tab 和最新元素引用；断连或结果未知时先核对状态，不重放提交动作。
- 隔离会话不导入用户真实浏览器 profile、Cookie、密码或历史。登录由用户明确处理，不从配置复制凭据。
- 不开放额外 shell、任意浏览器代码执行、上传或安装工具作为绕过审批的捷径。实际暴露工具与后端版本一起审查；必要时使用现有工具过滤能力，先验证其可用性。

### 预期改动位置

- `crates/atomcode-cli/src/browser_cmd.rs`：本地配置和诊断；`lib.rs`、`main.rs`：命令注册及无副作用早期分发。
- `crates/atomcode-cli/assets/browser-use/SKILL.md`：浏览器指导，不写凭据、不预授权工具。
- `crates/atomcode-host-api/src/lib.rs`、`crates/atomcode-cli/src/host.rs`：有明确 session 地址的本地操作和显式不支持错误。
- `crates/atomcode-tui/src/commands.rs`、`crates/atomcode-i18n/src/screen/`：命令、补全与双语帮助。
- `crates/atomcode-cli/tests/browser_setup.rs` 及相应单测：真实子进程启动路径与配置冲突测试。
- `site/docs/en/mcp.html`、`site/docs/zh/mcp.html`：安装、使用、体积和权限边界。

优先复用 MCP 配置函数、原子写和 SetupLock；不为两个设置入口建立无边界的集成框架。跨多个文件的已存在配置服务暂不做无关重构。

### 验收

1. status、缺配置的 disable、失败 doctor/enable 不创建产品目录；自定义同名配置、符号链接和并发修改不能被覆盖。
2. 启用/禁用只影响归属明确的配置，保留其他 MCP；失败明确报错，不假成功。技能与配置的分步写入语义应与现有服务一致并有说明。
3. CLI/TUI 参数、session 不匹配、忙时 reload、Host 不支持均有可观察终态。
4. 在临时 profile 和本地受控测试网页中验证打开、读取、填写、截图、审批拒绝，以及通过唯一提交计数识别未知结果后的重复提交。
5. macOS、Windows、Linux 各自实测后才在支持矩阵标为已验收。不能把同一 Node 后端可安装当作三端验收。HarmonyOS PC 单列未验证。
6. 运行受影响 crate 的 nextest、`bash gates/compile.sh`、格式和 lock 检查。不把无关 provider 修改混入验证或提交。

## 第二阶段：权限和生命周期强化

这是独立工作，不能由第一阶段的技能提示宣称完成。

- **站点权限**：先选择真正执行策略的后端或执行边界。导航 URL 过滤无法覆盖重定向、弹窗、iframe、子资源和任意 CDP；上传/下载也要在实际执行处限制。未有执行方前不添加会造成安全假象的公开权限配置。
- **取消与清理**：复核现有 runtime cancel、generation 和终态。取消后停止新派发；对已发送动作保留未知结果。只有已验证支持且明确归属的后端才能调用受限 cleanup；cleanup 有独立短时限，不隐式执行普通业务工具，不关闭他人浏览器或删除用户 profile。不新增重叠 turn-complete hook。
- **并发**：隔离浏览器会话优先每会话资源所有权；真实共享桌面才需要宿主级互斥及进程退出释放。不能用单进程 Mutex 冒充跨进程互斥。
- **展示**：从现有工具事件投影操作、失败和图片提示，保留时间顺序及展开详情；不另建日志权威，不把 base64 写到终端，不按任意服务名推断其权限等级。

## 后续平台与入口

Windows 原生桌面另选后端并验收应用定位、权限、坐标映射和部分失败；不能仅放开 macOS 平台判断。Linux 原生桌面需分别验证 X11/Wayland。HarmonyOS PC 未选后端。

daemon、Web、ACP 若调用 MCP，控制对象始终是后端所在机器，不自动是浏览器客户端的电脑。后续接入必须清楚显示执行宿主，验证审批回传、断连和取消。远程控制用户本机需要单独认证的本地代理，第一阶段不提供。

## 体积与发布

Git 仅纳入源代码、技能、文档和小型测试数据；后端 npm 包、浏览器缓存、profile、截图及虚拟机镜像均不进仓库。安装包不携带这些外部组件，安装不触发隐式下载。

实施完成后测量源码 diff 字节、同一 profile/target 下主二进制及发布包大小；用户按需安装的磁盘占用单独报告。目前不提供未经测量的具体增量或承诺“零增长”。第一阶段单独交付，不把后续平台、权限增强和远程入口列为已完成。
