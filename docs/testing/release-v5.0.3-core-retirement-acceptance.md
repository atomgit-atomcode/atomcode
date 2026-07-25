# release/v5.0.3 Core 收口审计与真机验收清单

> 审计日期：2026-07-25
> 审计分支：`release/v5.0.3`
> 初始审计基线：`9daecd7c541a0b09deb7c4934bbe279d402c8aa2`
> 实施基线：`4900677ff40cf214ec3aa5fa7a9af7e92512f769`

## 1. 结论

core 中的 conversation、provider、tool、MCP、plugin、skill、LSP、vision 等生产能力已经迁出，CLI、TUI、daemon、ACP 和 clix 的 coding agent 路径均已切到 `CodingRuntime` / kernel native stack。

本次收尾已完成物理退役：

- workspace metadata、default members 和 lockfile 中已不存在 `atomcode-core`；
- CLI、TUI、daemon 已删除对 `atomcode-core` 的直接依赖；
- daemon legacy importer fixture 已迁至 daemon，行为 fixture/invariant tests 已迁至 capabilities/config；
- proprietary release feature 改由最终 CLI 包的 `atomcode/codingplan-crypto` 持有；
- 无生产代码引用 `atomcode_core::`，历史说明和迁移注释不构成可达依赖。

按四态判定：

- [x] 新 owner 已实现对应能力
- [x] 生产消费者已切换
- [x] 未发现 core runtime fallback
- [x] core crate、依赖、fixture 和发布 feature surface 已删除

因此代码结构已达到“`atomcode-core` 完全退役”。本文未勾选的真机项目仍是 release 验收门槛，不应由本地编译或单测替代。

## 2. 建议修复范围

### 2.1 P0：正式发布前必须修复

#### A. 迁移 daemon legacy importer fixture

将：

```text
crates/atomcode-core/tests/fixtures/session/legacy_full.json
crates/atomcode-core/tests/fixtures/session/legacy_minimal.json
```

迁移至：

```text
crates/atomcode-daemon/tests/fixtures/session/legacy_full.json
crates/atomcode-daemon/tests/fixtures/session/legacy_minimal.json
```

同步修改：

- `crates/atomcode-daemon/src/legacy_convert.rs`
- `crates/atomcode-daemon/src/lib.rs`
- `crates/atomcode-daemon/tests/legacy_turn_boundary_repair.rs`

验收条件：

- [x] daemon 不再从 core 目录读取或 `include_bytes!` fixture
- [x] legacy importer、repair、resume 测试全部通过
- [x] legacy importer 仍是 daemon 私有冻结 DTO 的单向导入

#### B. 重建 proprietary release feature 入口

当前官方构建通过：

```text
--features atomcode-core/codingplan-crypto
```

间接激活 `atomcode-auth/codingplan-crypto`。删除 core 前必须建立由最终发布二进制拥有的明确 feature，例如：

```toml
# crates/atomcode-cli/Cargo.toml
[features]
codingplan-crypto = ["atomcode-auth/codingplan-crypto"]
```

正式构建改为：

```text
--features atomcode/codingplan-crypto
```

验收条件：

- [x] 开源默认构建继续使用 stub
- [ ] proprietary release 构建包含真实签名实现
- [ ] CodingPlan claim/status 和 AtomGit gateway 请求签名真机通过
- [x] 发布脚本和 workspace 构建入口不再引用 `atomcode-core/codingplan-crypto`

#### C. 物理删除 atomcode-core

删除：

- `crates/atomcode-core/`
- CLI、TUI、daemon 中的 `atomcode-core` 依赖
- workspace `default-members` 中的 `crates/atomcode-core`
- `Cargo.lock` 中仅由 core 引入的依赖

验收条件：

- [x] `rg 'atomcode_core::' crates Cargo.toml` 无生产代码引用
- [x] `cargo metadata` 中不存在 `atomcode-core`
- [x] capabilities、coding、kernel 的依赖方向保持 core-free
- [x] workspace 全 targets 编译通过

### 2.2 P1：建议与 core 删除同批修复

#### D. 迁移 core 中仍有价值的独立测试

将以下历史行为 fixture 和 invariant tests 迁往能力 owner：

