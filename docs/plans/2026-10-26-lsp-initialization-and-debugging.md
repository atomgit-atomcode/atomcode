# LSP 初始化配置与调试通道实施计划

**Goal:** 让用户通过独立 JSON 文件配置语言服务，明确区分禁用、能力不支持、查询无结果和连接失败，并可复现正式工具的连接过程。

**Architecture:** 配置层保存声明，coding 负责装配，capabilities 内现有 LspManager/LspClient 负责配置解析后的连接和协议交互。CLI probe 与模型的只读调试操作复用此流程；不创建第二套客户端或 runtime owner。路径及日志目录由宿主通过 ProductDirs 注入。

**Tech Stack:** Rust、serde/TOML/JSON、tokio、现有 stdio JSON-RPC LSP 客户端。

## 勘察基线

- 分支：release/v5.2.5，HEAD：c0cd7dea2。
- 工作区有另一批 WebUI 改动及未跟踪文件，保留，不纳入本功能。
- `atomcode-config/src/lsp_registry.rs` 的服务配置仅有 command/args/root_markers。
- `atomcode-config/src/config/mod.rs` 的全局 enabled、auto_detect 默认均为 false，保持其显式启用边界。
- `atomcode-coding/src/config.rs::lsp_settings_from_config` 负责配置映射。
- `atomcode-capabilities/src/codeintel/mod.rs::register_lsp_tool` 装配本地与 IDE 接入。
- `codeintel/lsp/client.rs` 的 initialize 没有 initializationOptions，workspace/configuration 统一返回 null；仅跟踪诊断能力的动态注册。
- `codeintel/lsp/manager.rs` 的缓存 key 为 root/command/args，新增初始化配置后必须补足连接身份。
- `codeintel/lsp_tool.rs` 当前只有只读查询和 status；没有 rename/format，不能用新增开关暗示已支持。
- 近期相关提交：3818ded3f（只读代码智能）、3e7827ad8（JSON 顺序测试）、1f1fa3e0b（协议头长度限制）。

## 建议配置契约

### 四个本地仓库的对照结论（2026-10-26）

本节为静态源码勘察，未运行四个产品或验证其发行版；只描述这些 checkout。

| 仓库 / HEAD | 已确认内容 | 对 AtomCode 的启示 |
| --- | --- | --- |
| opencode / e5a557efee | `packages/schema/src/config/lsp.ts` 定义 initialization 对象、env、disabled；文档描述初始化选项。当前 checkout 中未检索到初始化握手实现，不能据此确认运行时行为 | 借鉴简短配置，不把文档/schema 当作协议实现证据 |
| codex / 806d9732c9 | 对 codex-rs、docs 检索未发现内置 LSP 初始化、服务配置或协议客户端 | 本功能没有找到可直接借鉴的内置实现；不等于断言外部插件也不支持 |
| oh-my-pi / 4585e9da4a | `packages/coding-agent/src/lsp/config.ts`、types.ts、client.ts、tool.ts 提供独立 lsp.json/yaml、initOptions、settings、disabled、内置服务器、status/reload/capabilities/request；initialize 发送 initOptions，initialized 后推送 settings，按请求 section 回答 workspace/configuration | 最主要的配置、连接身份、重载与诊断参考；其 capabilities 开关主要是 rust-analyzer 扩展，并非本提案的全部通用功能开关 |
| deepseek-harness / 687f282e86 | `packages/lsp/lsp-stdio/src/{index,instance,connection}.ts` 提供静态 initializationOptions/configuration；每工作区按需池化；检查能力；保留受限 stderr；只读查询传输失败可新进程重试一次；拒绝 workspace/applyEdit | 借鉴清晰失败分类、stderr、只读权限与有界恢复；其 configuration 是每项相同的静态回答，不能直接当成按 section 的设置解析器 |

进一步核对到 OMP 的 clientKey 包含 args、initOptions、settings、languageId，reload 会清理失效连接；这证明新配置仅传到 initialize 不够，还必须解决连接复用。其工具审批按只读 action 白名单分类，其余归为 write，包含通用 request；AtomCode 的只读 probe 不应直接暴露任意 RPC，以免绕过写权限。

四份源码没有证明已完整具备本提案的外部初始化文件引用、模板展开、仓颉 cjpm 默认生成、统一 on/auto/off 和全文 trace/probe 组合。这些属于 AtomCode 自身要设计、验证的能力，不能宣称照搬竞品已有方案。

