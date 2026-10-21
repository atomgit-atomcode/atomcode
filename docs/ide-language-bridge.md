# IDE 语言服务桥接（只读试点）

这条通道复用当前 IDE 窗口里的语言服务和未保存的编辑器内容。扩展只处理查询，现有 AtomCode runtime 仍负责 agent 和工具执行；不需要配置第三方 MCP、注入插件或再启动语言服务器。

## 启用

1. 安装包含本功能的第一方 VS Code 扩展，打开并信任项目，等待对应语言扩展就绪。
2. 执行命令 **AtomCode: 启用 IDE 语言服务桥接（复制配置）**（英文：Enable IDE Language Bridge）。这会为当前窗口开启随机本机端口，并把配置片段复制到剪贴板。
3. 将片段合并到现有 config.toml 的 `[lsp]` 表中，避免重复创建表。例如：

   ```toml
   [lsp]
   ide_discovery_dir = "/absolute/private/path/ide-bridge"
   ```

   使用实际复制出的目录路径。扩展在宿主用户目录（`ATOMCODE_HOME`，否则 `~/.atomcode`）下的 `ide-bridge/` 发布每窗口私有文件；目录配置只需设置一次，窗口重新启用后仍适用。文件包含凭据，不要将其内容发给模型、提交到 Git 或共享。配置里只有目录或文件路径；token 不进入运行时行配置或工具描述。
4. 重新打开 AtomCode 会话以加载配置。`lsp` 会默认发现在线且包含当前工作目录的 IDE 窗口，只有唯一匹配才会自动连接。本地语言服务器仍受原来的 `enabled` 和 `auto_detect` 开关控制；绑定 IDE 不会开启它们。

停止命令为 **AtomCode: 停用 IDE 语言服务桥接**。窗口关闭、扩展停用或工作区文件夹发生变化时连接文件会清理。每次重新启用会产生新窗口标识，但发现目录保持不变。异常退出遗留的离线条目不会参与自动选择；不会自动删除条目。连接目录限 32 个 JSON 绑定、128 个目录条目，超限时提示清理旧文件或显式绑定。

## 状态与窗口选择

- 扩展状态栏显示桥接未启用、启动中、就绪或服务断开；点击可启用、停用或复制当前窗口绑定。“就绪”指桥接服务在线，不保证语言服务已完成索引。
- Agent 调用 `lsp` 的 `{"operation":"status"}`，无需文件路径或启动语言服务，可查看窗口 ID、IDE、工作区匹配和在线状态。多匹配返回 `ambiguous_window`，随后在查询中传 `window_id`；不按端口、启动顺序或“最新窗口”猜测。
- 希望长期固定窗口时，执行 **AtomCode: 复制当前 IDE 窗口绑定**，将其 `ide_connection_file` 配置合并到 `[lsp]`。显式文件优先于目录；失效时不会改连发现目录中的其他窗口。再次启用该窗口后需复制新绑定。要恢复自动发现，删除旧的 `ide_connection_file` 配置。
- 未配置或目录不存在为 `not_enabled`；有效绑定无在线匹配为 `disconnected`；工作区不匹配为 `wrong_workspace`；指定窗口不可用为 `window_unavailable`；私有文件或目录不合法为 `invalid_binding`。根据返回提示启用对应窗口、更新配置或检查工作区，避免盲目重试。
- 发现通过鉴权 `/status` 探测，单次最多 800ms、最多 8 路并发，整体受工具 1–120 秒期限及取消约束。状态探测不进入编辑器 provider，也不占用被卡住查询的槽位。查询超时仍返回 `timeout`。
- 旧版显式连接文件继续支持查询；若旧扩展没有 `/status`，状态探测会显示断开，需升级并重新启用扩展。旧显式文件仍优先，不会默默切换窗口。

## 查询

```json
{"operation":"status"}
{"operation":"hover","window_id":"status 返回的窗口 ID","file_path":"src/main.cj","line":138,"symbol":"文档1"}
{"operation":"hover","file_path":"src/main.cj","line":138,"symbol":"文档1"}
{"operation":"inspect","file_path":"src/main.cj","line":138,"symbol":"文档1"}
{"operation":"references","file_path":"src/main.cj","line":138,"symbol":"文档1","occurrence":2}
{"operation":"diagnostics","file_path":"src/main.cj","severity":"all"}
{"operation":"document_symbols","file_path":"src/main.cj"}
```