```text
session_p0_sprint_clean.jsonl
session_404_recovery.jsonl
```

建议归属：

- prompt/tool-call/result parity：`atomcode-coding` 或 `atomcode-kernel`
- bash exit marker、shell workaround：`atomcode-capabilities`
- unified prompt tests：`atomcode-config` 或 `atomcode-coding`

不要为了删除 crate 直接丢弃仍能防回归的测试。

#### E. 删除失效 build script 和 proxy facade

`atomcode-core/build.rs` 注入的 `ATOMCODE_BUILD_ID` 已无有效消费者；真正 CLI build ID 由 `atomcode-cli/build.rs` 提供。

`atomcode-core::proxy` 同样已无生产调用方。各 HTTP 客户端已经在自己的 owner crate 中应用 proxy/TLS policy，不需要再迁移 core proxy。

验收条件：

- [x] CLI build script 仍独立提供 `ATOMCODE_BUILD_ID`
- [x] core build script 和 proxy facade 已删除
- [ ] proxy/TLS 真机矩阵通过

### 2.3 P2：文档和维护性清理

修正仍声明 core 持有 config、i18n、LSP、plugin、proxy 的失真注释，重点包括：

- `crates/atomcode-config/src/lib.rs`
- `crates/atomcode-config/src/proxy.rs`
- `crates/atomcode-tuix/src/i18n/mod.rs`
- `crates/atomcode-coding/src/parts.rs`
- `crates/atomcode-cli/tests/uninstall_integration.rs`
- `crates/atomcode-tuix/tests/plugin_integration.rs`

验收条件：

- [x] 本次触及的当前架构注释已改为现有 owner
- [x] 历史迁移信息仅作为来源说明，不作为当前实现前提
- [x] 不存在“transition shim”已经删除但注释仍声称存在的情况

## 3. 推荐落地顺序

### 提交 1：解除 core 对 fixture 和发布 feature 的所有权

- 迁移 daemon legacy fixtures
- 迁移 core 中仍有价值的 invariant tests
- 新增 CLI-owned `codingplan-crypto` feature
- 更新发布流水线

验证：

```bash
cargo test -p atomcode-daemon
cargo test -p atomcode-capabilities
cargo test -p atomcode-coding
cargo test -p atomcode
```

### 提交 2：物理删除 core

- 删除三条依赖边
- 删除 workspace member
- 删除 core crate
- 更新 lockfile

验证：

```bash
cargo check --workspace --all-targets
cargo tree -p atomcode-capabilities
cargo tree -p atomcode-kernel
rg 'atomcode_core::|atomcode-core' Cargo.toml crates
```

### 提交 3：注释和文档收口

- 清理失真注释
- 更新架构说明和发布构建说明

验证：

```bash
git diff --check
```

## 4. 真机测试环境

至少准备：

- [ ] macOS 或 Linux 一台
- [ ] Windows 一台，覆盖 SChannel、TLS 1.2 和中文路径
- [ ] 一份真实旧版本 session 数据备份
- [ ] 一个配置了 MCP、plugin、hook、skill 的项目
- [ ] AtomGit CodingPlan 账号
- [ ] 一个外部 OpenAI-compatible provider
- [ ] 一个原生 vision provider 或独立 VL provider

执行测试前：

- [ ] 备份 `~/.atomcode`
- [ ] 记录测试二进制 commit SHA
- [ ] 记录操作系统、终端和网络/代理环境
- [ ] release 构建确认使用预期 crypto feature

## 5. 真机验收清单

### 5.1 基础启动与普通对话

- [ ] 无历史配置首次启动可进入 TUI，不 panic
- [ ] 已登录用户启动后可以直接发送消息
- [ ] 外部 provider 能完成一次流式文本响应
- [ ] AtomGit provider 能完成一次流式文本响应
- [ ] tool call 的开始、参数、结果和完成状态展示正常
- [ ] provider 错误有明确提示，当前 turn 只有一个终态
- [ ] Ctrl+C 中断当前 turn 后可以继续发送下一条消息
- [ ] 运行中 steer 消息进入当前 turn，不被当成新 session

### 5.2 模式、审批与用户问询

