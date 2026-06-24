# AtomCode for JetBrains

AtomCode for JetBrains 是本地 `atomcode-daemon` 的 IntelliJ 平台前端。

## 开发

环境要求：

- JDK 21
- 已生成的 Gradle wrapper
- Kotlin Gradle Plugin 2.2.21

常用命令：

```bash
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

本地开发和手动冒烟测试请使用 IntelliJ IDEA Community Edition。它不需要商业 JetBrains 许可证。对于 `runIde`，优先使用本地 Community Edition 路径，或省略 `platformLocalPath` 让 Gradle 下载配置的 Community IDE：

```bash
./gradlew runIde
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" runIde
```

使用 `-PplatformLocalPath=/Applications/IntelliJ IDEA.app` 会将 `runIde` 指向 IntelliJ IDEA Ultimate，即使在沙箱 IDE 中也需要有效的 Ultimate 许可证。

如果本地未安装 Community Edition，首次 `./gradlew runIde` 可能需要很长时间来下载和解包配置的 Community IDE。要加快手动冒烟测试，可以先构建 zip 文件，然后通过 `Install Plugin from Disk...` 安装到已运行的 IDE 中。

如果在无头环境中构建可搜索选项时本地非 IDEA 的 verifier 运行被终止，请使用以下命令重新运行兼容性检查：

```bash
./gradlew "-PplatformLocalPath=/Applications/GoLand.app" "-PskipSearchableOptions=true" verifyPlugin
```

正常的发布构建应保持可搜索选项启用，除非本地 IDE 进程因环境原因失败。

如果本地已安装 IntelliJ IDEA Community Edition，请传入 `platformLocalPath` 以避免在开发期间下载 IDE 发行版，并使 `verifyPlugin` 针对本地 IDE 而非默认的远程推荐 IDE 矩阵进行验证：

```bash
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

仅在测试机器拥有有效的 Ultimate 许可证时才使用 IntelliJ IDEA Ultimate。类似 `There are no valid licenses associated with the account ...` 的许可证错误是 IDE 授权问题，而非插件构建失败。

打包后的插件 zip 文件写入 `build/distributions/`。

如需发布包含私有签名器和所有支持平台捆绑后端的正式版本，请在仓库根目录下运行：

```bash
./build-official-jetbrains.sh [branch]
```

该脚本将插件 zip、后端二进制文件和校验和写入 `dist/v<workspace-version>/`。使用 `./build-official-jetbrains.sh clean` 可在中断构建后恢复公共存根文件。

构建时可选择将 `atomcode-daemon` 打包到 `resources/bin/<platform>`。本地开发构建在存在时会自动包含当前平台来自 `target/release` 或 `target/debug` 的后端程序。市场/发布构建可以通过以下方式提供显式的后端二进制文件：

```bash
ATOMCODE_DAEMON_DARWIN_ARM64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_DARWIN_X64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_LINUX_X64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_LINUX_ARM64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_WIN32_X64=/path/to/atomcode-daemon.exe \
./gradlew buildPlugin
```

在运行时，后端发现按顺序检查：用户配置的路径 > 打包的后端 > PATH 和常见安装位置中的 `atomcode`/`atomcode-daemon`。打包的后端资源会在启动前提取到临时可执行路径，因为 JetBrains 插件资源位于插件 jar 内部。当插件使用打包后端并在已配置端口上发现已运行的 `atomcode-daemon` 时，它会比较 `/health.version` 与 `resources/bin/daemon-version.txt`。不匹配会触发优雅的 `/shutdown` 并重新启动为打包后端；如果旧后端无法停止，连接状态会报告不兼容的后端，而不是静默地与错误版本通信。

同一项目内的并发连接尝试共享同一个正在进行的启动 future，因此 IDE 启动、状态更新和多个聊天标签页不会产生重复的后端进程。

该构建使用 Kotlin Gradle Plugin `2.2.21` 配合 `-Xjvm-default=all`。这种组合可以在针对本地 IntelliJ IDEA Community Edition 2026.1 Kotlin 元数据编译的同时，避免生成较旧的 2025.1 验证器会归类为内部 API 使用的 ToolWindowFactory 桥接方法。

## v0.1 范围

