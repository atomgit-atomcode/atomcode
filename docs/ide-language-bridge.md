# IDE 语言服务桥接（只读试点）

这条通道复用当前 IDE 窗口里的语言服务和未保存的编辑器内容。扩展只处理查询，现有 AtomCode runtime 仍负责 agent 和工具执行；不需要配置第三方 MCP、注入插件或再启动语言服务器。

## 启用

1. 安装包含本功能的第一方 VS Code 扩展，打开并信任项目，等待对应语言扩展就绪。
2. 执行命令 **AtomCode: 启用 IDE 语言服务桥接（复制配置）**（英文：Enable IDE Language Bridge）。这会为当前窗口开启随机本机端口，并把配置片段复制到剪贴板。
3. 将片段合并到现有 config.toml 的 `[lsp]` 表中，避免重复创建表。例如：

   ```toml
   [lsp]
   ide_connection_file = "/absolute/private/path/window-id.json"
   ```

   使用实际复制出的路径。文件包含凭据，不要将其内容发给模型、提交到 Git 或共享。配置里只有路径；token 不进入运行时行配置或工具描述。
4. 重新打开 AtomCode 会话以加载配置。`lsp` 会默认使用绑定的 IDE 窗口。本地语言服务器仍受原来的 `enabled` 和 `auto_detect` 开关控制；绑定 IDE 不会开启它们。

停止命令为 **AtomCode: 停用 IDE 语言服务桥接**。窗口关闭、扩展停用或工作区文件夹发生变化时连接文件会清理。每次重新启用会产生新绑定，必要时重新复制配置；异常退出遗留的绑定只会报告不可用，不会自动选择其他窗口。

## 查询

```json
{"operation":"hover","file_path":"src/main.cj","line":138,"symbol":"文档1"}
{"operation":"inspect","file_path":"src/main.cj","line":138,"symbol":"文档1"}
{"operation":"references","file_path":"src/main.cj","line":138,"symbol":"文档1","occurrence":2}
{"operation":"diagnostics","file_path":"src/main.cj","severity":"all"}
{"operation":"document_symbols","file_path":"src/main.cj"}
```

支持 hover、definition、references、diagnostics、document_symbols；inspect 在一次 HTTP 请求内组合 hover、definition、references。位置使用一基行号和 UTF-16 列号，也可用 `line + symbol`，同一行多次出现必须加一基 `occurrence`，不会自动猜第一个。`symbol` 按字面子串匹配，查找来源是编辑器缓冲区。

`backend` 可指定 `ide`、`local` 或 `auto`（默认）。auto 在配置了 IDE 绑定时使用 IDE，失败时明确报告，不静默降级。`local` 要求 `[lsp] enabled = true`，仅支持已有四项操作且需要数值列号；若语言服务器不可用，可显式改用 AST/文本工具。文本引用不等价于语义引用。IDE 请求 `timeout_secs` 默认 20 秒，可设为 1–120 的整数。

## 结果与边界

- 结果携带 `source=ide`、窗口标识、文档版本、语言、未保存状态；查询期间版本变化会返回 `stale_document`，不采用过期查询结果。
- 语言服务能否支持某项查询无法总是通过 VS Code 公共 API 枚举，因此能力允许 `unknown`。空结果是 `empty`，不能据此认定“不支持”或“符号不存在”；provider 报错另标 `provider_error`。
- 诊断来自 Problems 集合，标为 `editor_snapshot_unverified`：公共 API 不提供可靠的对应 LSP 文档版本/分析完成信号。它补充构建，不证明编译通过，也不会等同于“整个项目无错误”。默认仅返回 error，`all` 包含 warning/information/hint。
- 返回值限量；hover 文本至多 12000 字符，位置/诊断至多 200 项，符号树也有总节点/深度限制，整个响应至多 1 MiB。超限会标记裁剪（`omitted_at_least` 为至少省略项数）或返回 `response_too_large`。
- 仅绑定当前窗口，拒绝跨当前工作目录的请求、浏览器 Origin 请求和未鉴权请求。没有 rename、format、codeAction、任意命令或落盘接口。查询不会保存文件。
- VS Code provider 命令无法通过公共 executeCommand 接口强制取消；客户端取消/超时后不会采用迟到结果，服务端会限制尚未结束的查询数量。停用桥接可停止接收新请求；持续卡住的 provider 应重启相应语言服务或 IDE。
- 本试点需要 IDE 扩展与 AtomCode 处于同一主机/容器/远端环境，并能访问相同连接文件与回环网络。扩展声明为 workspace 扩展；完整 WSL/Remote/devcontainer 产品化、自动多窗口发现留到后续。仓颉、CodeArts Doer、真实 Windows 兼容性需实机验收。

## 线协议 v1

私有连接文件字段为 `protocol_version`（1）、`window_id`、`ide`、`endpoint`（随机端口 `http://127.0.0.1:<port>/query`）、`token`（32 字节随机数的 hex）和 `workspace_roots`。Unix 文件权限为 0600。文件不能是符号链接；客户端不接受重定向、代理、非指定回环地址、URL 凭据或 query 参数。

请求为带 `Authorization: Bearer <token>` 的 JSON POST，包含工具参数及客户端校验后的绝对 `file_path`、`workspace_root`。扩展再次校验工作区。响应 JSON 带 `protocol_version=1`、`window_id`、`source=ide`、`status`，成功时还带 `document`、`capabilities` 和逐项 `results`。认证失败为 HTTP 401，操作结果/超时/无结果在 JSON 状态里区分；读写两端均有大小和时间限制。