- [ ] Build 模式文件修改正常弹出审批
- [ ] Accept Edits 自动批准普通工作区编辑
- [ ] Accept Edits 对敏感路径仍弹出审批
- [ ] Auto / `-y` 模式普通工具无需审批
- [ ] Auto / `-y` 模式 `RequestUserInput` 仍展示问题面板
- [ ] Plan 模式只读工具可用，修改操作仍受安全边界约束
- [ ] “允许一次”“始终允许”“拒绝”均返回正确结果
- [ ] 审批面板期间 Ctrl+C 后 pending request 不悬挂
- [ ] provider reload 后旧审批不能作用于新 generation
- [ ] session switch 后旧问询不能作用于新 session

### 5.3 Session 生命周期与持久化

- [ ] 新 session 退出重启后历史、工具调用和标题完整恢复
- [ ] `/resume` 显示正确项目、标题、时间和摘要
- [ ] resume 后继续对话，旧历史不重复
- [ ] `/fresh` 创建新 session，不覆盖旧 session
- [ ] rename 后重启仍保留新名字
- [ ] delete 删除 snapshot/meta/presentation，列表不再显示
- [ ] `/undo` 后重启仍保持回退结果
- [ ] 同一 session 被两个进程打开时明确报 lease 冲突
- [ ] session switch 失败时保留原 session，不静默 fresh
- [ ] provider reload 后 session id、工作目录和历史不变

### 5.4 Legacy session 单向导入

- [ ] 仅有 legacy JSON 的旧 session 能出现在列表中
- [ ] 首次 resume 完成 native cutover
- [ ] 导入后的历史消息、标题和工具调用完整
- [ ] 第二次 resume 不重复导入、不重复消息
- [ ] 图片或 presentation 内容没有丢失
- [ ] legacy 源在 cutover 后变化时不覆盖 native 数据
- [ ] 损坏 session 只阻断自身，不影响其他 session
- [ ] native snapshot 缺失或损坏时明确失败，不 silent fresh
- [ ] 导入中途杀进程，再启动后能恢复或 fail-closed
- [ ] 已导入 session 后续只写 native 格式，不恢复 legacy writer

### 5.5 Compact 与上下文

- [ ] 长会话执行 `/compact` 成功
- [ ] compact 后仍能回答压缩前的关键上下文
- [ ] compact 后重启恢复，摘要不重复
- [ ] 连续 compact 只有一个有效 summary anchor
- [ ] compact provider 缺失时明确报错且不破坏 session
- [ ] compact 期间 Ctrl+C 不错误覆盖旧 snapshot
- [ ] provider reload/session switch 与 compact 冲突时安全终止
- [ ] `/context`、`/cost`、`/todo` 在 compact 前后结果合理

### 5.6 图片与 VL

- [ ] 纯文本消息不触发 VL provider
- [ ] 原生 vision 模型可接收图片和文字
- [ ] 文本模型配置 VL provider 时先识别再继续对话
- [ ] VL provider 缺失时明确显示识别失败
- [ ] 裸图片不会误发给 text-only provider
- [ ] VL stream 报错后 turn 有唯一明确终态
- [ ] 带图片 session 重启后展示信息仍完整
- [ ] WebUI 上传图片后刷新仍能看到展示内容
- [ ] Windows 中文图片路径可以正常处理

### 5.7 MCP

- [ ] `/mcp` 能列出 stdio 和 HTTP server
- [ ] 未信任的 project stdio server 不会启动
- [ ] trust 后 server 能连接并列出工具
- [ ] untrust 后当前 runtime 撤销对应工具
- [ ] read-only MCP tool 在 Plan 模式可执行
- [ ] destructive MCP tool 正常触发审批
- [ ] auto-approved tool 不扩大为整个 server 授权
- [ ] HTTP MCP OAuth 登录和 token refresh 正常
- [ ] 单个 server 连接失败不阻塞 agent 启动
- [ ] MCP reload 后旧 tool call 不污染新 registry

### 5.8 Plugin、skill 与 hooks

