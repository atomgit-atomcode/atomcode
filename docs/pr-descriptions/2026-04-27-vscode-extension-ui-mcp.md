## 概述

VS Code 扩展完整实现：从零搭建 React Webview UI 并全面对标 Claude Code 风格，同时将 MCP 工具集成到 daemon 后端。

**UI 重做（14 commits）：**
- 建立 `--app-*` CSS 变量体系，品牌色 `#7c3aed`（紫色），映射到 VS Code 主题变量实现自动 dark/light 适配
- Timeline 消息布局：左侧圆点状态指示（紫色=用户/进行中，绿=完成，红=错误）+ 竖线连接
- 工具调用改为 Grid IN/OUT 终端风格，渐变 mask 截断长内容，注解标签（applied/error/耗时）
- 替换自实现 Markdown 渲染器为 `marked` + `highlight.js` + `dompurify`，支持语法高亮和 XSS 防护
- 输入框紫色聚焦光环（`color-mix` box-shadow），浮动定位，附件 Pill 标签
- 新增 PermissionRequest 组件（Allow/Deny + destructive 警告）
- 新增 DiffView 组件（双列行号 unified diff），集成到 Edit/Write 工具调用
- 新增 SearchBar 搜索高亮 + 非匹配消息 dimmed 效果
- 支持 `WebviewPanel` 编辑器标签页模式（`AtomCode: Open in New Tab`）

**Daemon MCP 集成（1 commit）：**
- `AppState` 新增 `Arc<RwLock<Arc<McpRegistry>>>`，启动时后台连接 `~/.atomcode/mcp.json` 中配置的 MCP server
- `process_chat_request` 中自动注册 MCP 工具（命名 `mcp__{server}__{tool}`）
- 新增 `GET /mcp/status` 和 `POST /mcp/reload` API 端点

## 关联 Issue

N/A

## 变更类型
- [x] ✨ 新功能
- [x] 🐛 Bug 修复
- [x] 🗑️ 代码清理

## 测试计划
- [x] `cargo build` 成功（daemon + core）
- [ ] `cargo test` 通过
- [ ] `cargo clippy` 无警告
- [x] VS Code 扩展 F5 调试模式验证
- [x] MCP 连接验证：`curl /mcp/status` 返回 2 个 server（feishu-mcp 10 tools, chrome-devtools 29 tools）
- [x] Chat 流式响应验证：AI 能识别并列出 MCP 工具
- [x] esbuild webview 构建成功
- [x] TypeScript 编译无错误

## 已验证的 Provider
- [x] DeepSeek

