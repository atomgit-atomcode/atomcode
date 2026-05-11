use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn zh_cn(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 =>
            "欢迎使用 AtomCode，请选择一项开始：".into(),
        Msg::WelcomeBannerLine2 =>
            "（↑↓ 切换，Enter 确认，Esc 跳过）".into(),
        Msg::WelcomeOptionCodingPlan => "配置 CodingPlan".into(),
        Msg::WelcomeOptionCodingPlanHint => "免费额度 · 推荐".into(),
        Msg::WelcomeOptionConfigureManually => "手动配置".into(),
        Msg::WelcomeOptionConfigureManuallyHint => "使用 API key".into(),
        Msg::WelcomeOptionSkip => "暂时跳过".into(),
        Msg::WelcomeOptionSkipHint => "稍后再说".into(),

        Msg::ErrUnsupportedLocale { input } =>
            format!("不支持的语言：{input}").into(),

        // ── 状态栏 ──
        Msg::StatusNoProvider =>
            "未配置 Provider · 使用 /provider 配置".into(),
        Msg::StatusUpgradeHint { version } =>
            format!("↑ {version} 可用 · 使用 /upgrade 升级").into(),
        Msg::StatusModelNotConfigured =>
            "（未配置）".into(),

        // ── 帮助 ──
        Msg::HelpAvailableCommands =>
            "  可用命令：\n".into(),

        // ── Provider 向导 ──
        Msg::ProviderWizardHeader =>
            "  Provider 管理 — 添加 / 编辑 / 删除 / 设为默认。按 Esc 取消。\n".into(),
        Msg::ProviderWizardCancelled =>
            "（已取消）".into(),
        Msg::ProviderMenuAdd => "添加".into(),
        Msg::ProviderMenuAddDesc => "添加新 Provider".into(),
        Msg::ProviderMenuEdit => "编辑".into(),
        Msg::ProviderMenuEditDesc => "编辑已有 Provider".into(),
        Msg::ProviderMenuDelete => "删除".into(),
        Msg::ProviderMenuDeleteDesc => "移除 Provider".into(),
        Msg::ProviderMenuSetDefault => "设为默认".into(),
        Msg::ProviderMenuSetDefaultDesc => "切换默认 Provider".into(),
        Msg::ProviderNoProviders =>
            "尚未配置任何 Provider。".into(),
        Msg::ProviderDeleteConfirm { name } =>
            format!("删除 \"{name}\"？[y/N]").into(),
        Msg::ProviderDeleted { name } =>
            format!("已移除 \"{name}\"。").into(),
        Msg::ProviderDeleteKept => "（已保留）".into(),
        Msg::ProviderDefaultSet { name } =>
            format!("默认已设为 {name}。").into(),
        Msg::ProviderAdded { name, model } =>
            format!("已添加 Provider \"{name}\"，并切换到 {name} · {model}。").into(),
        Msg::ProviderUpdated { name } =>
            format!("已更新 \"{name}\"。").into(),
        Msg::ProviderStepName => "Provider 名称？".into(),
        Msg::ProviderStepType => "类型？（openai / claude / ollama）".into(),
        Msg::ProviderStepTypeWithHint { current } =>
            format!("类型？[{current}]（openai / claude / ollama，留空保持不变）").into(),
        Msg::ProviderStepBaseUrl =>
            "Base URL？（留空使用默认值）".into(),
        Msg::ProviderStepBaseUrlWithHint { current } =>
            format!("Base URL？[{current}]（留空保持不变）").into(),
        Msg::ProviderDefaultHint => "Provider 默认值".into(),
        Msg::ProviderStepApiKey =>
            "API 密钥？（留空不设置）".into(),
        Msg::ProviderStepApiKeyWithHint { hint } =>
            format!("API 密钥？[{hint}]").into(),
        Msg::ProviderStepApiKeySet => "已设置 — 留空保持不变".into(),
        Msg::ProviderStepApiKeyUnset => "未设置".into(),
        Msg::ProviderStepModel => "模型？".into(),
        Msg::ProviderStepModelWithHint { current } =>
            format!("模型？[{current}]（留空保持不变）").into(),
        Msg::ProviderNameEmpty => "名称不能为空。".into(),
        Msg::ProviderUnknownType =>
            "未知类型。请选择 openai / claude / ollama。".into(),
        Msg::ProviderUnknownTypeEdit =>
            "未知类型。请选择 openai / claude / ollama 或留空。".into(),
        Msg::ProviderModelEmpty => "模型不能为空。".into(),
        Msg::ProviderEditKeep => "（保持不变）".into(),

        // ── Model 选择器 ──
        Msg::ModelSwitched { provider, model } =>
            format!("  已切换到 {provider} · {model}\n").into(),

        // ── 会话选择器 ──
        Msg::SessionLoadFailed { error } =>
            format!("加载会话失败：{error}").into(),
        Msg::SessionResumedLabel { name } =>
            format!("已恢复：{name}").into(),
        Msg::SessionTimeJustNow => "刚刚".into(),
        Msg::SessionTimeMinAgo { n } => format!("{n}分钟前").into(),
        Msg::SessionTimeHourAgo { n } => format!("{n}小时前").into(),
        Msg::SessionTimeDayAgo { n } => format!("{n}天前").into(),
        Msg::SessionMsgCount { count } =>
            format!("{count} 条消息").into(),

        // ── 目录选择器 ──
        Msg::DirCurrent => "当前".into(),
        Msg::DirNotExists { path } =>
            format!("目录已不存在：{path}").into(),
        Msg::DirChanged { path } =>
            format!("  已切换到：{path}\n").into(),

        // ── Issue 向导 ──
        Msg::IssueCancelled => "（已取消）".into(),
        Msg::IssueNewOn { owner, repo } =>
            format!("在 atomgit.com/{owner}/{repo} 创建 Issue").into(),
        Msg::IssueStep1 =>
            "步骤 1/2 — 输入标题（必填，按 Esc 取消）：".into(),
        Msg::IssueStep2 =>
            "步骤 2/2 — 输入描述（Shift+Enter 换行，Enter 提交，Esc 取消）：".into(),
        Msg::IssueTitleConfirmed { title } =>
            format!("✓ 标题：{title}").into(),
        Msg::IssueRequiredField { field } =>
            format!("（必填 — 请输入 {field}，或按 Esc 取消）").into(),

        // ── 语言 ──
        Msg::LanguageSetTo { locale } =>
            format!("语言已切换为：{locale}").into(),

        // ── 空闲/引导提示 ──
        Msg::IdleHintPrefix =>
            "输入内容，或按 ".into(),
        Msg::IdleHintSlash => "/".into(),
        Msg::IdleHintSuffix =>
            " 浏览命令".into(),
        Msg::IdleHintFull =>
            "输入内容，或按 / 浏览命令".into(),
        Msg::IdleHintProvider => "/provider".into(),
        Msg::IdleHintProviderSuffix =>
            "添加自定义模型".into(),
        Msg::IdleHintProviderFull =>
            "使用 /provider 添加自定义模型".into(),

        // ── 斜杠命令 ──
        Msg::CmdSwitchedPlanMode =>
            "  已切换到 Plan 模式（只读探索）。\n".into(),
        Msg::CmdSwitchedBuildMode =>
            "  已切换到 Build 模式（完整执行）。\n".into(),
        Msg::CmdNewSession =>
            "  新会话已开始。\n".into(),
        Msg::CmdNoProviders =>
            "  未配置任何 Provider。\n".into(),
        Msg::CmdNoSessions =>
            "  未找到历史会话。请先开始一段对话。\n".into(),
        Msg::CmdUnknownCommand { name } =>
            format!("未知命令：/{name}").into(),
        Msg::CmdLoginFailed { error } =>
            format!("登录失败：{error}").into(),
        Msg::CmdLogoutDone =>
            "  已退出 AtomGit 登录。权限已刷新。\n".into(),
        Msg::CmdLogoutFailed { error } =>
            format!("退出登录失败：{error}").into(),
        Msg::CmdWhoamiNotSignedIn =>
            "  尚未登录。使用 /login 进行认证。\n".into(),
        Msg::CmdReloadDone { provider, model } =>
            format!("  配置已重载。当前：{provider} · {model}\n").into(),
        Msg::CmdReloadFailed { error } =>
            format!("重载失败：{error}（保留先前配置）").into(),
        Msg::CmdUndoNotSupported =>
            "  撤销功能暂不支持。\n".into(),
        Msg::CmdNoChanges =>
            "  （无变更）\n".into(),
        Msg::CmdCheckingUpdate =>
            "  正在检查更新...\n".into(),
        Msg::CmdNoActiveProvider =>
            "未配置活跃的 Provider。使用 /provider 添加一个。".into(),

        // ── 审批提示 ──
        Msg::ApprovalPromptAlt { tool, detail } =>
            format!("允许 {}({})？[Y]是 / [N]否 / [A]总是", tool, detail).into(),
        Msg::ApprovalWaitingLabel =>
            "▶ 等待审批：".into(),
        Msg::ApprovalAllow => " 允许  ".into(),
        Msg::ApprovalAlways => " 总是  ".into(),
        Msg::ApprovalDeny => " 拒绝".into(),

        // ── 取消 / 错误前缀 ──
        Msg::Cancelled => "（已取消）".into(),
        Msg::ErrorPrefix { msg } =>
            format!("[错误：{msg}]").into(),

        // ── 升级 ──
        Msg::UpgradeSuccess { from, to } =>
            format!("  ✓ 已升级 {} → {}\n", from, to).into(),

        // ── 终端键盘提示 ──
        Msg::KbdHintMacos =>
            "  ⚠ 终端不支持增强键盘协议。\n    请使用 Ctrl+Enter 插入换行（Shift+Enter 不可用）。\n\n".into(),
        Msg::KbdHintOther =>
            "  ⚠ 终端不支持增强键盘协议。\n    请使用 Alt+Enter 或 Ctrl+Enter 插入换行（Shift+Enter 不可用）。\n\n".into(),

        // ── JediTerm / conhost 回退 ──
        Msg::JediTermFallback =>
            "  ⓘ 检测到 JetBrains IDE 终端 — 运行在备用屏幕模式下。\n    \
            使用鼠标滚轮、PageUp/PageDown 或 Shift+Up/Down 滚动历史。\n    \
            AtomCode 运行期间无法使用宿主终端的原生回滚；\n    \
            退出后宿主终端将恢复到 AtomCode 之前的状态。\n    \
            设置 ATOMCODE_PLAIN=1 使用基础 CI 风格输出，或\n    \
            设置 ATOMCODE_RETAIN=1 绕过此回退（可能导致对齐问题）。\n\n".into(),
        Msg::LegacyConhostFallback =>
            "  ⓘ 检测到旧版 Windows 控制台 — 运行在备用屏幕模式下。\n    \
            使用鼠标滚轮、PageUp/PageDown 或 Shift+Up/Down 滚动历史。\n    \
            AtomCode 运行期间无法使用宿主终端的原生回滚。\n    \
            要获得完整的宿主终端回滚支持，请安装 Windows Terminal\n    \
            （免费，Microsoft Store）、ConEmu、Alacritty 或 WezTerm。\n    \
            设置 ATOMCODE_PLAIN=1 使用基础输出，或设置 ATOMCODE_RETAIN=1\n    \
            绕过此回退（滚动时可能出现重复内容）。\n\n".into(),

        // ── 会话回放 ──
        Msg::SessionReplayHint =>
            "  ⓘ 正在显示上次会话 — 模型上下文从头开始。\n    \
            使用 /resume 完整恢复对话，包括模型记忆。\n\n".into(),

        // ── 后台任务 ──
        Msg::BackgroundComplete { turns } =>
            format!("  后台任务完成（{} 轮）：\n", turns).into(),
        Msg::BackgroundFailed { turns } =>
            format!("  后台任务失败，共 {} 轮：\n", turns).into(),
        Msg::BackgroundFilesEdited =>
            "  已编辑的文件：\n".into(),

        // ── /config ──
        Msg::ConfigProviderLabel { provider, path } =>
            format!("  Provider：{}\n  配置文件：{}\n\n", provider, path).into(),

        // ── /cost ──
        Msg::CostReport { prompt, completion, cached, cache_rate, total, cost } =>
            format!(
                "  提示 Token：       {}\n  补全 Token：       {}\n  缓存 Token：       {}（{}% 命中率）\n  Token 总计：       {}\n  预估费用：         {}\n",
                prompt, completion, cached, cache_rate, total, cost
            ).into(),

        // ── /think ──
        Msg::ThinkStatus { status, budget, provider } =>
            format!(
                "  深度思考：{}\n  预算：{} Token\n  Provider：{}\n\n  用法：/think on | off | budget <N>\n",
                status, budget, provider
            ).into(),
        Msg::ThinkEnabled { budget } =>
            format!("  深度思考已启用（预算：{} Token）。\n", budget).into(),
        Msg::ThinkDisabled =>
            "  深度思考已禁用。\n".into(),
        Msg::ThinkBudgetSet { n } =>
            format!("  思考预算已设为 {} Token。\n", n).into(),
        Msg::ThinkBudgetTooSmall { n } =>
            format!("预算必须 >= 1024（当前 {}）", n).into(),
        Msg::ThinkBudgetUsage =>
            "用法：/think budget <数字>".into(),
        Msg::ThinkUsage =>
            "  用法：/think [on | off | budget <N>]\n".into(),

        // ── /remember, /forget ──
        Msg::RememberUsage =>
            "用法：/remember <要记住的内容>（--global 为全局范围）".into(),
        Msg::ForgetUsage =>
            "用法：/forget <关键词>".into(),

        // ── /background ──
        Msg::BackgroundUsage =>
            "  用法：/background <任务描述>\n".into(),

        // ── /init ──
        Msg::InitAlreadyExists { path } =>
            format!("  {} 已存在。使用 `/init --force` 覆盖。\n", path).into(),
        Msg::InitWrote { path, bytes } =>
            format!("  已写入 {}（{} 字节）。编辑以自定义；下次会话生效。\n", path, bytes).into(),
        Msg::InitFailed { error } =>
            format!("  /init 失败：{}\n", error).into(),

        // ── /cd ──
        Msg::CdWorkingDir { cwd } =>
            format!("  工作目录：{}\n  无最近项目。使用 `/cd <路径>` 切换。\n", cwd).into(),

        // ── /diff ──
        Msg::DiffFailed { error } =>
            format!("git diff 失败：{}", error).into(),

        // ── /upgrade ──
        Msg::UpgradeUnknownArg { arg } =>
            format!("未知的 /upgrade 参数：{}\n  用法：/upgrade [rollback|--force]", arg).into(),

        // ── /skills ──
        Msg::SkillsNone =>
            "  没有可调用的技能。\n".into(),
        Msg::SkillsAvailable =>
            "  可用技能：\n".into(),
        Msg::SkillUnknown { name } =>
            format!("未知技能：{}（输入 /skills 查看列表）", name).into(),

        // ── /mcp ──
        Msg::McpReloading { count } =>
            format!("  正在重载 MCP 服务器...（{} 个已配置）\n", count).into(),
        Msg::McpConnecting =>
            "  正在连接：\n".into(),
        Msg::McpConnectingServer { name } =>
            format!("    - {}  连接中...\n", name).into(),
        Msg::McpNoServersConfigured =>
            "  未配置 MCP 服务器。\n".into(),
        Msg::McpClearedReconnecting { removed } =>
            format!("  ✓ 已清除 {} 个 MCP 工具。正在后台重新连接...\n", removed).into(),
        Msg::McpClearedNoServers { removed } =>
            format!("  ✓ 已清除 {} 个 MCP 工具。无需连接。\n", removed).into(),
        Msg::McpToolsUsage =>
            "  用法：/mcp tools <服务器名>\n  示例：/mcp tools filesystem\n".into(),
        Msg::McpToolsListing { server } =>
            format!("  正在列出 '{}' 的 MCP 工具...\n", server).into(),
        Msg::McpNoRegistry =>
            "  MCP 注册表未加载。请先运行 /mcp reload。\n".into(),
        Msg::McpServersHeader =>
            "  MCP 服务器：\n".into(),
        Msg::McpReloadFailed { error } =>
            format!("MCP 重载失败：无法加载 .mcp.json / ~/.atomcode/mcp.json：{:#}", error).into(),

        // ── /worktree ──
        Msg::WorktreeUsage =>
            "  用法：\n    /worktree create <分支> [基准]   创建工作树并切换\n    /worktree list                    列出所有工作树\n    /worktree done                    切回原始目录\n    /worktree cleanup <分支>          清理工作树\n".into(),
        Msg::WorktreeCreateUsage =>
            "  用法：/worktree create <分支> [基准]\n  示例：/worktree create fix-bug main\n".into(),
        Msg::WorktreeCreated { branch, base, path } =>
            format!("  ✓ 工作树已创建\n    分支：{}（基于 {}）\n    路径：{}\n    工作目录已切换\n", branch, base, path).into(),
        Msg::WorktreeCreateFailed { error } =>
            format!("工作树创建失败：{}", error).into(),
        Msg::WorktreeNoActive =>
            "  没有活跃的工作树。\n".into(),
        Msg::WorktreeListFailed { error } =>
            format!("工作树列表失败：{}", error).into(),
        Msg::WorktreeActiveHeader =>
            "  活跃工作树：\n".into(),
        Msg::WorktreeHasChanges => "（有变更）".into(),
        Msg::WorktreeClean => "（clean）".into(),
        Msg::WorktreeCurrent => " ← 当前".into(),
        Msg::WorktreeDoneBack { path } =>
            format!("  ✓ 工作目录已切回：{}\n", path).into(),
        Msg::WorktreeDoneMergeHint { branch } =>
            format!("  提示：使用 'git merge {}' 或创建 PR 合入主分支\n", branch).into(),
        Msg::WorktreeNoSession =>
            "  没有活跃的工作树会话。先使用 /worktree create 创建一个。\n".into(),
        Msg::WorktreeCleanupUsage =>
            "  用法：/worktree cleanup <分支> [--force]\n".into(),
        Msg::WorktreeCleaned { branch } =>
            format!("  ✓ 工作树 '{}' 已清理\n", branch).into(),
        Msg::WorktreeCleanedSwitched { path } =>
            format!("  工作目录已切回：{}\n", path).into(),
        Msg::WorktreeCleanupUncommitted { branch } =>
            format!("  ⚠ 工作树 '{}' 有未提交的变更。\n  使用 /worktree cleanup {} --force 强制清理\n", branch, branch).into(),
        Msg::WorktreeCleanupFailed { error } =>
            format!("工作树清理失败：{}", error).into(),

        // ── /help commands（自定义命令） ──
        Msg::HelpCustomCommandsHeader =>
            "  自定义命令：\n".into(),
        Msg::HelpCustomNone =>
            "    （无）\n\n".into(),
        Msg::HelpCustomCreateHint =>
            "  创建方式：~/.atomcode/commands/<名称>.md 或 .atomcode/commands/<名称>.md\n".into(),
        Msg::HelpSourceGlobal => "全局".into(),
        Msg::HelpSourceProject => "项目".into(),

        // ── /plugin ──
        Msg::PluginUsage =>
            "用法：/plugin [marketplace add|remove|update|list | install <p>@<m> | uninstall <p>@<m> | list]".into(),
        Msg::PluginMarketplaceUsage =>
            "用法：/plugin marketplace [add|remove|update|list] <参数>".into(),
        Msg::PluginInstallUsage =>
            "用法：/plugin install <插件>@<市场>".into(),
        Msg::PluginUninstallUsage =>
            "用法：/plugin uninstall <插件>@<市场>".into(),
        Msg::PluginNoMarketplaces =>
            "未注册任何市场".into(),
        Msg::PluginMarketplacesHeader =>
            "已注册的市场：".into(),
        Msg::PluginNoInstalled =>
            "未安装任何插件".into(),
        Msg::PluginInstalledHeader =>
            "已安装的插件：".into(),

        // ── 命令描述 ──
        Msg::CmdDescCodingplan =>
            "领取 CodingPlan 并从计划的模型列表中配置模型".into(),
        Msg::CmdDescResume => "恢复上次会话".into(),
        Msg::CmdDescLogin => "使用 AtomGit OAuth 登录".into(),
        Msg::CmdDescLogout => "退出 AtomGit 登录".into(),
        Msg::CmdDescWhoami => "显示当前登录用户".into(),
        Msg::CmdDescModel => "切换 Provider / 模型".into(),
        Msg::CmdDescProvider => "管理 Provider（添加 / 编辑 / 删除）".into(),
        Msg::CmdDescStatus => "显示会话状态".into(),
        Msg::CmdDescConfig => "显示配置文件路径".into(),
        Msg::CmdDescReload => "从磁盘重新加载 ~/.atomcode/config.toml".into(),
        Msg::CmdDescCd => "切换工作目录".into(),
        Msg::CmdDescInit => "从工作目录生成 .atomcode.md 项目指令".into(),
        Msg::CmdDescBackground => "在隔离的后台上下文中运行一次性任务（只读工具子集）".into(),
        Msg::CmdDescDiff => "显示 git diff".into(),
        Msg::CmdDescClear => "清屏".into(),
        Msg::CmdDescSession => "开始新会话（清除对话）".into(),
        Msg::CmdDescCost => "显示 Token 费用".into(),
        Msg::CmdDescContext => "显示上下文预算明细".into(),
        Msg::CmdDescCompact => "压缩对话历史".into(),
        Msg::CmdDescRemember => "保存记忆（/remember --global 为全局）".into(),
        Msg::CmdDescForget => "删除匹配的记忆".into(),
        Msg::CmdDescMemory => "显示所有已保存的记忆".into(),
        Msg::CmdDescMcp => "显示 MCP 服务器状态（子命令：reload）".into(),
        Msg::CmdDescUndo => "撤销上次变更（暂不支持）".into(),
        Msg::CmdDescWorktree => "Git 工作树隔离（create/list/done/cleanup）".into(),
        Msg::CmdDescUpgrade => "升级到最新版本（子命令：rollback）".into(),
        Msg::CmdDescIssue => "为 AtomCode 报告 Bug / 提出功能建议（交互式向导）".into(),
        Msg::CmdDescPlan => "切换到 Plan 模式（只读探索）".into(),
        Msg::CmdDescBuild => "切换到 Build 模式（完整执行）".into(),
        Msg::CmdDescThink => "深度思考控制（on/off/budget N）".into(),
        Msg::CmdDescHelp => "显示帮助".into(),
        Msg::CmdDescLanguage => "切换显示语言".into(),
        Msg::CmdDescQuit => "退出 AtomCode".into(),
        Msg::CmdDescSkills => "浏览已加载的技能".into(),
        Msg::CmdDescPlugin => "插件市场（子命令：marketplace, install, uninstall, list）".into(),

        // ── 配置保存失败 ──
        Msg::ConfigSaveFailed { error } =>
            format!("配置保存失败：{}", error).into(),
    }
}