### 对照后的收敛

- 普通用户只需 command/root_markers 与一个 init_options_file；策略、合并、trace 和能力字段均有默认值，示例不应要求全填。
- 配置覆盖默认以整个 initOptions/settings 对象替换，避免隐式深合并；高级 shallow_merge 可延期，不阻塞根因修复。
- 第一交付优先包含 init_options_file/inline、settings、disabled、查询开关、status/probe/trace 以及显式 reload。仓颉自动生成随已验证逻辑交付，不用猜测的内置默认阻塞通用入口。
- 新增 reload：重新读取配置和初始化文件，关闭身份变化的旧连接，清理对应失败缓存；初始化期间的旧连接不能在 reload 后迟到发布。只修改文件而未 reload 时，status 必须明确当前连接使用的配置身份。
- 借鉴 DSH 的有界恢复：仅传输故障的幂等只读查询允许重建后重试一次，仍受原始截止时间与取消约束；坏 JSON、配置禁用、协议错误和 initialize 被拒不循环重试。是否启用重试需测试证明不破坏当前同步/取消流程。
- 写操作与通用 request 暂不加入只读工具；未来提供时单独经过写入审批，不能仅靠 method 字符串或功能开关判定安全。

以下是待实现配置，不是当前已支持的配置。全局开关继续使用现有 bool；新增策略使用 on/auto/off，避免破坏已有 TOML。

```toml
[lsp]
enabled = true
builtin_init_options = "auto"
trace = "off"

[lsp.features]
definition = "auto"
hover = "auto"
references = "auto"
diagnostics = "auto"
document_symbols = "auto"

[lsp.servers.cj]
command = "C:/CangJie_HOME/cangjie/tools/bin/LSPServer.exe"
args = []
root_markers = ["cjpm.toml"]
disabled = false
init_options_mode = "auto"
init_options_file = "~/.atomcode/LSP_config/cangjie.json"
# 另一种写法：init_options = { ... }，与文件互斥。
# 默认用户配置整体替换内置配置；需要时显式选择 shallow_merge。
init_options_merge = "replace"

[lsp.servers.cj.features]
diagnostics = "off"
```

用户目录结构建议：`LSP_config/cangjie.json`、`LSP_config/rust-analyzer.json` 等。目录名只在宿主配置约定处定义，库接收实际目录。不要为每种语言安装含无效路径的活动配置；提供按需生成的样例。

仓颉文件可表达用户已验证的字段：

```json
{
  "multiModuleOption": {
    "${workspaceUri}": {
      "name": "jian_lang_server",
      "src_path": "${workspaceUri}/src"
    }
  },
  "conditionCompileOption": {},
  "singleConditionCompileOption": {},
  "conditionCompilePaths": [],
  "targetLib": "${workspaceFolder}/.cache/lsp"
}
```

此例只说明单包项目配置。内置生成器需取得并核验官方插件/用户验证过的逻辑，覆盖 workspace、多模块、src-dir、条件编译和 SDK 差异后再默认启用；不能把单包样例推广成所有仓颉工程的项目模型。

### 解析与失败规则

1. init_options_mode 与 init_options 分开命名，避免一个字段同时表示策略字符串和 JSON 内容。
2. off 完全不发送 initializationOptions，也不读取对应文件或执行生成逻辑。
3. auto 在配置文件存在或匹配已支持的内置工程探测时使用；显式配置的文件不存在可跳过并说明原因，但文件存在却不可读、JSON 错误或变量缺失必须报错。
4. on 要求能够得到有效初始化数据，否则在启动服务前明确失败。
5. 用户 file/inline 是互斥来源；默认 replace。shallow_merge 仅允许对象顶层合并，用户字段覆盖内置字段，不隐式深合并数组或嵌套对象。
6. builtin_init_options 控制内置生成器，不能盖过服务级 off。一次性宿主环境覆盖优先级最高，只关闭本次初始化，不修改配置文件。
7. `${workspaceFolder}` 是本机路径，`${workspaceUri}` 是正确编码的 file URI，`${fileDir}`/`${fileDirUri}` 同理。`${HOME}` 与 `${env:VAR}` 为显式替换；未定义时报错。JSON 对象键和字符串值均支持替换，非字符串值保持类型。
8. 相对配置文件路径按声明配置文件所在目录解析，不能随 cwd 改变；JSON 内相对项目路径按语言服务契约处理，不做猜测式递归转换。
9. 模板依赖 fileDir 时，同一根目录内的不同文件可能得到不同初始化消息。缓存必须按解析结果身份区分，不能复用错误连接。
10. init 失败保留明确的阶段、退出状态、受限 stderr 和 trace 位置；不能静默当作查询无结果。

