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

        // ── /codingplan ──
        Msg::CodingPlanSetupFailed { error } =>
            format!("CodingPlan 设置失败：{error}").into(),
        Msg::CpSetupHeader =>
            "  AtomCode CodingPlan 配置：\n\n".into(),
        Msg::CpLoggedIn { who, username, email } =>
            format!("  ✔ 已登录：{} ({}，{})\n", who, username, email).into(),
        Msg::CpStepSkipped { reason } =>
            format!("  ✔ {}\n", reason).into(),
        Msg::CpLoginFailed { error } =>
            format!("  ✘ 登录失败 — {}\n", error).into(),
        Msg::CpClaimed { message, plan_type } =>
            format!("  ✔ CodingPlan 已领取 — {}（CodingPlan {}）\n", message, plan_type).into(),
        Msg::CpClaimSuccessFallback => "成功".into(),
        Msg::CpAlreadyClaimed { reason } =>
            format!("  ✔ CodingPlan 已领取 — {}\n", reason).into(),
        Msg::CpClaimFailed { error } =>
            format!("  ✘ CodingPlan 领取失败 — {}\n", error).into(),
        Msg::CpAddedProviders { count, plural_s: _ } =>
            format!("  ✔ 已添加 {} 个 Provider：\n", count).into(),
        Msg::CpLockedJediterm { name } =>
            format!("      ✗ {}  （已锁定：需要升级套餐）\n", name).into(),
        Msg::CpLockedAnsi { name } =>
            format!("      • \x1b[9m{}\x1b[29m  （需要升级套餐）\n", name).into(),
        Msg::CpProviderRow { provider, model, default_suffix } =>
            format!("      • {}  →  {}{}\n", provider, model, default_suffix).into(),
        Msg::CpDefaultSuffix => "  （默认）".into(),
        Msg::CpVisionAuto { kind } =>
            format!("  ✔ 视觉预处理器 → {}  （自动检测）\n", kind).into(),
        Msg::CpVisionUserSupplied { kind } =>
            format!("  ✔ 视觉预处理器 → {}  （保留用户设置）\n", kind).into(),
        Msg::CpVisionCleared =>
            "  ⚠ 视觉预处理器已清除 — 当前模型列表中没有可用的 VL/OCR 模型\n".into(),
        Msg::CpModelsSkipped { reason } =>
            format!("  ✔ 模型步骤已跳过 — {}\n", reason).into(),
        Msg::CpModelsFailed { error } =>
            format!("  ✘ 模型步骤失败 — {}\n", error).into(),
        Msg::CpStatusHeader =>
            "  ✔ CodingPlan 状态：\n".into(),
        Msg::CpPlanPending { plan } =>
            format!("      套餐：{}  ·  正在激活\n", plan).into(),
        Msg::CpPlanActive { plan, expires_at, remaining_days, total_days } =>
            format!(
                "      套餐：{}  ·  到期时间 {}（剩余 {}d / 共 {}d）\n",
                plan, expires_at, remaining_days, total_days,
            ).into(),
        Msg::CpUsageLine { usage, reset_at, duration } =>
            format!("      用量：{}  ·  重置于 {}（{} 后）\n", usage, reset_at, duration).into(),
        Msg::CpWindowQuotaExhausted =>
            "      ⚠ 当前窗口配额已耗尽\n".into(),
        Msg::CpWindowQuotaHint { hint } =>
            format!("      ⚠ {}\n", hint).into(),
        Msg::CpStatusFetchSkipped { reason } =>
            format!("  ⚠ 状态获取已跳过 — {}\n", reason).into(),
        Msg::CpStatusFetchFailed { error } =>
            format!("  ⚠ 状态获取失败（非致命） — {}\n", error).into(),

        Msg::ErrUnsupportedLocale { input } =>
            format!("不支持的语言：{input}").into(),

        // ── 状态栏 ──
        Msg::StatusNoProvider =>
            "未配置 Provider · 使用 /provider 配置".into(),
        Msg::StatusUpgradeHint { version } =>
            format!("↑ {version} 可用 · 使用 /upgrade 升级").into(),
        Msg::StatusModelNotConfigured =>
            "（未配置）".into(),
        Msg::StatusClipboardImageHint =>
            "剪贴板有图片 · ctrl+v 粘贴".into(),

        // ── /status 命令主体 ──
        Msg::StatusBody { model, dir, config, tokens } =>
            format!(
                "  模型：    {}\n  目录：    {}\n  配置文件：{}\n  Token：   {}\n",
                model, dir, config, tokens,
            ).into(),
        Msg::StatusCpNotSignedIn =>
            "  CodingPlan：（未登录 — 运行 /codingplan 进行配置）\n".into(),
        Msg::StatusCpFetchFailed { error } =>
            format!("  CodingPlan：（状态获取失败 — {}）\n", error).into(),
        Msg::StatusCpNoActive =>
            "  CodingPlan：（无激活套餐 — 运行 /codingplan）\n".into(),
        Msg::StatusCpLine { plan, expires_at, remaining_days, total_days } =>
            format!(
                "  CodingPlan：{}  ·  到期 {}（{}d / 共 {}d）\n",
                plan, expires_at, remaining_days, total_days,
            ).into(),
        Msg::StatusCpUsage { usage, reset_at, seconds } =>
            format!("  用量：{}  ·  重置于 {}（{}s 后）\n", usage, reset_at, seconds).into(),
        Msg::StatusCpWindowExhausted =>
            "  ⚠ 当前窗口配额已耗尽\n".into(),
        Msg::StatusCpWindowHint { hint } =>
            format!("  ⚠ {}\n", hint).into(),
        Msg::StatusInstructionFilesHeader =>
            "  指令文件：\n".into(),
        Msg::StatusInstructionPresent { path, label } =>
            format!("    ✓ {} ({})\n", path, label).into(),
        Msg::StatusInstructionMissing { label } =>
            format!("    ✗ {} — 未找到\n", label).into(),

        // ── /login 完成提示 ──
        Msg::LoginSignedInWithCpHint { name, username } =>
            format!(
                "  已登录：{}（{}）。现在可以开始对话；运行 /codingplan 同步最新的模型权限。\n",
                name, username,
            ).into(),

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
        Msg::SessionNameEmpty =>
            "会话名不能为空".into(),
        Msg::SessionNameTooLong { max } =>
            format!("会话名过长（最多 {max} 个字符）").into(),
        Msg::SessionNameControlChars =>
            "会话名不能包含控制字符".into(),
        Msg::SessionListFailed { error } =>
            format!("列出会话失败：{error}").into(),
        Msg::SessionRenamed { old, new } =>
            format!("  已重命名：'{old}' -> '{new}'").into(),
        Msg::SessionNoneSelected =>
            "未选中会话".into(),
        Msg::SessionRenameEditing { buffer } =>
            format!("> {buffer}_  [Enter: 确认, Esc: 取消]").into(),

        // ── 目录选择器 ──
        Msg::DirCurrent => "当前".into(),
        Msg::DirNotExists { path } =>
            format!("目录已不存在：{path}").into(),
        Msg::DirChanged { path } =>
            format!("  已切换到：{path}\n").into(),
        Msg::DirNotADirectory { path } =>
            format!("不是目录：{path}").into(),

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
        Msg::IssueCreated { number, title, url } =>
            format!("  [issue] ✔ 已创建 #{number}：{title}\n  {url}\n").into(),
        Msg::IssueCreateFailed { error } =>
            format!("  [issue] ✗ 创建失败：{error}\n").into(),
        Msg::IssueRequiredField { field } =>
            format!("（必填 — 请输入 {field}，或按 Esc 取消）").into(),

        // ── 语言 ──
        Msg::LanguageSwitched { label, locale } =>
            format!("  ✓ 已切换语言为 {label}（{locale}）。\n").into(),

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
            format!("允许 {}（{}）？[Y]是 / [N]否 / [A]总是", tool, detail).into(),
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
        Msg::UpgradeManifestFetched { version } =>
            format!("  最新版本: {}\n", version).into(),
        Msg::UpgradeDownloading { pct, bytes, total } =>
            format!("  下载中 {}% ({} / {} bytes)\n", pct, bytes, total).into(),
        Msg::UpgradeVerifying =>
            "  正在校验 SHA256\n".into(),
        Msg::UpgradeReplacing =>
            "  正在替换二进制文件\n".into(),
        Msg::UpgradeDone { version, backup } =>
            format!("\n✓ 已升级到 {}（旧版本保留为 {}）\n  正在重启新版本...\n", version, backup).into(),
        Msg::UpgradeAlreadyLatest { detail } =>
            format!("  ✓ 已是最新版本，无需更新。{}\n", detail).into(),
        Msg::UpgradeFailed { error } =>
            format!("升级失败: {}", error).into(),
        Msg::UpgradeRolledBack { exe, backup } =>
            format!("\n✓ 已回滚。当前二进制: {}；另一版本保存在 {}\n  正在重启回滚版本...\n", exe, backup).into(),

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
        // /mcp login / logout
        Msg::McpOAuthLoginUsage =>
            "  用法：/mcp login <服务名>\n  示例：/mcp login github\n".into(),
        Msg::McpOAuthLogoutUsage =>
            "  用法：/mcp logout <服务名>\n  示例：/mcp logout github\n".into(),
        Msg::McpOAuthLoadConfigFailed { error } =>
            format!("  MCP OAuth 登录失败：无法加载配置：{error}\n").into(),
        Msg::McpOAuthServerNotFound { server } =>
            format!("  MCP OAuth 登录失败：配置中未找到服务 '{server}'。\n").into(),
        Msg::McpOAuthStarting { server } =>
            format!("  正在浏览器中启动 '{server}' 的 MCP OAuth 流程...\n").into(),
        Msg::McpOAuthSaved { provider, server } =>
            format!("  已保存 MCP 服务 '{server}' 的 {provider} OAuth Token。运行 /mcp reload 完成连接。\n").into(),
        Msg::McpOAuthFailed { error } =>
            format!("  MCP OAuth 失败：{error}\n").into(),
        Msg::McpOAuthTokenRemoved { server } =>
            format!("  已移除 MCP 服务 '{server}' 保存的 OAuth Token。\n").into(),
        Msg::McpOAuthNoToken { server } =>
            format!("  未找到 MCP 服务 '{server}' 保存的 OAuth Token。\n").into(),
        Msg::McpOAuthLogoutFailed { error } =>
            format!("  MCP OAuth 登出失败：{error}\n").into(),
        // MCP / LSP 服务连接反馈
        Msg::McpServerConnected { name } =>
            format!("✓ MCP 服务 '{name}' 已连接").into(),
        Msg::McpServerFailed { name, error } =>
            format!("✗ MCP 服务 '{name}' 失败：{error}").into(),
        Msg::LspServerStarted { name, ext } =>
            format!("✓ LSP 服务 '{name}' 已为 .{ext} 启动").into(),
        Msg::LspServerFailed { name, ext, error } =>
            format!("✗ LSP 服务 '{name}'（.{ext}）失败：{error}").into(),

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
        Msg::WorktreeClean => "（无变更）".into(),
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
        Msg::PluginMarketplaceCloning { url } =>
            format!("正在从 {url} 克隆 marketplace…").into(),
        Msg::PluginMarketplaceRemoved { name } =>
            format!("已移除 marketplace `{name}`").into(),
        Msg::PluginMarketplaceRemoveFailed { error } =>
            format!("移除 marketplace 失败：{error}").into(),
        Msg::PluginMarketplaceUpdating { name } =>
            format!("正在更新 marketplace `{name}`…").into(),
        Msg::PluginMarketplaceListFailed { error } =>
            format!("列出 marketplace 失败：{error}").into(),
        Msg::PluginInstalling { plugin, marketplace } =>
            format!("正在安装 `{plugin}@{marketplace}`…").into(),
        Msg::PluginUninstalled { plugin, marketplace } =>
            format!("已卸载 `{plugin}@{marketplace}`").into(),
        Msg::PluginUninstallFailed { error } =>
            format!("卸载失败：{error}").into(),
        Msg::PluginListFailed { error } =>
            format!("列出插件失败：{error}").into(),

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

        // ── OnboardingWizard ──
        Msg::OnboardingStepHeaderWelcome => "第 1/3 步 · 欢迎".into(),
        Msg::OnboardingStepHeaderLanguage => "第 2/3 步 · 语言".into(),
        Msg::OnboardingStepHeaderSetup => "第 3/3 步 · 配置".into(),
        Msg::OnboardingPanelTitle => "AtomCode".into(),
        Msg::OnboardingIntroVersionLine { v } =>
            format!("版本 {v}  ·  在终端里运行的 AI 编程代理").into(),
        Msg::OnboardingIntroBullet1 =>
            "• 多步骤 agent loop · 内置代码图工具".into(),
        Msg::OnboardingIntroBullet2 =>
            "• 兼容所有 OpenAI 风格 API".into(),
        Msg::OnboardingIntroBullet3 =>
            "• 通过 CodingPlan 获取免费额度".into(),
        Msg::OnboardingIntroPressEnter => "按 Enter 继续。".into(),
        Msg::OnboardingIntroCtrlC => "Ctrl+C 可随时退出。".into(),
        Msg::OnboardingIntroCompactTagline =>
            "在终端里运行的 AI 编程代理。".into(),
        Msg::OnboardingLanguageTitleBilingual =>
            "Choose your language / 选择语言".into(),
        Msg::OnboardingLanguagePrompt =>
            "选择界面语言。任何时候都可以用 `/language` 修改。".into(),
        Msg::OnboardingLanguageOptionAuto =>
            "自动检测 (LC_ALL / LANG)".into(),
        Msg::OnboardingLanguageOptionEn => "English".into(),
        Msg::OnboardingLanguageOptionZhCn => "简体中文 (Simplified Chinese)".into(),
        Msg::OnboardingSetupTitle => "想怎么开始？".into(),
        Msg::OnboardingNavHint =>
            "1-3 选择 · Enter 确认 · ← 返回 · Esc 跳过".into(),
        Msg::OnboardingConfirmClear =>
            "/welcome 会清屏。是否继续？[y/N]".into(),
        Msg::CmdWelcomeDescription => "重新运行 onboarding 向导".into(),
    }
}