- AtomCode 工具窗口，包含聊天、设置状态、模型选择和会话控制
- 多个 AtomCode 工具窗口聊天标签页，具有隔离的活动会话，通过 JetBrains 原生标签式工具窗口匹配 VS Code 的新建标签页工作流
- JetBrains 状态栏小部件，用于 AtomCode 连接状态和快速聊天访问
- 项目启动连接初始化和定期后端健康检查
- 诊断对话框，可复制经过脱敏处理的 IDE、后端、设置、配置和队列状态信息
- 多行聊天输入，支持可配置的 Enter/Ctrl+Enter 发送行为和聊天字体大小
- 打包/本地后端发现和可选的自动启动钩子
- AtomCode 读取项目内容前自动保存文件
- 选中文本上下文的隐私控制及相对/绝对路径显示
- 用于健康、设置、提供商、会话、聊天、停止、权限和文件变更工作流的后端 REST/SSE 客户端
- 提供商创建/编辑/删除、默认模型切换以及思考/推理控制
- CodingPlan 设置触发器
- 支持文本、推理、工具、产物、令牌、停止、错误和权限事件的流式聊天
- 在生成响应时排队另一条聊天消息
- 复制最后一条助手回复，以及在活动编辑器中预览/应用最后一个围栏代码块
- 会话的新建/加载/重命名/删除
- 会话历史对话框，支持搜索、加载、重命名、单个/批量删除和刷新
- 用于打开聊天、聚焦输入、新建对话、停止生成、打开变更和打开设置的 IDE 操作
- 在新标签页中打开聊天的 IDE 操作
- 用于解释选中代码、修复选中代码、优化选中代码和添加选中代码/文件作为上下文的编辑器操作
- 用于解释选中代码、修复选中代码、优化选中代码和添加选中代码/文件作为上下文的 Alt+Enter 意图操作
- 下一条聊天消息的上下文附件队列，包括 IDE 文件选择器附件
- 上下文级别行为：最小、CurrentFile 自动上下文，或 ProjectContext 元数据加当前文件
- 用于审查修改文件的 Git/本地变更入口点

## JetBrains 操作

插件注册了 JetBrains 原生操作，用户可以通过全局搜索（Search Everywhere）、工具菜单、编辑器上下文菜单、Alt+Enter 意图或自定义快捷键来调用 AtomCode：

- `AtomCode: Open Chat`（打开聊天）
- `AtomCode: Open Chat in New Tab`（在新标签页中打开聊天）
- `AtomCode: Focus Input`（聚焦输入框，快捷键 `Ctrl+Alt+Shift+I`）
- `AtomCode: New Conversation`（新建对话，快捷键 `Ctrl+Alt+Shift+N`）
- `AtomCode: Stop Generation`（停止生成）
- `AtomCode: Open Changes`（打开变更）
- `AtomCode: Open Settings`（打开设置）
- `AtomCode: Explain Selection`（解释选中代码，快捷键 `Ctrl+Alt+Shift+E`）
- `AtomCode: Fix Selection`（修复选中代码）
- `AtomCode: Optimize Selection`（优化选中代码）
- `AtomCode: Add Selection/File as Context`（添加选中代码/文件作为上下文）

工具窗口中的提供商行暴露了后端 `/providers/{name}/thinking` API 的 `Thinking` 控制，匹配 VS Code 中支持推理预算的模型的提供商设置工作流。

聊天输入还支持与 VS Code 对齐的斜杠命令：

- `/login`（登录）
- `/codingplan`（编码计划）
- `/explain`（解释）
- `/fix`（修复）
- `/test`（测试）
- `/refactor`（重构）
- `/docs`（文档）
- `/review`（审查）
- `/optimize`（优化）

## 从磁盘安装

1. 构建插件：

   ```bash
   ./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
   ```

2. 在 IntelliJ IDEA Community Edition 中，打开 `Settings | Plugins`。
3. 使用齿轮菜单并选择 `Install Plugin from Disk...`。
4. 选择：

   ```text
   build/distributions/atomcode-jetbrains-0.1.0.zip
   ```

5. 如果提示，重启 IDE。

## 端到端冒烟测试

在认为插件构建可用之前，请运行以下检查清单：