支持 hover、definition、references、diagnostics、document_symbols；inspect 在一次 HTTP 请求内组合 hover、definition、references。位置使用一基行号和 UTF-16 列号，也可用 `line + symbol`，同一行多次出现必须加一基 `occurrence`，不会自动猜第一个。`symbol` 按字面子串匹配，查找来源是编辑器缓冲区。

`backend` 可指定 `ide`、`local` 或 `auto`（默认）。auto 在配置了 IDE 目录或显式绑定时使用 IDE，失败时明确报告，不静默降级。`local` 要求 `[lsp] enabled = true`，仅支持已有四项操作且需要数值列号；若语言服务器不可用，可显式改用 AST/文本工具。文本引用不等价于语义引用。IDE 请求 `timeout_secs` 默认 20 秒，可设为 1–120 的整数。

## 结果与边界

- 结果携带 `source=ide`、窗口标识、文档版本、语言、未保存状态；查询期间版本变化会返回 `stale_document`，不采用过期查询结果。
- 语言服务能否支持某项查询无法总是通过 VS Code 公共 API 枚举，因此能力允许 `unknown`。空结果是 `empty`，不能据此认定“不支持”或“符号不存在”；provider 报错另标 `provider_error`。
- 诊断来自 Problems 集合，标为 `editor_snapshot_unverified`：公共 API 不提供可靠的对应 LSP 文档版本/分析完成信号。它补充构建，不证明编译通过，也不会等同于“整个项目无错误”。默认仅返回 error，`all` 包含 warning/information/hint。
- 返回值限量；hover 文本至多 12000 字符，位置/诊断至多 200 项，符号树也有总节点/深度限制，整个响应至多 1 MiB。超限会标记裁剪（`omitted_at_least` 为至少省略项数）或返回 `response_too_large`。
- 每个桥接只处理自身窗口，客户端按当前工作目录选择；拒绝跨当前工作目录的请求、浏览器 Origin 请求和未鉴权请求。没有 rename、format、codeAction、任意命令或落盘接口。查询不会保存文件。
- VS Code provider 命令无法通过公共 executeCommand 接口强制取消；客户端取消/超时后不会采用迟到结果，服务端会限制尚未结束的查询数量。停用桥接可停止接收新请求；持续卡住的 provider 应重启相应语言服务或 IDE。
- 本试点需要 IDE 扩展与 AtomCode 处于同一主机/容器/远端环境，并能访问相同连接文件与回环网络。扩展声明为 workspace 扩展；跨主机发现和完整 WSL/Remote/devcontainer 产品化留到后续。仓颉、CodeArts Doer、真实 Windows 兼容性需实机验收。

## 线协议 v1

私有连接文件字段为 `protocol_version`（1）、`window_id`、`ide`、`endpoint`（随机端口 `http://127.0.0.1:<port>/query`）、`token`（32 字节随机数的 hex）和 `workspace_roots`。Unix 文件权限为 0600。文件不能是符号链接；客户端不接受重定向、代理、非指定回环地址、URL 凭据或 query 参数。

状态探测为同端口 `POST /status`，同样要求 Bearer token 并拒绝 Origin；返回协议、窗口、来源和 `ready` 状态，不调用语言服务。查询请求为带 `Authorization: Bearer <token>` 的 JSON POST，包含工具参数及客户端校验后的绝对 `file_path`、`workspace_root`。扩展再次校验工作区。响应 JSON 带 `protocol_version=1`、`window_id`、`source=ide`、`status`，成功时还带 `document`、`capabilities` 和逐项 `results`。认证失败为 HTTP 401，操作结果/超时/无结果在 JSON 状态里区分；读写两端均有大小和时间限制。

## 实机验收清单（尚需目标环境验证）

1. Windows 下打开带中文和空格路径的仓颉项目，启用桥接，配置复制出的目录；`status` 显示唯一 ready，hover/definition/references 与 IDE 操作一致。
2. 打开同一项目的第二个窗口（可包含 CodeArts），两侧启用桥接；无 window_id 时明确消歧，指定两侧 ID 后读取各自未保存的内容。
3. 关闭窗口或停止桥接后，旧窗口不得再被选中；异常退出遗留条目标记离线，重新启用可用新 ID 查询。显式旧绑定必须报错，不能改连其他窗口。
4. WSL/Remote/devcontainer 将扩展安装在远端 workspace 侧，并让 AtomCode 在同一环境运行；确认双方目录和回环地址相同。Windows 进程不能直接使用 WSL 私有回环连接。
5. 慢查询时取消/超时不采用迟到结果，状态探测仍响应；停用后连接文件清理，原有 grep/AST 和本地 LSP 开关行为保持。