### 能力规则

- 全局 features 是默认策略，服务级字段覆盖；disabled=true 不启动该服务。
- off 明确返回“该能力已被配置禁用”，不发对应 RPC。
- auto 依据静态与动态注册能力；未声明支持时说明不支持，不能与空查询结果混淆。
- on 允许显式尝试声明不完整的服务，MethodNotFound 必须说明服务端不支持。
- inspect 这种复合操作只执行允许的子能力，并分别解释被跳过的部分。
- IDE 与 local backend 均应用用户能力限制，不能通过自动 fallback 绕过 off。
- diagnostics=off 同时关闭主动拉取、结果暴露及相应本地缓存；推送消息仍正常消费，避免阻塞协议流。
- 当前只读工具不增加无效 rename/format 开关。写操作作为独立后续功能，默认 off，并经过审批、工作区路径验证与可恢复的文件修改流程。

### 工作区配置与脚本

初始化参数与 workspace/configuration 是不同协议入口。提供独立 settings/settings_file 来源，并按服务端请求的 section 返回正确子树；不能把所有语言的设置都塞入 initializationOptions。首期不替所有语言生成未经验证的默认配置。

init_options_command 暂不进入首期：它是额外命令执行入口。后续若加入，使用 argv 数组而非默认 shell，要求显式启用，并限制时间、输出、取消、退出码与项目配置信任。只读 probe 不应隐式执行项目提供的初始化脚本。

## Task 1：配置与解析

**Files:** 修改 config 的 lsp_registry.rs/config/mod.rs、coding/src/config.rs、capabilities/src/codeintel/mod.rs 与 lsp/registry.rs；新增 lsp/options.rs。

1. 为策略枚举、旧配置兼容、来源互斥及非法组合添加配置测试。
2. 实现文件/内联解析、变量替换、replace/shallow_merge、内置策略选择，返回解析来源与判定原因。
3. 添加 Windows URI 编码、中文/空格目录、对象键替换、缺失环境变量、auto 缺失/on 缺失与坏 JSON 测试。
4. resolved options 只进入持有连接的实例，不进入可打印行配置或 describe_self；测试环境值不会泄漏到配置树。

## Task 2：正式连接与能力控制

**Files:** 修改 capabilities/src/codeintel/lsp/{manager,client,registry}.rs 与 lsp_tool.rs。

1. 用 duplex mock 服务断言 initialize 的最终 JSON；off 时必须完全没有初始化字段。
2. 增加真实 workspaceFolders、初始化参数、工作区设置应答及必要的能力注册/注销跟踪。
3. 补齐客户端缓存身份与失败缓存身份，配置变更后不能沿用旧 options 或旧失败。
4. 在 IDE/local/inspect 分派处统一执行功能策略；测试禁用时无 RPC，unsupported 与空结果不同。
5. 用虚拟时间覆盖 initialize 超时、退出、取消与 pending 清理；保持启动锁和单个连接 owner。

## Task 3：可诊断的协议轨迹

**Files:** 新增 capabilities/src/codeintel/lsp/trace.rs；修改 client.rs/manager.rs 及宿主日志装配调用点。

1. off 不记录协议正文；messages 记录方法、id、耗时、错误、配置来源、能力及脱敏初始化参数；verbose 按显式启用记录更多请求响应。
2. 显式标注脱敏字段，保留结构用于核查；环境替换可能包含凭据，不承诺任意正文绝无秘密。不能把原始正文混入默认日志。
3. 通过宿主注入 logs/lsp.log 目标，限制日志大小与 stderr 缓冲，避免服务端输出阻塞进程。
4. 测试 EOF、畸形协议、坏初始化后退出时能定位到握手阶段；日志不能污染 stdio JSON-RPC。

## Task 4：复用连接流程的自调试入口

**Files:** 修改 CLI 现有参数/子命令声明和分派文件（实施时先定位）；修改 lsp_tool.rs 的只读操作 schema。

