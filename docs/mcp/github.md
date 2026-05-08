# GitHub MCP OAuth 使用说明

本文说明如何在 AtomCode 中配置 GitHub Remote MCP，并通过 OAuth 登录后使用 GitHub MCP tools。

## 1. 前置条件

- 已构建或安装 `atomcode`。
- 有可访问 GitHub 的浏览器环境。
- GitHub 账号具备访问目标仓库/组织的权限。

## 2. 创建 GitHub OAuth App

打开 GitHub Developer settings：

```text
https://github.com/settings/developers
```

创建一个 OAuth App：

1. 进入 `OAuth Apps`。
2. 点击 `New OAuth App` / `Register a new application`。
3. 填写：
   - `Application name`：例如 `AtomCode MCP Local`
   - `Homepage URL`：`http://127.0.0.1`
   - `Authorization callback URL`：`http://127.0.0.1/callback`
4. 创建完成后，复制 `Client ID`。
5. 生成并复制 `Client secret`。

> AtomCode 登录时会使用本地随机端口，例如 `http://127.0.0.1:<random-port>/callback`。GitHub 对 loopback redirect URL 支持随机端口，OAuth App 中 callback 写 `http://127.0.0.1/callback` 即可。

## 3. 配置环境变量

不要把 secret 写进仓库文件。建议只通过环境变量提供：

```bash
export ATOMCODE_GITHUB_MCP_CLIENT_ID="<your-github-oauth-client-id>"
export GITHUB_MCP_CLIENT_SECRET="<your-github-oauth-client-secret>"
```

如果使用 shell profile 持久化，也不要提交包含 secret 的文件。

## 4. 添加 GitHub MCP 配置

项目级配置：

```bash
atomcode mcp add-github-oauth github
```

或全局配置：

```bash
atomcode mcp add-github-oauth github --global
```

也可以手写 `.mcp.json` 或 `~/.atomcode/mcp.json`：

```json
{
  "mcpServers": {
    "github": {
      "url": "https://api.githubcopilot.com/mcp/",
      "auth": {
        "type": "oauth",
        "provider": "github",
        "client_secret_env": "GITHUB_MCP_CLIENT_SECRET"
      },
      "timeout_ms": 60000
    }
  }
}
```

说明：

- `client_secret_env` 是环境变量名，不是 secret 本身。
- `timeout_ms` 建议给 GitHub Remote MCP 设置为 `60000`，避免首次 `tools/list` 较慢时被 TUI 外层超时取消。

## 5. OAuth 登录

执行：

```bash
atomcode mcp login github \
  --client-id "$ATOMCODE_GITHUB_MCP_CLIENT_ID" \
  --client-secret-env GITHUB_MCP_CLIENT_SECRET
```

如果配置中已经写了 `client_secret_env`，也可以：

```bash
atomcode mcp login github --client-id "$ATOMCODE_GITHUB_MCP_CLIENT_ID"
```

登录过程：

1. 终端会打印 GitHub 授权 URL，并尝试打开浏览器。
2. 在浏览器中授权 OAuth App。
3. GitHub 回调到本地 `127.0.0.1:<random-port>/callback`。
4. AtomCode 保存 token 到本机配置目录下的 `mcp_auth.toml`。

可以检查 token 是否保存：

```bash
cat ~/.atomcode/mcp_auth.toml
```

不要把该文件内容贴到日志、issue 或 PR 中。

## 6. 在 AtomCode TUI 中验证

启动 AtomCode：

```bash
atomcode
```

在 TUI 中执行：

```text
/mcp reload
/mcp
/mcp tools github
```

预期结果：

- `/mcp reload` 显示 GitHub MCP server 正在连接，随后连接成功。
- `/mcp` 能看到 `github` 状态为 connected。
- `/mcp tools github` 能列出 `mcp__github__...` 工具，例如 issue、pull request、repository、file 相关工具。

首次连接时 GitHub MCP 的 `tools/list` 可能较慢。如果设置了 `timeout_ms: 60000`，AtomCode 的外层 tools/list 超时会按该配置加少量余量等待。

## 7. 调用 GitHub MCP

可以让模型执行只读任务，例如：

```text
用 GitHub MCP 获取当前登录用户信息，只读操作。
```

或：

```text
用 GitHub MCP 列出我能访问的仓库，先只读，不要做任何修改。
```

当模型调用 MCP 工具时，AtomCode 会展示工具审批。确认参数无误后批准执行。

## 8. 常见问题

### MCP server not found in config

说明还没有写入 `github` 这个 MCP server 配置。先执行：

```bash
atomcode mcp add-github-oauth github
```

或写入全局配置：

```bash
atomcode mcp add-github-oauth github --global
```

### missing field `access_token`

通常是 GitHub token endpoint 返回了错误 JSON。常见原因是没有传 `client_secret`。

确认已设置：

```bash
echo "$ATOMCODE_GITHUB_MCP_CLIENT_ID"
echo "$GITHUB_MCP_CLIENT_SECRET"
```

并使用：

```bash
atomcode mcp login github \
  --client-id "$ATOMCODE_GITHUB_MCP_CLIENT_ID" \
  --client-secret-env GITHUB_MCP_CLIENT_SECRET
```

### tools/list timed out

GitHub Remote MCP 首次 `tools/list` 可能较慢。建议在 MCP 配置里设置：

```json
"timeout_ms": 60000
```

然后在 TUI 中执行：

```text
/mcp reload
/mcp tools github
```

### 已登录但连接失败

可以删除 GitHub 的 MCP token 后重新登录：

```bash
atomcode mcp logout github
atomcode mcp login github \
  --client-id "$ATOMCODE_GITHUB_MCP_CLIENT_ID" \
  --client-secret-env GITHUB_MCP_CLIENT_SECRET
```

也可以检查配置文件：

```bash
cat .mcp.json
cat ~/.atomcode/mcp.json
```

项目级 `.mcp.json` 会覆盖同名的全局 `~/.atomcode/mcp.json` server。

## 9. 安全注意事项

- 不要提交 GitHub OAuth `Client secret`。
- 不要提交 `~/.atomcode/mcp_auth.toml`。
- 对写操作工具保持审批，例如创建分支、改文件、发评论、创建 PR 等。
- 需要最小权限时，应在 GitHub OAuth App 和组织策略中限制授权范围。