1. 打开 `AtomCode` 工具窗口。
2. 运行 `AtomCode: Open Chat in New Tab` 或点击 `New Tab`，确认出现第二个可关闭的聊天标签页，并确认每个标签页保留自己的已加载/新会话，同时编辑器/上下文操作以选中标签页为目标。
3. 点击 `Start`，确认状态变为已连接。
4. 确认 IDE 状态栏显示 `AtomCode` 已连接状态，点击它聚焦选中的聊天标签页。
5. 点击 `Settings` 或运行 `AtomCode: Open Settings`，调整一个无害的设置，确认 AtomCode 设置页面已打开。
6. 点击 `Provider`，创建一个 OpenAI/Claude/Ollama 提供商，并将其设置为默认。
7. 确认设置状态显示提供商数量，模型下拉列表列出提供商模型。
8. 发送一条简单的聊天提示，确认流式输出出现。
9. 输入多行提示，确认 Enter/Ctrl+Enter 行为遵循 AtomCode 设置。
10. 在设置中更改 `Chat font size`（聊天字体大小），重新打开/聚焦工具窗口，确认聊天/输入文本大小随之变化。
11. 确认启用 `Auto-save files before AtomCode reads them`（AtomCode 读取文件前自动保存），编辑一个文件而不保存，附加/发送，确认使用的是已保存的内容。
12. 在响应流式传输时，输入另一条提示，点击 `Queue`，确认当前响应完成后自动发送。
13. 请求一个代码块，点击 `Copy Last`，确认最后一条助手回复被复制。
14. 在编辑器打开的情况下，点击 `Apply Code`，检查 JetBrains 差异预览，确认，并检查最后一个围栏代码块插入到光标处或替换了选中内容。
15. 在长响应期间点击 `Stop`，确认生成停止。
16. 打开编辑器文件，右键点击 `AtomCode: Add Selection/File as Context`，然后发送提示，确认上下文显示并被使用。
17. 在设置中禁用 `Allow selected text context`（允许选中文本上下文），确认选择操作被禁用，而整个文件上下文仍然有效。
18. 切换 `Send relative path with selection`（发送选中内容的相对路径），确认附加的上下文标签相应地使用相对或绝对路径。
19. 将 `Context level`（上下文级别）设置为 `CurrentFile`，在编辑器打开的情况下发送提示，确认当前文件自动包含在内。
20. 将 `Context level` 设置为 `ProjectContext`，发送提示，确认项目元数据加当前文件上下文都包含在内。
21. 点击 `Attach File`，选择一个项目文件，然后发送提示，确认文件上下文显示并被使用。
22. 选择代码并运行 `AtomCode: Explain Selection`。
23. 创建一个新会话，发送一条消息，刷新会话，重新加载该会话，重命名它，然后删除它。
24. 点击 `History`，搜索一个会话，加载它，重命名它，选择多个会话，批量删除它们，然后刷新列表。
25. 点击 `Diagnostics`，确认一份脱敏的诊断报告已打开，并确认它已被复制到剪贴板。
26. 编辑项目中的一个文件，点击 `Changes`，确认本地变更打开，修改过的文件已打开。

## 验证状态

已知的无需许可证的本地检查：

```bash
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

最新的本地 Community Edition 验证器运行已通过 `IC-261.25134.95`，并将 HTML 报告写入 `build/reports/pluginVerifier/IC-261.25134.95/report.html`。

Ultimate 兼容性回归检查也在不启动许可 IDE UI 的情况下本地通过：

```bash
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" test
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" buildPlugin
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" verifyPlugin
```

最新的本地 Ultimate 验证器运行已通过 `IU-251.26094.121`，并将 HTML 报告写入 `build/reports/pluginVerifier/IU-251.26094.121/report.html`。

其他本地的兼容性检查已通过：

- `PY-251.26927.74` (`/Applications/PyCharm.app`)
- `GO-251.26094.127` (`/Applications/GoLand.app`，在本地 searchable-options IDE 进程退出 137 后使用 `-PskipSearchableOptions=true` 运行验证器)

后端预检冒烟测试：

```bash
cd webui && npm ci --cache .npm-cache && npm run build
cargo check -p atomcode-daemon
cargo build -p atomcode-daemon
./target/debug/atomcode-daemon --host 127.0.0.1 --port 13456 --idle-timeout 0 --no-telemetry --client jetbrains
curl -sS http://127.0.0.1:13456/health
curl -sS -X POST http://127.0.0.1:13456/cd -H "Content-Type: application/json" -d '{"path":"/path/to/project"}'
curl -sS http://127.0.0.1:13456/auth/status
curl -sS http://127.0.0.1:13456/providers
curl -sS http://127.0.0.1:13456/models
curl -sS http://127.0.0.1:13456/sessions
curl -sS http://127.0.0.1:13456/config
```

最新的本地后端冒烟测试返回了 `service=atomcode-daemon`、`version=4.25.0`，成功更改了项目目录，并返回了 auth/provider/model/session/config 的数据。这验证了 JetBrains 插件在完整的 IDE 安装冒烟测试之前所使用的 HTTP 端点。

## 安全说明

插件默认将后端主机设置为 `127.0.0.1`，使用后端的 HTTP API，并且不收集插件遥测数据。在发送编辑器选中内容或文件作为聊天上下文之前，会应用敏感路径分类。