1. 提供 `atomcode lsp probe --file src/main.cj --line 13 --col 26 --method definition`，说明一基位置及列编码约定。
2. 提供模型可调用的只读 probe/status 结果：根目录、初始化来源/策略、服务版本和能力、阶段耗时、结果/失败原因及日志位置。
3. CLI 与工具复用同一解析器和正式 LspManager/LspClient；不编写独立示例客户端作为产品实现。
4. 宿主解析一次性环境覆盖；如加入 CLI flag，各 CLI/TUI/daemon 启动路径统一透传。已有连接需要明确重建，不能假称运行中连接已应用初始化变化。
5. 添加 CLI 参数、路径错误、服务不可执行和权限边界测试；自调试不擅自改配置、安装服务或执行写操作。

## Task 5：仓颉默认生成与交付

1. 对照已验证的生成逻辑实现 cjpm.toml 单包与 workspace 测试夹具，明确 SDK 兼容范围。
2. 缺乏完整项目模型时返回可执行的手动配置说明，不生成看似成功但错误的模型。
3. 更新用户文档、配置样例和实际实现处的 self-knowledge；不向模型描述尚未实现的能力。
4. 运行受影响 crate 的 `cargo nextest run -p <crate>`；LSP feature 使用该 crate 当前 feature 名显式覆盖。
5. 运行 `cargo fmt --all`、`cargo fmt --all -- --check`、`bash gates/compile.sh`；审查无关格式变更。
6. Windows + Cangjie SDK 1.1.3 人工验收跨包 hover/definition/references；无法实测时明确标记未验证，不以 mock 通过替代。
7. 提交前检查 Cargo.lock 的 hkdf/hmac/zeroize_derive 均为零，仅提交本功能文件。

## 首期边界

首期覆盖配置文件/内联、独立模式开关、查询能力控制、trace/probe 与可验证的仓颉默认生成。工作区设置与初始化参数应保持独立。初始化命令、重命名和格式化需要额外执行/写入边界，单独交付；不以一次大改让只读工具悄然获得写权限。

## 本轮交付记录

- 已实现通用 init_options_file/inline、settings_file/inline、模板展开、服务与能力开关、messages/verbose trace、只读 probe/status/reload，以及 CLI probe 和一次性关闭初始化。使用说明见 `docs/lsp/configuration.md`。
- 初始化 JSON 和 settings 参与连接身份；reload 清理连接与失败缓存，并使等待中的请求显式结束。TOML 的命令和策略仍由 runtime 装配读取，reload 不宣称重读整个 config.toml。
- inspect 包含禁用子能力时整体明确拒绝，建议分别调用允许的子能力；本轮没有为 IDE 新建复合请求拆分流程。
- 仓颉提供反馈中字段的手动单包 JSON 示例；自动项目模型生成尚未实现，等待经过 SDK / 多模块验证的生成逻辑。builtin_init_options、合并策略、初始化命令、任意 RPC 和写操作未作为空开关上线。
- 受影响三个 crate 完整 nextest：3582 passed、10 skipped；CLI 参数测试通过；扩充后的 LSP 凭据不进入配置树/describe_self 测试单独通过。最终编译门与 rustfmt 检查通过。
- 实际构建 CLI + 临时模拟服务冒烟通过：正常初始化得到跨包定义；一次性关闭初始化后无该结果；坏 JSON 在启动进程前失败；环境替换值不会进入 trace；自定义 --config / --dir 得到正确配置和项目根。
- 上述模拟仅验证产品到协议的完整链路，不代表已在 Windows / Cangjie SDK 1.1.3 上验收。没有修改其他并行 WebUI 工作。

## 提交前审计

- 修复 IDE 查询通过不同扩展名的符号链接绕过服务级功能限制的问题；策略按实际目标语言判断，并补充回归测试。
- 初始化与 settings 中的凭据值加入运行时脱敏列表，服务端将其回显到普通错误文本时仍会被遮盖；这些值仍不进入行配置或 describe_self。
- trace 写入前检查实际文件长度，避免多个已打开连接各用独立计数而越过累计日志预算。
- 审计后 capabilities（含 lsp）完整测试 1263 passed；48 项 LSP 专项测试通过；编译门、rustfmt、diff 和 Cargo.lock 私有依赖检查通过。其他 crate 的完整测试与 CLI 冒烟沿用上面的已通过基线。