## 检查清单
- [ ] 代码符合 Rust 格式化规范（`cargo fmt`）
- [ ] 改动有对应的或新增的测试覆盖
- [ ] 必要时已更新文档
- [x] PR 标题遵循 [Conventional Commits](https://www.conventionalcommits.org/) 规范
- [ ] 破坏性变更已在描述中明确说明

---

## 插件执行、使用、调试步骤

### 一、环境准备

```bash
# 1. 编译 daemon（Rust 后端）
cd /Users/liguanchen/Desktop/atomcode
cargo build -p atomcode-daemon --release

# 2. 编译 VS Code 扩展（TypeScript Extension Host + React Webview）
cd extensions/vscode
npm install                        # 安装依赖（react, marked, highlight.js, dompurify 等）
npx tsc                            # 编译 Extension Host (src/ → out/)
node webview-ui/esbuild.js         # 编译 Webview UI (webview-ui/src/ → webview/)
```

### 二、启动 Daemon

```bash
# 启动 daemon（后台运行，监听 0.0.0.0:13456）
nohup ./target/release/atomcode-daemon > /tmp/atomcode-daemon.log 2>&1 &

# 验证 daemon 运行状态
curl http://127.0.0.1:13456/health
# 期望输出: {"status":"ok","version":"4.20.4","service":"atomcode-daemon"}

# 验证 MCP 连接状态
curl http://127.0.0.1:13456/mcp/status | python3 -m json.tool
# 期望输出: servers 列表，status 为 "connected"

# 验证可用模型
curl http://127.0.0.1:13456/models
# 期望输出: [{"provider":"deepseek","model":"deepseek-v4-flash",...}]
```

### 三、开发调试（F5 模式）

```
1. 用 VS Code 打开 extensions/vscode/ 目录
2. 按 F5 启动 Extension Development Host（新窗口）
3. 在新窗口中：
   - 侧边栏点击 AtomCode 图标 → 打开侧边栏聊天
   - Cmd+Shift+P → "AtomCode: Open in New Tab" → 在编辑器标签页中打开
   - Cmd+Shift+P → "AtomCode: New Conversation" → 新建对话
```

**调试 webview 前端（React 组件）：**
```
1. 在 Extension Development Host 窗口中
2. Cmd+Shift+P → "Developer: Open Webview Developer Tools"
3. 可以看到 Console、Network、Elements（React 渲染的 DOM）
4. 修改 webview-ui/src/ 后运行 node webview-ui/esbuild.js 重新构建
5. Cmd+Shift+P → "Developer: Reload Webviews" 刷新
```

**调试 Extension Host（TypeScript 逻辑）：**
```
1. 在 extensions/vscode/src/ 中设置断点
2. F5 启动后，断点会自动命中
3. 查看 Debug Console 输出
4. 修改 src/ 后需要 npx tsc 重新编译，然后 Cmd+Shift+P → "Developer: Reload Window"
```

### 四、打包发布（VSIX）

```bash
cd extensions/vscode

# 安装打包工具（全局安装一次即可）
npm install -g @vscode/vsce

# 打包为 .vsix 文件
vsce package
# 输出: atomcode-vscode-0.1.0.vsix

# 本地安装测试
code --install-extension atomcode-vscode-0.1.0.vsix

# 卸载
code --uninstall-extension atomgit.atomcode-vscode
```

### 五、MCP 配置

```bash
# 编辑 MCP 配置
vim ~/.atomcode/mcp.json
```

配置格式：
```json
{
  "mcpServers": {
    "feishu-mcp": {
      "url": "https://mcp.feishu.cn/mcp/your-endpoint"
    },
    "chrome-devtools": {
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest"]
    },
    "custom-server": {
      "command": "/path/to/mcp-server",
      "args": ["--port", "3000"],
      "env": {
        "API_KEY": "your-key"
      }
    }
  }
}
```

```bash
# 修改配置后热重载（无需重启 daemon）
curl -X POST http://127.0.0.1:13456/mcp/reload
# 输出: {"status":"reloading"}

# 等待几秒后检查状态
curl http://127.0.0.1:13456/mcp/status | python3 -m json.tool
```

### 六、常见问题排查

| 问题 | 排查方法 |
|------|---------|
| 侧边栏空白/不加载 | 检查 daemon 是否运行：`curl http://127.0.0.1:13456/health` |
| 发送消息无响应 | 查看 daemon 日志：`tail -f /tmp/atomcode-daemon.log` |
| MCP 工具不可用 | 检查连接：`curl http://127.0.0.1:13456/mcp/status` |
| webview 样式异常 | 重新构建：`node webview-ui/esbuild.js` 后 Reload Webviews |
| TypeScript 编译错误 | `npx tsc --noEmit` 查看具体错误 |
| Tab 模式打不开 | 确认 `npx tsc` 已编译，然后 Reload Window |
| daemon 端口被占用 | `lsof -i :13456` 查看占用进程，`pkill -f atomcode-daemon` 停止旧进程 |

### 七、关键文件速查

```
extensions/vscode/
├── src/                          # Extension Host (TypeScript)
│   ├── extension.ts              # 入口，注册命令和 Provider
│   ├── chat/provider.ts          # WebviewViewProvider + WebviewPanel
│   ├── daemon/client.ts          # HTTP/SSE 客户端
│   └── daemon/process.ts         # daemon 进程管理
├── webview-ui/src/               # Webview UI (React)
│   ├── index.tsx                 # React 入口
│   ├── App.tsx                   # 根组件
│   ├── components/               # UI 组件（13个）
│   ├── state/                    # useReducer + Context 状态管理
│   ├── styles/                   # CSS（variables, index, messages, input, components）
│   └── utils/format.ts           # 工具函数
├── webview/                      # esbuild 编译产物（webview.js/css）
└── out/                          # tsc 编译产物（extension host）

crates/atomcode-daemon/src/
└── main.rs                       # HTTP API 服务（含 MCP 集成）
```
