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
    }
}