- [ ] `/plugin` 列表、安装、卸载正常
- [ ] plugin skill 在新 session 中可发现、可调用
- [ ] plugin custom command 可以执行
- [ ] 未信任 plugin hook 不执行
- [ ] 信任后 PreToolUse/PostToolUse 按 matcher 执行
- [ ] hook `deny` 能阻止工具
- [ ] hook `ask` 能触发审批
- [ ] hook 启动失败不被误判为主动 deny
- [ ] reload 后旧 hook 不重复注册
- [ ] daemon 和 TUI 使用同一套 plugin skills/hooks

### 5.9 daemon 与 WebUI

- [ ] daemon `/chat` 返回 authoritative stop reason
- [ ] `/live` 文本、reasoning、tool progress 顺序正确
- [ ] WebUI cancel 后可以继续发送消息
- [ ] WebUI approval 与 request id 精确关联
- [ ] session switch 后旧事件不污染新 session
- [ ] provider switch 后显示模型与实际请求一致
- [ ] WebUI `/compact`、`/undo`、`/resume` 与 TUI 结果一致
- [ ] daemon 重启后 native session 正常恢复
- [ ] `/chat` 和 `/live` 不会为同一会话创建第二 runtime owner

### 5.10 ACP、clix 与 headless

- [ ] `atomcode acp` 能创建 session 并完成 prompt
- [ ] ACP 图片 prompt 进入 vision 预处理
- [ ] ACP approval 四种选择映射正确
- [ ] ACP cancel 只取消目标 session
- [ ] ACP provider error 不被误报为成功停止
- [ ] `atomcodex code` 可完成一次读文件和修改文件任务
- [ ] `atomcodex review` 能读取 diff 并输出 findings
- [ ] clix 超时或取消有明确非成功终态
- [ ] headless 遇到必须人工审批的操作时 fail-closed

### 5.11 Proxy、TLS 与发布构建

- [ ] default proxy 模式在无代理网络可登录和聊天
- [ ] system proxy 覆盖登录、CodingPlan、provider、MCP、updater
- [ ] explicit HTTP/HTTPS proxy 重启后生效
- [ ] no_proxy 确实绕过系统代理
- [ ] `ATOMCODE_TLS_MAX=1.2` 对 AtomGit 登录和 provider 生效
- [ ] Windows 默认 SChannel 可以登录和聊天
- [ ] Windows 能访问 `acs.atomgit.com`
- [ ] Windows 能访问 `llm-api.atomgit.com`
- [ ] 自动 TLS fallback 只影响 AtomGit endpoint
- [ ] 外部 provider 不被无条件降级到 TLS 1.2
- [ ] 版本检查和自更新在 proxy/no_proxy 下均正常
- [ ] proprietary release 构建包含真实 CodingPlan crypto

## 6. 自动化验证基线

退役实现已通过：

- [x] `cargo check --workspace --all-targets`
- [x] `atomcode` lib：52 tests
- [x] `atomcode-daemon` lib：202 tests
- [x] `atomcode-tuix` lib：1633 tests
- [x] `atomcode-capabilities` lib：1094 tests
- [x] 迁移后的 session fixture invariants：8 tests
- [x] daemon legacy boundary repair：5 tests
- [x] config unified prompt：3 tests
- [x] `cargo check -p atomcode --features codingplan-crypto`
- [x] kernel、capabilities 不依赖 core
- [x] `git diff --check`
- [ ] `cargo fmt --all -- --check`（workspace 现有未格式化差异；本次新增/迁移文件需保持局部格式检查）
- [ ] proprietary private crypto overlay 的正式产物与请求签名真机验证

`cargo check --workspace --all-targets` 当前有一个与 core 收口无关的 warning：

```text
crates/atomcode-kernel/tests/liveness.rs: unused import SilentStreamProvider
```

## 7. 发布门槛

以下条件全部满足后，才可以把状态更新为“atomcode-core 已完全退役”：

- [x] core fixture 已迁往真实 owner
- [x] proprietary crypto feature 已由发布入口持有
- [x] CLI、TUI、daemon 不再依赖 core
- [x] workspace 不再包含 core
- [x] `crates/atomcode-core` 已删除
- [x] 无 core runtime fallback
- [x] legacy importer 仍是单向、fail-closed、可测试的兼容入口
- [x] workspace 全 targets 编译通过
- [x] 相关 crate 测试通过
- [ ] 本文 P0 真机项全部通过
- [ ] Windows TLS、代理和正式签名构建通过
