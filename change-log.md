# 变更日志 — release/v4.25.2

> 基准：`7395b2955b032800b42b5fd3f9dbadccea194e31` (main)
> 范围：main → HEAD (`8bbdc6a1`)
> 提交数：**121 个** | 变更文件：**485 个** | +68,330 / -82,568 行

---

## 新功能 (feat)

### 🏗️ 引擎 v2 栈 (新 crate)
- `atomcode-kernel` / `atomcode-capabilities` / `atomcode-coding` / `atomcode-bridge` / `atomcode-review` / `atomcode-clix` 六个新 crate
- v2 成为默认引擎 (`--engine v1` 回退)
- 缓存友好的历史压缩 (StubCompaction) + OverflowCompaction
- 计划模式 (PlanModeGate + PlanModeReminderHook)
- 按轮次系统提醒 (StatusReminderHook)
- `/undo` / `/background` / `/plan` / `!cmd` 本地 shell 透传
- `/review` 内联代码审查能力
- ToolBatchStarted/ToolBatchCompleted 并行工具 UI 分组事件
- Compacted 事件携带 CompactTrigger（溢出遥测）
- 工具中间件决策模型 (BeforeOutcome/AfterOutcome)

### 🧩 JetBrains 插件
- 全新 IntelliJ IDEA 插件脚手架
- JBCefBrowser 替代 JEditorPane，支持 CSS3 现代气泡渲染
- 多标签页聊天与会话状态管理 (Redux 风格)
- Daemon 客户端、SSE 解析器、权限管理
- Markdown 渲染 (marked/purify/highlight)
- 会话搜索 + 300ms 防抖

### 📝 代码审查 (atomcode-review)
- 独立 `atomcodex review` CLI 子命令
- 46 种语言/文件类型审查规则 (rules/*.md)
- 规则引擎内置 + `--rules-dir` / `--no-rules` 覆盖
- Diff 行号标注、变更文件清单驱动覆盖
- `--append-system-prompt` 追加模式
- JSON envelope 输出、用量累加、修复建议字段
- 收尾复扫条款：数值边界/热路径/同位二级缺陷

### 🛠️ MCP 增强
- Streamable-HTTP 会话支持 (Mcp-Session-Id 捕获与回传)
- 关闭时 HTTP DELETE 释放会话
- 递归 SKILL.md 发现 (深度 ≤ 8 层)

### 🔐 安全增强
- 敏感路径读取拦截 (SensitivePathGate)：SSH 密钥 / .env / 凭证等
- 外部路径 Enumerate 操作需用户确认
- DNS 重新绑定 TOCTOU 修复 + IPv4 兼容 IPv6 SSRF
- web_fetch 请求固定已验证 IP

### 🔧 插件系统
- `clone_with_optional_auth` + `git_pull_ff` 认证感知操作
- `host_is_trusted` / `scheme_host_prefix` URL 工具
- 非交互式 git (GIT_TERMINAL_PROMPT=0) 防止 TUI 冻结

### 🌐 国际化
- `--help` 支持根据 config/env 语言设置显示中英文

### 🤖 GitCode API 工具
- `atomgit_repo` / `atomgit_pr` / `atomgit_issue` 工具
- 仓库 CRUD、PR 管理、Issue 管理
- 创建 Tag (create_tag)

### 📋 其他
- web_fetch: 可选 max_chars、Markdown 输出格式、真实浏览器 UA
- web_search: 配置驱动后端 (Exa/DDG)
- 当前日期注入 system prompt
- `/exit` 命令 (同 `/quit`)
- `/goal` 目标评估 + 评估器
- 邀请页面支持 `/invite/{code}` 路径格式
- 推理强度选择器 (Reasoning Effort: 默认/High/Max)
- Memory.md 用户驱动记忆能力

---

## 修复 (fix)

### 🔬 引擎 v2 栈
- 跨模型验证修复：provider 选择、reasoning_history、reasoning_effort 透传
- `--dangerously-skip-permissions` 在 v2 下生效
- `/setup` 后 bridge respawn 失败根因修复（3 个）
- v2 引擎图片识别失败 + VL 预处理冗余消除
- 审批 call_id 为空和 reason 错误
- daemon turn loop 错误消息清空对话修复
- 死 bridge 静默 no-op 修复
- 审批中途取消修复

### 🧠 推理/思考
- DeepSeek V4 Flash reasoning 持久化修复
- 非散文占位符 "·" 替代 "(no reasoning recorded)"
- 所有异常流终止路径保留 reasoning
- 空响应快速重试 (5 次 × 1/1/2/2/3s)
- 流超时 120s → 300s (环境可配置)

### 🎨 TUI
- 深色主题 Markdown 表格边框可见性
- 输入框高度上限 (10 行) 防止溢出
- 取消 prompt 时保留已输入草稿
- 滚动后 footer 延迟修复 (同 tick 重绘)
- 审批芯片真彩色 (Allow/Always/Deny)
- 工具名换行后高亮保留
- 并行 edit_file TUI/JetBrains 双条展示修复
- 只有 `/goal` 激活时才延迟 turn 分隔符
- 审批提示中 Enter 默认显示 `↵`

### 🧩 JetBrains 插件
- 助手消息回合生命周期与 JBCef 初始化时序修复
- StackOverflowError 修复 (Flow API → ofInputStream)
- 连接超时优化 (3s 短超时)

### 🔐 安全
- `edit_file`/`write_file`/`search_replace` 范围化审批 (RequireApprovalScoped)
- 插件命令 `/wechat` 门控修复
- Windows 详细路径哈希处理

### 🌐 VSCode 扩展
- `send` useCallback 中统一使用 stateRef.current 避免闭包陈旧引用
- ECONNREFUSED 重连后检查 controller.signal.aborted 避免竞态
- Session 删除顺序修复数据丢失

### 📦 其他
- 余额不足错误 fail-fast 并透传真实原因
- MCP 服务器超时启动修复 (notify_one 存储许可)
- 外部路径 Enumerate 操作修复
- 递归 SKILL.md 发现修复
- 约束 edit_file 重复调用
- `compact_old_tool_results_in_place keep_recent_turns=0` 数组越界修复

---

## 重构 (refactor)

- Core → tuix/cli 模块迁移：commands/git/init/pricing/version_check → atomcode-tuix；uninstall → atomcode-cli
- 编辑工具返回信息增加 compact diff 摘要 + TUI 展示简化
- 引擎 v2 提供者切换 Warning 日志移除
- JetBrains 聊天面板 UI 重构
- Kernel turn_end → offer_continuation 重命名 + turn_complete 添加
- C1 全量编码助手组装 (prepare/assemble + CodingParts)

---

## 文档 / 测试 / 工具链

- atomcode-clix/atomcode-review 中文 README
- kernel/capabilities/coding → cli/tuix 对等积压文档
- 25 种新语言审查规则 + 一致性测试
- 评测稳定漏检反向补规则条目
- 按评测稳定漏检反向补规则条目
- C1 预提交审查修复 (adversarial review)
- 测试固件 evaluator_provider / goal_indicator 补全
- 代码审查：前缀缓存确定性测试、reasoning 持久化回归测试
