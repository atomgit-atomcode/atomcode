use super::messages::Msg;
use std::borrow::Cow;

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

        // ── /login（完整配置流程） ──
        Msg::CodingPlanSetupFailed { error } =>
            format!("CodingPlan 设置失败：{error}").into(),
        Msg::CpReauthAfter401 =>
            "  ⚠ 登录凭证已失效 — 正在重新登录...\n".into(),
        Msg::ChatAuthExpired =>
            "认证已过期，请执行 /login 重新登录".into(),
        Msg::NetworkConnectHint =>
            "网络连接失败。若浏览器能打开，可能是代理/防火墙差异：用 /proxy 配置代理或设置 HTTPS_PROXY，或在浏览器打开上面的登录链接完成扫码。可按 Esc 跳过，稍后 /login 重试。".into(),
        Msg::CpSetupHeader =>
            "  AtomCode CodingPlan 配置：\n\n".into(),
        Msg::CpLoggedIn { who, username, email } =>
            format!("  ✓ 已登录：{} ({}，{})\n", who, username, email).into(),
        Msg::CpStepSkipped { reason } =>
            format!("  ✓ {}\n", reason).into(),
        Msg::CpLoginFailed { error } =>
            format!("  × 登录失败 — {}\n", error).into(),
        Msg::CpClaimed { message, plan_type } =>
            format!("  ✓ CodingPlan 已领取 — {}（CodingPlan {}）\n", message, plan_type).into(),
        Msg::CpClaimSuccessFallback => "成功".into(),
        Msg::CpAlreadyClaimed { reason } =>
            format!("  ✓ CodingPlan 已领取 — {}\n", reason).into(),
        Msg::CpClaimFailed { error } =>
            format!("  × CodingPlan 套餐配置失败 — {}\n", error).into(),
        Msg::CpClaimFailedBare =>
            "  × CodingPlan 套餐配置失败\n".into(),
        Msg::CpClaimTierSucceeded { plan } =>
            format!("  ✓ {} 生效\n", plan).into(),
        Msg::CpClaimTierAlreadyHeld { plan } =>
            format!("  ✓ {} 生效\n", plan).into(),
        Msg::CpClaimTierFailed { tier, reason } =>
            format!("  × CodingPlan {} 套餐配置失败 — {}\n", tier, reason).into(),
        Msg::CpAddedProviders { accounts, models } =>
            format!("  ✓ 已添加 {} 个账号 · {} 个模型：\n", accounts, models).into(),
        Msg::CpLocked { name } =>
            // SGR 31 / 39 = 标准红前景 + 默认色重置。用标准色（不
            // 是亮色）让终端按当前主题映射 —— Solarized / Dracula /
            // 浅色模式都会落到各自的「红」上，不会被一个写死的 RGB
            // 锁住。retained 渲染器走严格 sanitizer 会把 SGR 剥光，
            // 但 `× … （需要升级成 Pro 以上套餐）` 文本本身仍能传达含义。
            format!("      \x1b[31m× {}  （需要升级成 Pro 以上套餐）\x1b[39m\n", name).into(),
        Msg::CpProviderRow { provider, model, default_suffix } =>
            format!("      • {}  ·  {}{}\n", provider, model, default_suffix).into(),
        Msg::CpDefaultSuffix => "  （默认）".into(),
        Msg::CpVisionAuto { kind } =>
            format!("  ✓ 视觉预处理器 → {}  （自动检测）\n", kind).into(),
        Msg::CpVisionUserSupplied { kind } =>
            format!("  ✓ 视觉预处理器 → {}  （保留用户设置）\n", kind).into(),
        Msg::CpVisionCleared =>
            "  ⚠ 视觉预处理器已清除 — 当前模型列表中没有可用的 VL/OCR 模型\n".into(),
        Msg::CpModelsSkipped { reason } =>
            format!("  ✓ 模型步骤已跳过 — {}\n", reason).into(),
        Msg::CpModelsFailed { error } =>
            format!("  × 模型步骤失败 — {}\n", error).into(),
        Msg::CpStatusHeader =>
            "  ✓ CodingPlan 状态：\n".into(),
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
        Msg::CpOfficialBuildRequired => Cow::Borrowed(
            "此功能需要官方 AtomCode 构建，请前往 \
             https://atomgit.com/atomgit_atomcode/atomcode/releases 下载安装。",
        ),
        Msg::CpAuthRequired => Cow::Borrowed(
            "未登录 AtomCode CodingPlan。请运行 /login 完成登录后再发送请求。",
        ),
        Msg::CpSignStaleClockSkew => Cow::Borrowed(
            "请求被服务端拒绝：签名时间戳已过期。请校准本地系统时间（NTP 同步）后重试。",
        ),
        Msg::CpSignReplayPersisted => Cow::Borrowed(
            "请求多次被识别为重放，请重新运行命令。",
        ),
        Msg::CpSignVersionTooOld => Cow::Borrowed(
            "当前 AtomCode 版本过旧，已不兼容 CodingPlan。请升级 AtomCode 后继续使用。",
        ),
        Msg::CpUpgradeRequired => Cow::Borrowed(
            "需要升级才能继续使用 CodingPlan，请前往官方发布页安装最新版 AtomCode。",
        ),

        Msg::ErrUnsupportedLocale { input } =>
            format!("不支持的语言：{input}").into(),

        // ── 状态栏 ──
        Msg::StatusNoProvider =>
            "未配置 Provider · 使用 /provider 配置".into(),
        Msg::StatusRuntimeUnavailable =>
            "Runtime 不可用 · 请重启或查看上方错误".into(),
        Msg::StatusOfficialBuildRequired =>
            "CodingPlan 需要官方构建".into(),
        Msg::StatusUpgradeHint { version } =>
            format!("↑ {version} 可用 · 使用 /upgrade 升级").into(),
        Msg::StatusUpgradeHintPm { version } =>
            format!("↑ {version} 可用 · 运行 brew upgrade atomcode 升级").into(),
        Msg::StatusModelNotConfigured =>
            "（未配置）".into(),
        Msg::StatusClipboardImageHint =>
            "剪贴板有图片 · ctrl+v / ctrl+alt+v 粘贴".into(),
        Msg::StatusClipboardImageHintSlash =>
            "剪贴板有图片 · /paste 粘贴".into(),
        Msg::StatusWebuiHint =>
            "提示：使用 /webui 在浏览器中打开 AtomCode".into(),

        // ── /status 命令主体 ──
        Msg::StatusBody { model, dir, config } =>
            format!(
                "  模型：    {}\n  目录：    {}\n  配置文件：{}\n",
                model, dir, config,
            ).into(),
        Msg::StatusLoginLoggedIn { user } =>
            format!("  登录：  {}\n", user).into(),
        Msg::StatusLoginNotSignedIn =>
            "  登录：  未登录（运行 /login）\n".into(),
        Msg::StatusCpNotSignedIn =>
            "  CodingPlan：（未登录 — 运行 /login 进行配置）\n".into(),
        Msg::StatusCpFetchFailed { error } =>
            format!("  CodingPlan：（状态获取失败 — {}）\n", error).into(),
        Msg::StatusCpAuthExpired =>
            "  CodingPlan：（登录已过期 — 运行 /login 重新登录）\n".into(),
        Msg::StatusCpNoActive =>
            "  CodingPlan：（无激活套餐 — 运行 /login）\n".into(),
        Msg::StatusCpLine { plan, expires_at, remaining_days, total_days } =>
            format!(
                "  CodingPlan：{}  ·  到期 {}（{}d / 共 {}d）\n",
                plan, expires_at, remaining_days, total_days,
            ).into(),
        Msg::StatusCpUsage { usage, reset_at, duration } =>
            format!("  用量：{}  ·  重置于 {}（{} 后）\n", usage, reset_at, duration).into(),
        Msg::StatusCpWindowExhausted =>
            "  ⚠ 当前窗口配额已耗尽\n".into(),
        Msg::StatusCpWindowHint { hint } =>
            format!("  ⚠ {}\n", hint).into(),
        Msg::StatusInstructionFilesHeader =>
            "  指令文件：\n".into(),
        Msg::StatusInstructionScopeGlobal => "用户全局".into(),
        Msg::StatusInstructionScopeProject => "项目共享".into(),
        Msg::StatusInstructionScopeUser => "用户项目覆盖".into(),
        Msg::StatusInstructionPresent { path, label, scope } =>
            format!("    ✓ {scope}（{label}）：{path}\n").into(),
        Msg::StatusInstructionMissing { path, label, scope } =>
            format!("    × {scope}（{label}）：{path} — 未找到\n").into(),
        Msg::StatusMemoryFilesHeader => "  记忆文件：\n".into(),
        Msg::StatusMemoryScopeGlobal => "用户全局".into(),
        Msg::StatusMemoryScopeProject => "项目记忆".into(),
        Msg::StatusMemoryPresent { path, scope } =>
            format!("    ✓ {scope}：{path}\n").into(),
        Msg::StatusMemoryMissing { path, scope } =>
            format!("    × {scope}：{path} — 未找到\n").into(),

        // ── 帮助 ──
        Msg::HelpAvailableCommands =>
            "  可用命令：\n".into(),
        Msg::KeybindingsHelp => r#"  键盘快捷键

  ── 输入 ──
    Enter                            发送消息
    \ 后接 Enter                     插入换行（所有终端通用）
    Shift / Alt / Ctrl+Enter         插入换行 *
    Ctrl+J                           插入换行 *
    /                                打开斜杠命令菜单
    Tab                              接受斜杠菜单、文件路径等补全
    Backspace / Ctrl+H               删除上一个字符
    Delete / Ctrl+?                  删除下一个字符
    Ctrl+W                           删除前一个单词
    Ctrl+U                           清空当前行
    Ctrl+K                           删除到行尾
    Ctrl+A / Home                    跳到行首
    Ctrl+E / End                     跳到行尾
    Left / Right                     光标左右移动

  ── 历史 ──
    Up / Down                        上一条 / 下一条输入
    Ctrl+R                           反向搜索；再次按下继续向前查找
    Right                            接受下一步建议（不会自动发送）

  ── 模式与模型 ──
    Tab / Shift+Tab                  无补全菜单时切换下一个 / 上一个执行模式
    F2 / Shift+F2                    下一个 / 上一个模型（Mac：Fn+F2 / Fn+Shift+F2）
    Ctrl+T                           切换 reasoning_effort

  ── 翻看输出 ──
    Shift+Up / Shift+Down            向上 / 向下滚动一行
    PageUp / PageDown                向上 / 向下滚动 10 行
    Alt+Up / Alt+Down                跳到上一条 / 下一条消息
    Ctrl+Up / Ctrl+Down              跳到上一条 / 下一条用户消息
    Home / End                       空输入时跳到对话顶部 / 底部
    鼠标滚轮                         滚动聊天区
    鼠标拖选                         选择文本
    Shift+鼠标拖选                   使用终端原生文本选择
    Ctrl+Shift+C                     复制 AtomCode 选区

  ── 流程控制 ──
    Esc                              清空输入 / 关闭弹层 / 取消当前操作
    Esc Esc                          空闲且输入为空时撤销上一轮
    Ctrl+C                           取消当前操作；空闲时再次按下退出
    Ctrl+O                           切换工具实时输出
    Ctrl+V / Ctrl+Alt+V              粘贴文本或图片 **

  ── 斜杠菜单 / 弹层导航 ──
    Up / Down                        移动选择
    Enter                            确认
    Esc                              取消 / 关闭弹层
    Tab                              在斜杠菜单中插入当前高亮命令
    1..9                             审批 / 交互提问中选择对应选项
    y / a / n                        审批时允许一次 / 始终允许 / 拒绝

  * 组合键需要终端能区分修饰键。目前已知支持的有：
     Kitty / WezTerm / iTerm2（启用 Report Modifiers）/
     Windows Terminal / Ghostty / Warp。其他终端（包括 macOS
     Apple Terminal、默认 xterm、GNOME Terminal、VS Code 集成
     终端）不区分 Shift+Enter 与 Enter，请用 \ + Enter。
  ** Ctrl+Alt+V 是终端拦截 Ctrl+V 时的备用键；Ctrl+Shift+V
     保留给终端原生纯文本粘贴。

  提示：输入 /help 查看完整斜杠命令列表。
"#.into(),

        // ── Provider 向导 ──
        Msg::ProviderWizardHeader =>
            "  管理 Provider：添加、编辑、删除或设置全局默认。按 Esc 取消。\n".into(),
        Msg::ProviderWizardCancelled =>
            "（已取消）".into(),
        Msg::ProviderMenuAdd => "添加".into(),
        Msg::ProviderMenuAddDesc => "新建 Provider 配置".into(),
        Msg::ProviderMenuEdit => "编辑".into(),
        Msg::ProviderMenuEditDesc => "修改已有 Provider 配置".into(),
        Msg::ProviderMenuDelete => "删除".into(),
        Msg::ProviderMenuDeleteDesc => "删除已有 Provider 配置".into(),
        Msg::ProviderMenuSetDefault => "设为全局默认".into(),
        Msg::ProviderMenuSetDefaultDesc => "设置默认 Provider，并切换当前会话".into(),
        Msg::ProviderImportPrompt =>
            "粘贴模板自动识别（curl / JSON / TOML），或直接回车手动填写：".into(),
        Msg::ProviderImportParsed { base_url, type_name, model } =>
            format!("已识别：{base_url} · {type_name} · {model}").into(),
        Msg::ProviderImportFailed =>
            "未能识别为模板，请重贴 curl / JSON / TOML，或留空回车手动填写。".into(),
        Msg::ProviderNoProviders =>
            "尚未配置任何 Provider。".into(),
        Msg::ProviderDeleteConfirm { name } =>
            format!("删除 \"{name}\"？[y/N]").into(),
        Msg::ProviderDeleted { name } =>
            format!("已删除 \"{name}\"。").into(),
        Msg::ProviderDeleteKept => "（已保留）".into(),
        Msg::ProviderDefaultSet { name } =>
            format!("默认已设为 {name}。").into(),
        Msg::ProviderAdded { name } =>
            format!("已添加账号 \"{name}\"。已进入其模型列表，按 Ctrl+A 添加模型。").into(),
        Msg::ProviderUpdated { name } =>
            format!("已更新 \"{name}\"。").into(),
        Msg::ProviderStepName => "Provider 名称？".into(),
        Msg::ProviderStepType => "类型？（openai / claude / ollama）".into(),
        Msg::ProviderStepTypeWithHint { current } =>
            format!("类型？[{current}]（openai / claude / ollama，留空保持不变）").into(),
        Msg::ProviderStepBaseUrl =>
            "Base URL？（例：https://api.deepseek.com/v1）".into(),
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
        Msg::ProviderStepContextWindow { default } =>
            format!("上下文窗口？[{default}] tokens（留空使用默认值；如 128000 / 256000 / 512000 / 1000000，或 128k / 1m）").into(),
        Msg::ProviderStepContextWindowWithHint { current } =>
            format!("上下文窗口？[{current}] tokens（留空保持不变；如 128000 / 256000 / 512000 / 1000000，或 128k / 1m）").into(),
        Msg::ProviderContextWindowInvalid =>
            "上下文窗口必须是正整数 tokens，例如 128000 或 128k。".into(),
        Msg::ProviderNameEmpty => "名称不能为空。".into(),
        Msg::ProviderBaseUrlEmpty => "Base URL 不能为空。".into(),
        Msg::ProviderUnknownType =>
            "未知类型。请选择 openai / claude / ollama。".into(),
        Msg::ProviderUnknownTypeEdit =>
            "未知类型。请选择 openai / claude / ollama 或留空。".into(),
        Msg::ProviderModelEmpty => "模型不能为空。".into(),
        Msg::ProviderEditKeep => "（保持不变）".into(),
        Msg::ProviderTypeInferred { type_name } =>
            format!("已识别类型：{type_name}").into(),
        Msg::ProviderStepNameDefault { default } =>
            format!("Provider 名称？[{default}]（留空使用此名）").into(),
        Msg::ProviderStepProgress { current, total } =>
            format!("（{current}/{total}）").into(),

        // ── Provider 面板 ──
        Msg::ProviderPanelTabAccounts => "账号".into(),
        Msg::ProviderPanelTabModels => "模型".into(),
        Msg::ProviderPanelEmptyAccounts =>
            "（尚无 Provider 账号 — 按 Ctrl+A 添加）".into(),
        Msg::ProviderPanelNoMatchingAccounts => "（无匹配的 Provider 账号）".into(),
        Msg::ProviderPanelEmptyModels =>
            "（尚无模型 — 按 Ctrl+A 添加）".into(),
        Msg::ProviderPanelNoMatchingModels => "（无匹配的模型）".into(),
        Msg::ProviderPanelLegacyBadge => "旧".into(),
        Msg::ProviderPanelDefaultBadge => "默认".into(),
        Msg::ProviderPanelModelCount { count } => format!("{count} 个模型").into(),
        Msg::ProviderPanelAddModelRow => "＋ 添加模型".into(),
        Msg::ProviderPanelAccountsHint =>
            "筛选 · ↑↓选择 · ↵模型 · Ctrl+A添加 · Ctrl+E编辑 · Ctrl+Dx2 删除 · Tab切换 · Esc关闭".into(),
        Msg::ProviderPanelModelsHint =>
            "筛选 · ↑↓选择 · ↵默认/添加 · Ctrl+A添加 · Ctrl+E编辑 · Ctrl+Dx2 删除 · Tab切换 · Esc关闭".into(),
        Msg::ProviderPanelFilteredModelsHint { account } =>
            format!("〔{account}〕· ↑↓选择 · ↵默认/添加 · Ctrl+A加模型 · Ctrl+E编辑 · Ctrl+Dx2 删除 · Tab全部 · Esc关闭").into(),
        Msg::ProviderPanelModelSaved { model } => format!("已保存模型“{model}”。").into(),
        Msg::ProviderPanelAddTitle => "【添加 Provider 账号】".into(),
        Msg::ProviderPanelEditAccountTitle { account } =>
            format!("【编辑账号 {account}】").into(),
        Msg::ProviderPanelAddModelTitle => "【添加模型】".into(),
        Msg::ProviderPanelEditModelTitle => "【编辑模型】".into(),
        Msg::ProviderPanelFieldVendor => "厂商".into(),
        Msg::ProviderPanelFieldAccount => "账号".into(),
        Msg::ProviderPanelFieldBaseUrl => "Base URL".into(),
        Msg::ProviderPanelFieldApiKey => "API 密钥".into(),
        Msg::ProviderPanelFieldModel => "模型".into(),
        Msg::ProviderPanelFieldVision => "图片输入".into(),
        Msg::ProviderPanelVisionAuto => "自动".into(),
        Msg::ProviderPanelVisionEnabled => "启用".into(),
        Msg::ProviderPanelVisionDisabled => "禁用".into(),
        Msg::ProviderPanelFieldEffort => "默认思考强度".into(),
        Msg::ProviderPanelFieldEffortLevels => "支持档位".into(),
        Msg::ProviderPanelFieldWindow => "上下文窗口".into(),
        Msg::ProviderPanelFieldMakeDefault => "设为默认".into(),
        Msg::ProviderPanelSwitchHint => "←→ 切换".into(),
        Msg::ProviderPanelEnvHint { env } => format!("留空使用 ${env}").into(),
        Msg::ProviderPanelDefaultValue => "默认".into(),
        Msg::ProviderPanelKeepOriginal => "留空保留原值".into(),
        Msg::ProviderPanelProviderFormHint =>
            "Tab 下一项  ←→ 切厂商  空格 勾选  ↵ 保存  Esc 返回".into(),
        Msg::ProviderPanelAccountFormHint => "Tab 切换  ↵ 保存  Esc 返回".into(),
        Msg::ProviderPanelModelFormHint =>
            "Tab 下一项  ←→ 切选项  空格切换  ↵ 保存  Esc 返回".into(),
        // ── Model 选择器 ──
        Msg::ModelSwitched { provider, model } =>
            format!("  当前会话已切换到 {provider} · {model}\n").into(),
        Msg::ModelSwitchedAndDefault { provider, model } =>
            format!("  已切换到 {provider} · {model}；已设为新会话默认\n").into(),

        // ── 会话选择器 ──
        Msg::SessionLoadFailed { error } =>
            format!("加载会话失败：{error}").into(),
        Msg::SessionResumedLabel { name } =>
            format!("已恢复：{name}").into(),
        Msg::SessionBusyForked { source_id, fork_id } =>
            format!(
                "最近会话（{source_id}）正在另一个窗口运行，已从其最后提交状态创建独立分支（{fork_id}）。"
            ).into(),

        // ── 待办面板 ──
        Msg::TodoPanelTitle => "待办".into(),
        Msg::TodoPanelCompleted { n } => format!("{n} 已完成").into(),
        Msg::TodoPanelMore { n } => format!("+{n} 更多…").into(),

        // ── 审批面板 ──
        Msg::ApprovalAllowOnce => "允许一次".into(),
        Msg::ApprovalAlwaysAllow { tool } => format!("本会话总是允许 {tool}").into(),
        Msg::ApprovalAlwaysAllowFolder => "本会话总是允许写入此目录".into(),
        Msg::ApprovalAlwaysAllowCommand => "本会话总是允许此命令".into(),
        Msg::ApprovalDeny => "拒绝".into(),
        Msg::ApprovalHint => "↑↓ 选择 · Enter 确认 · Esc 取消".into(),
        Msg::ApprovalHeader { tool, detail } => {
            if detail.is_empty() {
                format!("允许 {tool}？").into()
            } else {
                format!("允许 {tool}（{detail}）？").into()
            }
        }
        Msg::CredentialApprovalNote => "⚠ 可能把凭据或敏感内容发送给模型 Provider".into(),
        Msg::ToolDenied => "已拒绝".into(),
        Msg::ToolBlockedBySecurityPolicy =>
            "安全策略已阻止工具调用：凭据不能通过通用 shell 参数、临时文件或环境变量传递".into(),
        Msg::PolicyRecoveryHeader => "需要安全决策".into(),
        Msg::PolicyRecoveryQuestion => "凭据操作已被拦截，AtomCode 下一步应如何处理？".into(),
        Msg::PolicyRecoveryComplete => "我已在外部完成".into(),
        Msg::PolicyRecoveryCompleteDesc => "确认外部操作已完成，不自动调用模型".into(),
        Msg::PolicyRecoverySkip => "跳过该步骤".into(),
        Msg::PolicyRecoverySkipDesc => "跳过认证步骤并结束本次恢复，不自动调用模型".into(),
        Msg::PolicyRecoveryInstructions => "查看安全执行指引".into(),
        Msg::PolicyRecoveryInstructionsDesc => "仅展示本地固定指引和占位符".into(),
        Msg::PolicyRecoveryEnd => "结束任务".into(),
        Msg::PolicyRecoveryEndDesc => "关闭本次介入，不调用模型".into(),
        Msg::PolicyRecoverySafeInstructions => "安全手动路径：\n  1. 打开一个由你控制的独立终端。\n  2. 使用服务官方登录流程或凭据感知的专用工具。\n  3. 在该终端完成认证操作；不要把密钥粘贴到 AtomCode。\n  4. 返回这里并选择“我已在外部完成”。\nAtomCode 不会重构或展示刚才被拒绝的命令。".into(),
        Msg::PolicyRecoveryCompletedLocally => "已确认认证步骤在外部完成。为避免重新生成敏感命令，本次恢复未调用模型；你可以继续提交不涉及凭据的任务。".into(),
        Msg::PolicyRecoverySkippedLocally => "已跳过需要访问凭据的步骤。为避免重新生成敏感命令，本次恢复未调用模型；你可以继续提交其他安全任务。".into(),
        Msg::PolicyRecoverySubmitError => "无法提交安全恢复选择，请重试。".into(),

        Msg::CmdSwitchedAutoMode => "  已切换到自动模式(所有工具自动批准)。\n".into(),
        Msg::CmdSwitchedAcceptEditsMode => "  已切换到自动接受编辑模式(文件编辑免审批;bash 仍会询问)。\n".into(),

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
        Msg::SessionSaveFailed { error } =>
            format!("保存会话失败：{error}。未持久化新名称。").into(),
        Msg::SessionNoneSelected =>
            "未选中会话".into(),
        Msg::SessionPickerHint =>
            "↑↓ 移动 · Enter 打开 · Ctrl+D×2 删除 · 输入内容搜索 · Esc 取消".into(),
        Msg::SessionPickerTitle { n, total, project } =>
            format!("恢复会话（{n}/{total} · {project}）").into(),
        Msg::SessionPickerTitleBare =>
            "恢复会话".into(),
        Msg::SessionPickerEmptyProject =>
            "（此项目暂无会话）".into(),
        Msg::SessionPickerEmptyFilter =>
            "（无匹配会话）".into(),
        Msg::SessionPickerEmptyFilterQuery { query } =>
            format!("（无匹配 \"{query}\" — Backspace 清除）").into(),
        Msg::SessionDeleted { name } =>
            format!("「{name}」已删除").into(),
        Msg::SessionDeleteConfirm { name } =>
            format!("再按 Ctrl+D 确认删除「{name}」").into(),
        Msg::SessionDeleteFailed { error } =>
            format!("删除会话失败：{error}").into(),
        Msg::SessionRenameEditing { buffer } =>
            format!("> {buffer}_  [Enter: 确认, Esc: 取消]").into(),

        // ── 目录选择器 ──
        Msg::DirPickerTitle { n, total } =>
            format!("切换工作目录（{n}/{total}）").into(),
        Msg::DirPickerHint =>
            "↑↓ 移动 · Tab 补全 · Enter 进入 · 输入内容搜索/路径 · Esc 取消".into(),
        Msg::DirPickerEmptyPath { query } =>
            format!("没有匹配的历史目录「{query}」· Enter 按路径进入").into(),
        Msg::DirCurrent => "当前".into(),
        Msg::DirNotExists { path } =>
            format!("目录已不存在：{path}").into(),
        Msg::DirChanged { path } =>
            format!("  已切换到：{path}\n").into(),
        Msg::DirNotADirectory { path } =>
            format!("不是目录：{path}").into(),

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
        Msg::IdleHintCodingplan => "/login".into(),
        Msg::IdleHintCodingplanSuffix =>
            "领取免费 Token 额度".into(),
        Msg::IdleHintCodingplanFull =>
            "使用 /login 领取免费 Token 额度".into(),
        Msg::IdleHintWebui => "/webui".into(),
        Msg::IdleHintWebuiSuffix =>
            "在浏览器中同步会话".into(),
        Msg::IdleHintWebuiFull =>
            "使用 /webui 在浏览器中同步会话".into(),

        // ── 欢迎屏幕提示 ──
        Msg::WelcomeTipsHeading => "上手提示".into(),
        Msg::WelcomeTipLogin => "领取免费额度".into(),
        Msg::WelcomeTipProvider => "添加自定义模型".into(),
        Msg::WelcomeTipModel => "设置默认模型".into(),
        Msg::WelcomeTipResume => "恢复上次会话".into(),
        Msg::WelcomeTipSetup => "一键推荐配置".into(),
        Msg::WelcomeTipSkills => "浏览可用技能".into(),
        Msg::WelcomeTipPlugin => "安装技能/命令插件".into(),
        Msg::WelcomeTipWebui => "在浏览器打开同步会话".into(),
        Msg::WelcomeTipMcp => "接入 MCP 工具".into(),
        Msg::WelcomeTipPlan => "只读规划模式".into(),
        Msg::WelcomeTipSession => "管理与切换会话".into(),
        Msg::WelcomeTipLoop => "循环执行提示词".into(),
        Msg::WelcomeTipGoal => "为本次会话设定目标".into(),
        Msg::WelcomeTipInit => "扫描代码库生成 AGENTS.md".into(),
        Msg::WelcomeTipLanguage => "切换界面语言".into(),
        Msg::WelcomeTipUsage => "查看用量与额度".into(),

        // ── 斜杠命令 ──
        Msg::CmdSwitchedPlanMode =>
            "  已切换到 Plan 模式（只读探索）。\n".into(),
        Msg::CmdSwitchedBuildMode =>
            "  已切换到 Build 模式（完整执行）。\n".into(),
        Msg::CmdNewSession =>
            "  新会话已开始。\n".into(),
        Msg::CmdSessionTransitionPending =>
            "  Runtime 正在重配置；就绪前会保留当前输入。\n".into(),
        Msg::CmdSessionTransitionFailed { error } =>
            format!("会话切换失败，原会话仍可用：{error}").into(),
        Msg::CmdCapabilityReloadFailed { error } =>
            format!("Runtime 能力重载失败，原 Runtime 仍可用：{error}").into(),
        Msg::CmdNoProviders =>
            "  未配置任何 Provider。\n".into(),
        Msg::CmdSessionListLoading =>
            "  正在加载会话列表…\n".into(),
        Msg::CmdNoSessions =>
            "  未找到历史会话。请先开始一段对话。\n".into(),
        Msg::CmdUnknownCommand { name } =>
            format!("未知命令：/{name}").into(),
        Msg::CmdCustomArgRequired { name } =>
            format!("/{name} 需要提供参数。用法：/{name} <你的输入>").into(),
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
        Msg::CmdUndoDone { target, last } =>
            format!("  ↩ 已退回到第 {target} 轮之前（删除第 {target}~{last} 轮）。你的提示词已填回输入框。\n").into(),
        Msg::CmdUndoDiskWarning =>
            "  ⚠ 仅回滚了对话记忆，磁盘文件未恢复。如需还原代码，请手动处理或用 /diff 查看。\n".into(),
        Msg::CmdUndoNoTurns =>
            "  没有可撤销的轮次。\n".into(),
        Msg::CmdUndoOutOfRange { requested, available } =>
            format!("  无效的轮次 {requested}（当前共 {available} 轮）。\n").into(),
        Msg::CmdUndoBusy =>
            "  当前回合进行中，无法撤销——请先按 Esc 取消。\n".into(),
        Msg::CmdRewindBusy =>
            "  当前回合进行中，无法回退——请先按 Esc 取消。\n".into(),
        Msg::CmdRewindUnavailable => "暂时无法打开回退".into(),
        Msg::CmdUndoBadArg =>
            "  用法：/undo 或 /undo N（N 为轮次号）。\n".into(),
        Msg::CmdNoChanges =>
            "  （无变更）\n".into(),
        Msg::CmdDiffTruncated =>
            "  … diff 输出已截断\n".into(),
        Msg::CmdCheckingUpdate =>
            "  正在检查更新...\n".into(),
        Msg::CmdNoActiveProvider =>
            "未配置活跃的 Provider。使用 /provider 添加一个。".into(),
        Msg::CmdProviderUnavailable =>
            "Provider 当前不可用。请使用 /login 登录，或用 /provider 配置。".into(),
        Msg::CmdProviderUnsupportedBuild =>
            "当前构建不支持 AtomGit 官方网关。请安装官方版本，或使用 /provider 切换其他 Provider。".into(),
        Msg::CmdProviderReloading =>
            "正在切换 Provider/模型，请等待切换完成后再发送。".into(),
        Msg::SubmitHeldUntilProviderReady =>
            "  ↳ provider 尚未就绪，消息已排队，就绪后将自动发送\n".into(),
        Msg::SubmitHeldUntilLogin =>
            "  ↳ 尚未登录，消息已排队，执行 /login 登录后将自动发送\n".into(),

        // ── 审批提示 ──
        Msg::ApprovalPromptAlt { tool, detail } =>
            format!("允许 {}（{}）？[Y]是=回车 / [N]否 / [A]总是", tool, detail).into(),
        Msg::ApprovalWaitingLabel =>
            "▶ 等待审批：".into(),
        Msg::ApprovalAllow => " 允许  ".into(),
        Msg::ApprovalAlways => " 总是  ".into(),

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
        Msg::UpgradeAlreadyLatest { current, latest } =>
            format!(
                "  ✓ 已是最新版本，无需更新（当前 {}，远端最新 {}）。如需重装请加 --force。\n",
                current, latest
            ).into(),
        Msg::UpgradeFailed { error } =>
            format!("升级失败: {}", error).into(),
        Msg::UpgradeRolledBack { exe, backup } =>
            format!("\n✓ 已回滚。当前二进制: {}；另一版本保存在 {}\n  正在重启回滚版本...\n", exe, backup).into(),


        // ── /config ──
        Msg::ConfigProviderLabel { provider, path } =>
            format!("  Provider：{}\n  配置文件：{}\n\n", provider, path).into(),

        // ── /cost ──
        Msg::CostTokenReport { prompt, completion, cached, cache_rate, total } =>
            format!(
                "  提示 Token：       {}\n  补全 Token：       {}\n  缓存 Token：       {}（{}% 命中率）\n  Token 总计：       {}\n",
                prompt, completion, cached, cache_rate, total
            ).into(),
        Msg::CostUnattributed { tokens } =>
            format!("历史未归属用量\n  Token 总计：       {}", tokens).into(),

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
        Msg::InitKickoff =>
            "  正在分析项目并生成 AGENTS.md…\n".into(),

        // ── /cd ──
        Msg::CdWorkingDir { cwd } =>
            format!("  工作目录：{}\n  无最近项目。使用 `/cd <路径>` 切换。\n", cwd).into(),

        // ── /diff ──
        Msg::DiffFailed { error } =>
            format!("git diff 失败：{}", error).into(),

        // ── /upgrade ──
        Msg::UpgradePackageManaged =>
            "本版本由 HarmonyBrew 管理，请运行 `brew upgrade atomcode` 升级".into(),
        Msg::UpgradeUnknownArg { arg } =>
            format!("未知的 /upgrade 参数：{}\n  用法：/upgrade [rollback|--force]", arg).into(),

        // ── /skills ──
        Msg::SkillsNone =>
            "  没有可调用的技能。\n".into(),
        Msg::SkillsAvailable =>
            "  可用技能：\n".into(),
        Msg::SkillUnknown { name } =>
            format!("未知技能：{}（输入 /skills 查看列表）", name).into(),
        Msg::SkillsLoaded { names } =>
            format!("  已加载 skills：{}\n", names).into(),

        // ── /mcp ──
        Msg::McpReloading { count } =>
            format!("  正在重载 MCP 服务器...（{} 个已配置）\n", count).into(),
        Msg::McpConnecting =>
            "  正在连接：\n".into(),
        Msg::McpConnectingServer { name } =>
            format!("    - {}  连接中...\n", name).into(),
        Msg::McpNoServersConfigured =>
            "  未配置 MCP 服务器。\n".into(),
        Msg::McpClearedReconnecting =>
            "  已请求重载 MCP；旧 MCP 工具会先撤下，再在后台重新连接。\n".into(),
        Msg::McpClearedNoServers =>
            "  已请求重载 MCP；旧 MCP 工具会先撤下，当前没有已配置的服务器。\n".into(),
        Msg::McpToolsUsage =>
            "  用法：/mcp tools <服务器名>\n  示例：/mcp tools filesystem\n".into(),
        Msg::McpServersHeader =>
            "  MCP 服务器：\n".into(),
        Msg::McpReloadFailed { error } =>
            format!("MCP 重载失败：无法加载 .mcp.json / $ATOMCODE_HOME/mcp.json：{:#}", error).into(),
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
            format!("  已保存 MCP 服务 '{server}' 的 {provider} OAuth Token。正在重载 MCP 能力。\n").into(),
        Msg::McpOAuthFailed { error } =>
            format!("  MCP OAuth 失败：{error}\n").into(),
        Msg::McpOAuthTokenRemoved { server } =>
            format!("  已移除 MCP 服务 '{server}' 保存的 OAuth Token。\n").into(),
        Msg::McpOAuthNoToken { server } =>
            format!("  未找到 MCP 服务 '{server}' 保存的 OAuth Token。\n").into(),
        Msg::McpOAuthLogoutFailed { error } =>
            format!("  MCP OAuth 登出失败：{error}\n").into(),
        Msg::McpProjectTrusted =>
            "  已信任本项目 — 正在重连 MCP。\n".into(),
        Msg::McpProjectUntrusted =>
            "  已撤销本项目信任。\n".into(),
        Msg::McpProjectNotTrusted =>
            "  本项目未被信任。\n".into(),
        Msg::LspServerStarted { name, ext } =>
            format!("✓ LSP 服务 '{name}' 已为 .{ext} 启动").into(),
        Msg::LspServerFailed { name, ext, error } =>
            format!("× LSP 服务 '{name}'（.{ext}）失败：{error}").into(),

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

        // ── /setup ──
        Msg::SetupHeader { installed, skipped, failed, duration_ms } =>
            format!("\n✅ Setup 完成 — {} 装好, {} 跳过, {} 失败  · 耗时 {}ms\n\n", installed, skipped, failed, duration_ms).into(),
        Msg::SetupInstalledLabel =>
            "已安装:\n".into(),
        Msg::SetupSkippedLabel =>
            "\n跳过:\n".into(),
        Msg::SetupFailedLabel =>
            "\n失败:\n".into(),
        Msg::SetupInstalledRow { kind, slug, path } =>
            format!("  ✓ {}:{} → {}\n", kind, slug, path).into(),
        Msg::SetupSkippedRow { kind, slug, reason } =>
            format!("  - {}:{} ({:?})\n", kind, slug, reason).into(),
        Msg::SetupFailedRow { kind, slug, error } =>
            format!("  × {}:{} — {}\n", kind, slug, error).into(),
        Msg::CmdSetupTip =>
            // No leading emoji — U+1F4A1 has ambiguous terminal display
            // width and desynced the line's cell layout on some terminals.
            // CJK chars below have stable width-2 so they're fine.
            "提示：运行 \x1b[1;96m/setup\x1b[0m 可自动为该项目配置 hooks、skills 和 MCP。".into(),
        Msg::CmdSetupRunning =>
            "正在运行 atomcode setup...".into(),
        Msg::CmdSetupSkillsReloaded { count } =>
            format!("  🔄 Skills 已重载 — {} 个可用", count).into(),
        Msg::CmdSetupError { error } =>
            format!("setup 错误：{error}").into(),
        Msg::CmdSetupRunningSkill =>
            "  🚀 正在运行 setup skill — 分析项目并生成推荐...".into(),
        Msg::CmdSetupSkillMissing =>
            "setup skill 未找到 — 请重新运行 /setup 以重新安装".into(),

        // ── /plugin ──
        Msg::PluginUsage =>
            "用法：/plugin [marketplace add|remove|update|list | install <p>@<m> | uninstall <p>@<m> | reload | list]".into(),
        Msg::PluginMarketplaceUsage =>
            "用法：/plugin marketplace [add|remove|update|list] <参数>".into(),
        Msg::PluginInstallUsage =>
            "用法：/plugin install <插件名> 或 <插件>@<市场>".into(),
        Msg::PluginInstallNotFound { plugin } =>
            format!("未在任何市场中找到插件 `{plugin}`。使用 /plugin marketplace list 查看已注册的市场。").into(),
        Msg::PluginInstallAmbiguous { plugin } =>
            format!("插件 `{plugin}` 存在于多个市场中，请指定：").into(),
        Msg::PluginUninstallUsage =>
            "用法：/plugin uninstall <插件名> 或 <插件>@<市场>".into(),
        Msg::PluginUninstallNotFound { plugin } =>
            format!("插件 `{plugin}` 未安装。使用 /plugin list 查看已安装插件。").into(),
        Msg::PluginUninstallAmbiguous { plugin } =>
            format!("插件 `{plugin}` 从多个市场安装，请指定卸载哪一个：\n").into(),
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
        Msg::PluginAutoUpdateSkipped { detail } =>
            format!("插件市场同步已跳过（不影响对话）：{detail}").into(),
        Msg::OfflineModeActive =>
            "离线模式：已停用联网工具、遥测与自动更新。".into(),
        Msg::PluginHooksUntrusted { count, names } => format!(
            "{count} 个插件带未信任的 hook（{names}）—— 不会运行。运行 atomcode plugin trust <name> 授权。"
        ).into(),
        Msg::PluginInstalling { plugin, marketplace } =>
            format!("正在安装 `{plugin}@{marketplace}`…").into(),
        Msg::PluginInstallingByName { plugin } =>
            format!("正在安装 `{plugin}`…").into(),
        Msg::PluginAlreadyInstalled { id } =>
            format!("  插件 `{id}` 已安装。\n  PS: 如需重新安装，请先执行 `/plugin uninstall {id}`，然后再执行 `/plugin install {id}`\n").into(),
        Msg::PluginMgrBrowse => "浏览并安装".into(),
        Msg::PluginMgrAdd => "添加市场…".into(),
        Msg::PluginMgrRemove => "移除市场…".into(),
        Msg::PluginMgrInstalled { count } => format!("已安装 ({count})").into(),
        Msg::PluginMgrInstalledMark => "✓ 已安装".into(),
        Msg::PluginMgrInstalledStatus => "已安装".into(),
        Msg::PluginMgrInstallableStatus => "可以安装".into(),
        Msg::PluginMgrInstallingStatus => "安装中".into(),
        Msg::PluginMgrUpdatingStatus => "更新中".into(),
        Msg::PluginMgrHintNav => "↑/↓ 选择 · ⏎ 进入 · esc 返回".into(),
        Msg::PluginMgrHintToggle => "⏎ 安装/卸载 · esc 返回".into(),
        Msg::PluginMgrHintRemove => "⏎ 移除 · esc 返回".into(),
        Msg::PluginMgrHintUninstall => "⏎ 卸载 · esc 返回".into(),
        Msg::PluginMgrHintUrl => "⏎ 确认添加 · esc 取消".into(),
Msg::PluginMgrHintPending => "安装中，请稍候… · esc 返回".into(),
Msg::PluginMgrHintUpdating => "更新中，请稍候… · esc 返回".into(),
Msg::PluginMgrInstallingLabel => "安装中…".into(),
        Msg::PluginMgrEmptyMarketplaces => "暂无市场，请选「添加市场…」 · esc 返回".into(),
        Msg::PluginMgrEmptyPlugins => "该市场暂无插件 · esc 返回".into(),
        Msg::PluginMgrEmptyInstalled => "暂无已安装插件 · esc 返回".into(),
        Msg::PluginMgrCloning => "正在克隆市场…".into(),
        Msg::PluginMgrInstalling { plugin } => format!("正在安装 {plugin}…").into(),
        Msg::PluginMgrUpdating { plugin } => format!("正在更新 {plugin}…").into(),
 Msg::PluginMgrEscToCancel => "Esc 取消".into(),
        Msg::PluginMgrRemoveMarketplaceTitle => "  ◆ 移除市场".into(),
        Msg::PluginMgrRemoveMarketplacePrompt { name } => format!("  \x1b[33m您确定要移除插件市场 '{name}' 吗？\x1b[39m").into(),
        Msg::PluginMgrRemoveMarketplaceYes => "是，移除".into(),
        Msg::PluginMgrRemoveMarketplaceNo => "否，保留".into(),
        Msg::PluginMgrRemoveMarketplaceHint => "↑/↓ 选择 · Enter 确认 · Esc 取消".into(),
 Msg::PluginScopeUser => "为你安装（用户级）".into(),
Msg::PluginScopeUserDesc => "~/.atomcode/plugins — 所有项目可见".into(),
Msg::PluginScopeProject => "为所有协作者安装（项目级）".into(),
Msg::PluginScopeProjectDesc => ".atomcode/plugins — 通过 git 共享".into(),
Msg::PluginScopeLocal => "仅在本仓库为你安装（本地级）".into(),
Msg::PluginScopeLocalDesc => ".atomcode/plugins/local — 不提交到 git".into(),
Msg::PluginScopeHint => "↑↓ 选择范围 · Enter 确认 · Esc 返回".into(),
Msg::PluginScopeUserShort => "用户级".into(),
Msg::PluginScopeProjectShort => "项目级".into(),
Msg::PluginScopeLocalShort => "本地级".into(),
Msg::PluginActionUninstall => "卸载".into(),
Msg::PluginActionUninstallDesc => "卸载该插件所有的组件与配置".into(),
Msg::PluginActionUpdate => "更新".into(),
Msg::PluginActionUpdateDesc => "重新拉取并安装最新版本".into(),
Msg::PluginActionDisable => "禁用".into(),
Msg::PluginActionDisableDesc => "临时禁用该插件".into(),
Msg::PluginActionBack => "返回到上一级".into(),
Msg::PluginActionBackDesc => "返回已安装插件列表".into(),
        Msg::PluginUninstalled { plugin, marketplace } =>
            format!("已卸载 `{plugin}@{marketplace}`").into(),
        Msg::PluginUninstallFailed { error } =>
            format!("卸载失败：{error}").into(),
        Msg::PluginListFailed { error } =>
            format!("列出插件失败：{error}").into(),
        Msg::PluginReloadDone { skills, warnings } =>
            format!("插件已重新加载：{skills} 个 skill，{warnings} 个警告").into(),
        Msg::PluginGitNotFound =>
            "💡 当前环境未安装 git 或 git 不在 PATH 中，插件市场自动安装和自动更新已禁用。请安装 git（macOS 可执行 `xcode-select --install`，Ubuntu 可执行 `sudo apt install git`）后重启 AtomCode。".into(),
        Msg::PluginMarketplaceAdded { name, commit, count, plugins } =>
            format!(
                "✓ 已添加 marketplace `{name}`（commit {commit}，共 {count} 个插件）\n  \
                 插件：{plugins} —— 运行 /plugin install <插件名>@{name} 安装后才能使用其命令"
            ).into(),
        Msg::PluginMarketplaceUpdated { name, commit } =>
            format!("✓ marketplace `{name}` 已更新至 {commit}").into(),
        Msg::PluginInstallDone { plugin, marketplace: _, loaded: _, skipped: _, show_details_hint: _ } => {
            format!("  ⎿  ✓ Installed {plugin}. Run /reload-plugins to apply.").into()
        }
        Msg::PluginUpdateDone { plugin, marketplace: _, loaded: _, skipped: _, show_details_hint: _ } => {
            format!("  ⎿  ✓ Updated {plugin}. Run /reload-plugins to apply.").into()
        }
        Msg::SetupAutoReloaded { skills, warnings } =>
            format!("✓ Setup 完成，已自动刷新：{skills} 个 skill，{warnings} 个警告").into(),

        // ── 命令描述 ──
        Msg::CmdDescWebui => "启动浏览器 webui（子命令：stop / lan / --host <地址>）".into(),
Msg::CmdDescSetup =>
"扫描项目、安装种子文件并运行 setup skill [hooks|mcp|skills|all]".into(),
        Msg::CmdDescResume => "恢复上次会话".into(),
        Msg::CmdDescRename => "重命名当前会话".into(),
        Msg::CmdDescLogin => "使用 AtomGit OAuth 登录并领取 CodingPlan 模型".into(),
        Msg::CmdDescLogout => "退出 AtomGit 登录".into(),
        Msg::CmdDescWhoami => "显示当前登录用户".into(),
        Msg::CmdDescModel => "设置默认 Provider / 模型，并切换当前会话".into(),
        Msg::CmdDescProvider => "管理 Provider（添加、编辑、删除、设为全局默认）".into(),
        Msg::CmdDescStatus => "显示会话状态".into(),
        Msg::CmdDescConfig => "显示配置文件路径".into(),
        Msg::CmdDescReload => "从磁盘重新加载 $ATOMCODE_HOME/config.toml".into(),
        Msg::CmdDescCd => "切换工作目录并开启新建对话".into(),
Msg::CmdDescInit => "分析项目并生成 AGENTS.md".into(),
Msg::CmdDescBg => "后台会话：/bg、/bg list、/bg <N>、/bg drop <N>".into(),
Msg::CmdDescBackground => "在隔离的后台上下文中运行一次性任务（只读工具子集）".into(),
        Msg::CmdDescDiff => "显示 git diff".into(),
        Msg::CmdDescClear => "清屏".into(),
        Msg::CmdDescSession => "开始新会话（清除对话）".into(),
        Msg::CmdDescCost => "显示本会话 Token 用量".into(),
        Msg::CmdDescUsage => "显示 CodingPlan 用量（标签：当前窗口 / 总览 / 模型）".into(),
        Msg::CmdDescContext => "显示上下文预算明细".into(),
        Msg::CmdDescCompact => "压缩对话历史".into(),
        Msg::CmdDescRemember => "保存记忆（/remember --global 为全局）".into(),
        Msg::CmdDescForget => "删除匹配的记忆".into(),
        Msg::CmdDescMemory => "显示所有已保存的记忆".into(),
        Msg::CmdDescMcp => "显示 MCP 服务器状态（子命令：reload）".into(),
        Msg::CmdDescUndo => "撤销：把对话记忆回退一轮（/undo 或 /undo N）".into(),
        Msg::CmdDescRewind => "回退：把对话恢复到更早的检查点".into(),
        Msg::CmdDescWorktree => "Git 工作树隔离（create/list/done/cleanup）".into(),
        Msg::CmdDescUpgrade => "升级到最新版本（子命令：rollback）".into(),
        Msg::CmdDescPlan => "切换到 Plan 模式（只读探索）".into(),
        Msg::CmdDescBuild => "切换到 Build 模式（完整执行）".into(),
        Msg::CmdDescAuto => "切换到 Auto 模式（所有工具自动批准）".into(),
        Msg::CmdDescThink => "深度思考控制（on/off/budget N）".into(),
        Msg::CmdDescEffort => "模型推理强度控制（low / medium / high / max / auto）".into(),
        Msg::CmdDescHelp => "显示帮助".into(),
        Msg::CmdDescKeys => "显示键盘快捷键".into(),
        Msg::CmdDescLanguage => "切换显示语言".into(),
        Msg::CmdDescQuit => "退出 AtomCode".into(),
        Msg::CmdDescSkills => "浏览已加载的技能".into(),
        Msg::CmdDescPlugin => "插件市场（子命令：marketplace, install, uninstall, reload, list）".into(),
        Msg::CmdDescPaste => "从剪贴板粘贴图片（Windows 下 Ctrl+V 被终端拦截时的备用入口）".into(),
        Msg::CmdDescCopy => "复制上一条回复里的代码块，或用 /copy msg 复制整条回复（/copy、/copy N、/copy all、/copy msg）".into(),
        Msg::CopyOk { lines, chars } => format!("已复制代码块到剪贴板（{lines} 行，{chars} 字符）").into(),
        Msg::CopyOkMsg { lines, chars } => format!("已复制回复到剪贴板（{lines} 行，{chars} 字符）").into(),
        Msg::CopyNoCodeBlock => "上一条回复里没有可复制的代码块".into(),
        Msg::CopyMsgEmpty => "上一条回复为空，没有可复制的内容".into(),
        Msg::CopyBadIndex { count } => format!("没有这个代码块——上一条回复共 {count} 个（用 /copy N，范围 1..={count}）").into(),
        Msg::CopyFailed => "剪贴板不可用——复制失败".into(),
        Msg::CmdDescSave => "把当前对话导出为 markdown 文件（/save、/save [文件名]）".into(),
        Msg::SaveOk { path } => format!("对话已保存到 {path}").into(),
        Msg::SaveEmpty => "当前没有对话内容可导出".into(),
        Msg::SaveIoError { error } => format!("保存对话失败：{error}").into(),
        Msg::SaveInvalidPath { path } => format!("路径无效——目录不存在：{path}").into(),
        Msg::SaveRefuseOverwrite { path } => format!("目标已存在且非 markdown 文件，已拒绝覆盖（避免误删源码/配置）：{path}。请换个 .md 文件名或新路径。").into(),
        Msg::CodeBlockCopied => "📋 代码块已复制到剪贴板".into(),
        Msg::CmdDescGuide => "向 atomcode-guide 提问使用方法".into(),
        Msg::CmdDescView => "在浮层窗口中查看文件内容".into(),
        Msg::CmdDescApp => "通过中继将当前会话暴露给手机 App（扫码配对；/app stop 断开）".into(),
        Msg::CmdDescSync => "接入实时 webui 会话（/sync off 断开）".into(),
        Msg::CmdDescReview => "审查当前代码改动（/review · /review staged · /review <基准>）".into(),
        Msg::CmdDescGoal => "设定完成目标（自主循环直到达成）".into(),
        Msg::CmdDescProxy => "切换出站代理模式".into(),
        Msg::CmdDescTodo => "显示当前任务清单；`/todo add <任务>` 追加一条，`/todo clear` 清空".into(),
        Msg::CmdDescTeam => "显示或控制 Team Agent 进度面板".into(),
        Msg::CmdDescSchedule => "查看定时任务列表和下次运行时间".into(),
        Msg::CmdDescDesktop =>
            "打开 AtomCode 桌面端（已安装则启动，否则显示下载地址）".into(),
        Msg::DesktopOpening { name, path } =>
            format!("正在打开 {}…\n  {}\n", name, path).into(),
        Msg::DesktopNotInstalled { url } =>
            format!("未检测到 AtomCode 桌面端。下载安装：\n  {}\n", url).into(),
        Msg::DesktopLaunchFailed { path, err } =>
            format!("找到了应用但启动失败：{}\n  {}\n", err, path).into(),
        Msg::TodoNoList => "当前无任务清单（模型尚未创建 todo）。".into(),
        Msg::TodoListHeader => "当前任务清单:".into(),
        Msg::TodoAddUsage => "用法：/todo add <任务描述>".into(),
        Msg::GuideMenuHeader => "📖 AtomCode 使用指南 — 输入 /guide <问题> 提问".into(),
        Msg::GuideMenuTopics => "常用话题：".into(),
        Msg::GuideMenuGettingStarted => "怎么开始使用          首次安装、登录、配置".into(),
        Msg::GuideMenuSwitchModel => "怎么设置默认模型       /model /provider 操作".into(),
        Msg::GuideMenuMcp => "怎么用 MCP            MCP 服务器配置与管理".into(),
        Msg::GuideMenuSkills => "怎么用技能和插件       /skills /plugin 使用".into(),
        Msg::GuideMenuMemory => "怎么用记忆功能         /remember /forget /memory".into(),
        Msg::GuideMenuBackground => "怎么用后台任务         /bg 后台执行".into(),
        Msg::GuideMenuContext => "怎么管理上下文         /compact /context /cost".into(),
        Msg::GuideMenuKeybindings => "快捷键有哪些           键盘快捷键参考".into(),
        Msg::GuideMenuConfig => "怎么配置               config.toml 配置说明".into(),
        Msg::GuideMenuTip => "
  提示：输入 /guide <你的问题> 获取具体回答。
  例如：/guide 怎么设置默认模型
".into(),
        Msg::GuideMenuDocUrl => "  完整文档：https://atomcode.atomgit.com/docs/zh/".into(),
        Msg::CmdGuideInstalling => "正在安装 ask skill，请稍候...".into(),
        Msg::CmdGuideAutoInstall => "ask skill 未安装，正在自动安装 atomcode@atomcode-skills...".into(),
        Msg::CmdGuideAutoInvoke { topic } =>
            format!("ask skill 安装完成，正在回答: {}", topic).into(),
        Msg::CmdGuideSkillNotFound =>
            "安装完成但未找到 ask skill，请运行 /plugin reload 后重试".into(),
        Msg::CmdGuideInstallFailed { error } =>
            format!("安装 ask skill 失败: {}. 请手动运行 /plugin install atomcode@atomcode-skills", error).into(),
        Msg::CmdPasteNoImage => "剪贴板中没有图片。".into(),
        Msg::CmdPasteNoImageOhos => {
            "鸿蒙暂不支持读取系统剪贴板图片。请把图片存成文件，然后粘贴/输入它的绝对路径（如 /storage/.../pic.png）来添加图片。".into()
        }

        // ── reasoning effort ──
        Msg::ReasoningEffortNoEffect => "当前模型未配置 reasoning_effort 支持，请在 /provider 中启用".into(),

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
        Msg::VisionPreprocessSuccess { char_count } =>
            format!("✓ VL 识别图片成功，返回 {char_count} chars").into(),
        Msg::VisionPreprocessFailed { reason } =>
            format!("VL 预处理失败：{reason} · 本轮以纯文字继续，图片已恢复可重试").into(),
        Msg::TurnSummary { done, turn_count, tool_call_count, duration, total_tokens, cached_pct } =>
            format!(
                "✓ {done} · {turn_count} 轮 · {tool_call_count} 工具 · {duration} · {} tokens{}",
                super::fmt_tokens(total_tokens),
                cached_pct.map(|p| format!(" · {p}% cached")).unwrap_or_default(),
            ).into(),
        Msg::TurnSummaryError { turn_count, tool_call_count, duration, total_tokens, reason } => {
            let cause = reason.map(|r| format!("：{r}")).unwrap_or_default();
            format!("✗ 已中断{cause} · {turn_count} 轮 · {tool_call_count} 工具 · {duration} · {} tokens", super::fmt_tokens(total_tokens)).into()
        }
        Msg::TurnSummaryPolicyDenied { turn_count, tool_call_count, duration, total_tokens, reason } => {
            let cause = reason.map(|r| format!("：{r}")).unwrap_or_default();
            format!("✗ 安全策略已终止本回合{cause} · {turn_count} 轮 · {tool_call_count} 工具 · {duration} · {} tokens", super::fmt_tokens(total_tokens)).into()
        }
        Msg::LoginQrHeader =>
            "  登录 AtomGit — 使用微信扫描下方二维码：\n\n".into(),
        Msg::LoginUrlAfterQr =>
            "\n\n  或在浏览器打开下方链接：\n  ".into(),
        Msg::LoginNoQrNoUrl =>
            "  当前终端无法渲染二维码，\n  \
             且该平台不支持基于 URL 的登录。\n  \
             请改用支持 Unicode 的终端以显示二维码。".into(),
        Msg::LoginUrlOnly =>
            "  在浏览器中打开此链接以登录 AtomGit：\n  ".into(),
        Msg::LoginCancelHint => "\n\n  按 ESC 取消\n".into(),
        Msg::CtxUsageHeader => "上下文用量".into(),
        Msg::CtxUsageNoTurns => "（请至少完成一轮对话 — 统计在每轮结束时记录）".into(),
        Msg::CtxUsageWaiting => "（等待首轮完成 — 当前仅为部分统计）".into(),
        Msg::CtxProvider => "Provider".into(),
        Msg::CtxCtxName => "ctx".into(),
        Msg::CtxLabelSystemPrompt => "系统提示".into(),
        Msg::CtxLabelToolDefs => "工具定义".into(),
        Msg::CtxLabelColdZone => "冷区".into(),
        Msg::CtxLabelMessages => "消息".into(),
        Msg::CtxLabelFree => "空闲".into(),
        Msg::CtxMessagesInWindow { n } => format!("窗口内消息数：{n}").into(),
        Msg::CtxSystemPromptHeader => "=== 系统提示 ===".into(),
        Msg::CtxSystemPromptEmpty => "（为空 — 完成一轮对话后捕获）".into(),
        Msg::CtxTokensSuffix => "tokens".into(),
        Msg::CompactNothingShort => "（无需压缩 — 当前对话较短）\n".into(),
        Msg::CompactStarting => "（正在使用 LLM 摘要进行压缩...）\n".into(),
        Msg::CompactInterrupted => "（压缩已中断 — coding runtime 已变更或停止）\n".into(),
        Msg::CompactUnavailableDuringSync =>
            "同步模式下 /compact 暂不可用；请先执行 /sync off".into(),
        Msg::CompactUnavailableDuringResync =>
            "本地 runtime 尚未恢复最新同步会话，暂不能执行 /compact".into(),
        Msg::LocalRuntimeRestorePending =>
            "本地 runtime 正在恢复同步会话，请稍候".into(),
        Msg::LocalRuntimeRestoreTimedOut =>
            "本地 runtime 恢复超时，已重新接回 Live 同步".into(),
        Msg::CompactNothingNoSavings { before, after } =>
            format!("（无需压缩 — 压缩后不会节省 token：{} → {}）\n", before, after).into(),
        Msg::CompactDropped { messages, before, after } =>
            format!("（已压缩 — 丢弃 {} 条消息，{} → {} tokens）\n", messages, before, after).into(),
        Msg::Compacting => "正在压缩…".into(),
        Msg::CompactingSlow => "正在压缩…（较慢）".into(),
        Msg::CompactMarkDrain { messages, before, after } =>
            format!("已压缩 · 摘要 {} 条 · ~{}→~{} tok", messages, before, after).into(),
        Msg::CompactMarkStub { saved } =>
            format!("已折叠工具输出 · 节省 ~{} tok", saved).into(),
        Msg::GoalHelp =>
            "  /goal — 朝着设定的条件自主进行多轮工作。\n  \
             用法：\n  \
             \u{20}\u{20}/goal <条件>          设定新目标；智能体循环执行直到评估器判定达成\n  \
             \u{20}\u{20}/goal                 显示当前目标状态\n  \
             \u{20}\u{20}/goal status          同上\n  \
             \u{20}\u{20}/goal clear           停止当前目标（别名：stop、off、reset、none、cancel）\n  \
             \u{20}\u{20}/goal help            显示本帮助\n  \
             说明：\n  \
             \u{20}\u{20}- 每轮由一个快速模型评估；通过 ~/.atomcode/config.toml 中的 [providers] +\n  \
             \u{20}\u{20}\u{20}\u{20}evaluator_provider 配置。\n  \
             \u{20}\u{20}- 没有内置的轮次 / 时间上限——请在条件文本中自行表达预算\n  \
             \u{20}\u{20}\u{20}\u{20}（例如 \"或在 20 轮后停止\"）。Claude Code 的 /goal 也是这样工作的。\n  \
             \u{20}\u{20}- 随时可用 Esc / Ctrl+C 停止目标。\n".into(),
        Msg::GoalStatus { condition, round, mins, secs } =>
            format!("  ◎ 目标：{}\n  轮次：{}\n  已用时：{}分 {}秒\n", condition, round, mins, secs).into(),
        Msg::GoalNoActive =>
            "  当前没有进行中的目标。\n  用法：/goal <条件>   |   /goal help\n".into(),
        Msg::GoalCleared => "  已清除目标。\n".into(),

        // ── /loop ──
        Msg::LoopStatus { label, round, mins, secs } =>
            format!("  ↻ loop：{} · 第 {} 轮 · {}分 {}秒\n", label, round, mins, secs).into(),
        Msg::LoopNoActive =>
            "  当前没有进行中的 /loop。\n  用法：/loop <间隔> <命令>  或  /loop <任务>\n".into(),
        Msg::LoopCleared => "  已停止 /loop。\n".into(),
        Msg::LoopRound { round, stats } =>
            format!("⚡ loop 第 {} 轮 · {}", round, stats).into(),
        Msg::LoopStopped => "⚠ loop 已停止（达到次数上限）\n".into(),
        Msg::LoopEnded { reason } =>
            format!("  ↻ Loop 已结束：{reason}\n").into(),
        Msg::LoopNoPersistHint =>
            "  （提示：重启 / 恢复会话后该 loop 不会保留）".into(),
        Msg::CmdDescLoop =>
            "按固定间隔重复执行提示/命令，或让模型自主决定节奏".into(),
        Msg::ModelNoImageSupport { model } => format!(
            "当前模型 \"{}\" 不支持图片输入，且未配置 vision_preprocessor_provider。\
             请用 /model 切换到支持视觉的模型，或在配置中设置 vision_preprocessor_provider。",
            model
        )
        .into(),
        Msg::VisionPreprocessorUnresolvable { model, provider } => format!(
            "当前模型 \"{}\" 不支持图片输入；已配置的 vision_preprocessor_provider \"{}\" \
             无法解析（请检查名称是否与配置中的 provider/model 一致）。\
             请修正该名称，或用 /model 切换到支持视觉的模型。",
            model, provider
        )
        .into(),
        // ── --dangerously-skip-permissions / -y ──
        Msg::BypassWarningBanner =>
            "\u{26a0} --dangerously-skip-permissions 已启用：所有工具调用将自动批准（无权限提示）\n".into(),
        Msg::BypassWarningHeadless =>
            "[headless] --dangerously-skip-permissions：所有工具调用将自动批准".into(),

        Msg::AdminWarningBanner =>
            "\x1b[33m\u{26a0} 警告：正在以管理员权限运行。\n   模型可能可以访问系统文件。\n   建议改用普通权限、并在受限的工作目录中运行 AtomCode。\x1b[39m\n".into(),
        Msg::AdminWarningHeadless =>
            "[warning] 正在以管理员权限运行 — 模型可能可以访问系统文件。".into(),

        Msg::CtrlCAgainToExit => "  （再次按 Ctrl+C 退出）\n".into(),
        Msg::EscAgainToUndo => "  （再次按 Esc 打开回退选择）\n".into(),
        Msg::BashInputHint => "回车执行 bash 命令".into(),
        Msg::ShellModeHint => "! 进入 shell 模式".into(),
        Msg::PendingMessagesTitle =>
            "将在下一次工具调用后提交的消息（按 Esc 中断并立即发送）".into(),
        Msg::PendingMessagesNotSent { count } =>
            format!("运行时已停止，{count} 条待处理消息未发送").into(),
        Msg::HintMultiLineInput =>
            "  \u{24d8} 多行输入：在行尾加 `\\` 再按 Enter。\n    \
            所有终端均可用。（Shift / Alt / Ctrl + Enter 在部分终端也支持，\n    \
            取决于该终端的键盘协议 — 可以试试看。）\n\n"
                .into(),

        // ── /bg（后台会话）──
        Msg::BgHelp =>
            "  /bg                 将当前会话放到后台，打开新的前台会话\n  /bg list            列出后台会话\n  /bg <N>             恢复第 N 号后台会话\n  /bg drop <N>        丢弃第 N 号后台会话\n  /bg help            显示此帮助\n".into(),
        Msg::BgListEmpty => "  没有后台会话。\n".into(),
        Msg::BgListHeader => "  #   ID        状态       创建时间   摘要\n".into(),
        Msg::BgListRow { slot, short_id, state, age, summary } =>
            format!("  {:<3} {:<8}  {:<9}  {:<8}  {}\n", slot, short_id, state, age, summary).into(),
        Msg::BgStateRunning => "运行中".into(),
        Msg::BgStateIdle => "空闲".into(),
        Msg::BgStateDone => "已完成".into(),
        Msg::BgStateCancelled => "已取消".into(),
        Msg::BgStateError => "错误".into(),
        Msg::BgAgeNow => "刚刚".into(),
        Msg::BgAgeMinutes { n } => format!("{n} 分钟").into(),
        Msg::BgAgeHours { n } => format!("{n} 小时").into(),
        Msg::BgAgeDays { n } => format!("{n} 天").into(),
        Msg::BgSlotLimitReached { max } =>
            format!("后台槽位已达上限（{max}）").into(),
        Msg::BgBackgroundCurrent { new_id, slot, old_id, state } =>
            format!("  新前台会话 [{new_id}]\n  后台：[#{slot}] {old_id}（状态：{state}）\n").into(),
        Msg::BgInvalidSlot { slot, available } =>
            format!("无效的后台槽位 {slot}（可用：{available}）").into(),
        Msg::BgNoRuntimeClient => "后台槽位没有运行时客户端".into(),
        Msg::BgResumed { slot, short_id } =>
            format!("  已恢复后台 [#{slot}] {short_id}\n").into(),
        Msg::BgPreviousForegroundMoved { slot } =>
            format!("  原前台会话已移至 [#{slot}]\n").into(),
        Msg::BgDropped { slot, short_id } =>
            format!("  已丢弃后台 [#{slot}] {short_id}\n").into(),
        Msg::BgTaskStarted { slot, short_id } =>
            format!("  后台：[#{slot}] {short_id}（状态：运行中）\n").into(),
        Msg::BgTaskTimedOut { secs } =>
            format!("后台任务超时（{secs} 秒）。").into(),
        Msg::BgTaskError { error } =>
            format!("错误：{error}").into(),
        Msg::BgTaskCancelled => "已取消。".into(),
        Msg::BgTaskNoSummary => "任务完成（无摘要文本）。".into(),
        // ── CLI atomcode --help i18n ──
        Msg::CliAbout => "终端中的 AI 编程助手".into(),
        Msg::CliAboutLogin => "通过 AtomGit OAuth 登录并领取 CodingPlan 模型".into(),
        Msg::CliAboutLogout => "退出 AtomCode 登录".into(),
        Msg::CliAboutStatus => "查看当前登录状态".into(),
        Msg::CliAboutUpgrade => "就地升级 atomcode 到最新发布版本".into(),
        Msg::CliAboutRollback => "回退到上一个版本（与 .bak 交换）".into(),
        Msg::CliAboutMcp => "管理 .mcp.json 中的 MCP 服务器配置".into(),
        Msg::CliAboutDaemon => "启动用于 IDE 集成的 HTTP 守护进程".into(),
        Msg::CliAboutWebui => "启动本地浏览器 webui".into(),
        Msg::CliAboutTelemetry => "遥测控制".into(),
        Msg::CliAboutPlugin => "管理技能/命令插件".into(),
        Msg::CliAboutUninstall => "卸载 AtomCode：移除二进制文件、PATH 编辑和数据".into(),
        Msg::CliAboutSetup => "安装种子文件（技能/命令/钩子/MCP）到 ~/.atomcode/".into(),
        Msg::CliAboutHooks => "管理钩子（列表、测试、启用/禁用）".into(),
        Msg::CliAboutHooksList => "列出所有已加载钩子及其状态".into(),
        Msg::CliAboutHooksTest => "按名称测试指定钩子".into(),
        Msg::CliAboutHooksPaths => "显示钩子配置路径".into(),
        Msg::CliAboutPluginMarketplace => "市场注册操作".into(),
        Msg::CliAboutPluginInstall => "从已注册的市场安装插件".into(),
        Msg::CliAboutPluginUninstall => "卸载已安装的插件".into(),
        Msg::CliAboutPluginList => "列出已安装的插件".into(),
        Msg::CliAboutMarketplaceAdd => "克隆市场 git 仓库并在本地注册".into(),
        Msg::CliAboutMarketplaceRemove => "删除已注册的市场".into(),
        Msg::CliAboutMarketplaceUpdate => "重新拉取已注册的市场并刷新插件索引".into(),
        Msg::CliAboutMarketplaceList => "列出已注册的市场".into(),
        Msg::CliAboutMcpAdd => "添加或替换 stdio MCP 服务器".into(),
        Msg::CliAboutMcpAddGithubOauth => "使用 OAuth 添加 GitHub 远程 MCP 服务器".into(),
        Msg::CliAboutMcpLogin => "完成远程 MCP 服务器的 OAuth 登录".into(),
        Msg::CliAboutMcpLogout => "删除远程 MCP 服务器的已保存 OAuth 凭证".into(),
        Msg::CliAboutTelemetryStatus => "查看当前遥测状态和队列统计".into(),
        Msg::CliAboutTelemetryEnable => "启用遥测".into(),
        Msg::CliAboutTelemetryDisable => "禁用遥测".into(),
        Msg::CliAboutTelemetryDump => "打印待发送的队列事件".into(),
        Msg::CliAboutTelemetryClear => "清除队列事件".into(),
        Msg::CliHelpContinue => "继续上一次会话而不是启动新会话".into(),
        Msg::CliHelpProvider => "指定使用的 Provider（覆盖配置默认值）".into(),
        Msg::CliHelpModel => "指定使用的模型（覆盖配置中的 Provider 模型）".into(),
        Msg::CliHelpLang => "设置界面语言（如 en、zh-CN、zh）".into(),
        Msg::CliHelpConfig => "配置文件路径".into(),
        Msg::CliHelpDir => "工作目录（默认为当前目录）".into(),
        Msg::CliHelpPrompt => "在无头（非交互）模式下运行的提示".into(),
        Msg::CliHelpPromptFile => "从文件读取提示".into(),
        Msg::CliHelpVerbose => "在 stderr 上显示工具调用、token 用量和回合摘要".into(),
        Msg::CliHelpDev => "禁用本次启动的自动更新".into(),
        Msg::CliHelpNoTelemetry => "禁用本次调用的遥测".into(),
        Msg::CliHelpDangerouslySkipPermissions => "跳过所有权限提示 -- 自动批准每个工具调用".into(),
        Msg::CliHelpForce => "即使已是最新版本也重新安装".into(),
        Msg::CliHelpPortDaemon => "监听端口（默认：13456）".into(),
        Msg::CliHelpClient => "遥测客户端标识".into(),
        Msg::CliHelpIdleTimeout => "空闲关闭超时（秒）；0 禁用".into(),
        Msg::CliHelpPortWebui => "端口（默认：13457）".into(),
        Msg::CliHelpHost => "绑定地址（默认：127.0.0.1）".into(),
        Msg::CliHelpUninstallYes => "跳过提示；使用每组的默认决定".into(),
        Msg::CliHelpUninstallPurge => "完全清除 ~/.atomcode/".into(),
        Msg::CliHelpUninstallKeepData => "完全保留 ~/.atomcode/".into(),
        Msg::CliHelpUninstallDryRun => "仅打印计划；不执行操作".into(),
        Msg::CliHelpMcpGlobal => "写入 ~/.atomcode/mcp.json 而非 <dir>/.mcp.json".into(),
        Msg::CliHelpMcpDir => "项目 .mcp.json 的目录".into(),
        Msg::CliHelpMcpName => "服务器键名".into(),
        Msg::CliHelpHooksTestName => "要测试的钩子名称".into(),
        Msg::CliHelpPluginSpec => "如 plugin@marketplace".into(),
        Msg::CliHelpMarketplaceUrl => "市场仓库的 Git URL".into(),
        Msg::CliHelpMarketplaceName => "市场名称".into(),
        Msg::CliAboutHelp => "打印帮助信息".into(),
        Msg::CliHelpMcpCommand => "可执行文件及参数".into(),

        // ── /usage modal ──
        Msg::UsageTabCurrent => "当前窗口".into(),
        Msg::UsageTabOverview => "总览".into(),
        Msg::UsageTabModels => "模型".into(),
        Msg::UsageCurrentTitle => "速率限制窗口".into(),
        Msg::UsageResetsIn { hms } => format!("剩余重置时间 {hms}").into(),
        Msg::UsageWindowHours { hours } => format!("{hours} 小时滚动窗口").into(),
        Msg::UsageWindowUnavailable => "窗口数据不可用".into(),
        Msg::UsageStatFavorite => "最常用模型".into(),
        Msg::UsageStatTotal => "总 Token 数".into(),
        Msg::UsageStatRequests => "请求次数".into(),
        Msg::UsageStatActiveDays => "活跃天数".into(),
        Msg::UsageStatMostActive => "最活跃日期".into(),
        Msg::UsageStatLongestStreak => "最长连续天数".into(),
        Msg::UsageStatCurrentStreak => "当前连续天数".into(),
        Msg::UsageHeatLess => "少".into(),
        Msg::UsageHeatMore => "多".into(),
        Msg::UsageModelsTitle => "各模型用量".into(),
        Msg::UsageNoData => "暂无用量数据".into(),
        Msg::UsageFooterHint => "← / → 或 Tab 切换 · Ctrl+S 复制 · Esc 关闭".into(),
        Msg::UsageFetchFailed { error } => format!("加载用量失败：{error}").into(),
        Msg::UsagePlanTitle => "计划".into(),
        Msg::UsagePlanActive => "生效中".into(),
        Msg::UsagePlanExpired => "已过期".into(),
        Msg::UsagePlanClaimedExpires { claimed, expires } =>
            format!("领取 {claimed} · 到期 {expires}").into(),
        Msg::UsagePlanRemaining { remaining, total } =>
            format!("剩余 {remaining}/{total} 天").into(),
        Msg::UsageCopied => "已复制到剪贴板".into(),
        Msg::UsageCodingPlanOnly =>
            "使用情况仅 CodingPlan 可用 — 请先 /login。".into(),

        // ── CodingRuntime provider init ──
        Msg::ProviderInitFailed { detail } =>
            format!("模型初始化失败：{detail}").into(),
        Msg::ProviderInitNeedsLogin =>
            "尚未登录，模型暂不可用；运行 /login 后可继续对话。".into(),
        Msg::ProviderInitSourceBuild =>
            "当前为源码构建，无法使用 AtomGit 免费网关。请用 /provider 配置一个自带 api_key \
             的模型（如 DeepSeek 官方 / GLM / OpenAI），或改用官方发布版。".into(),
        Msg::GatewayAuthUnavailable { base_url } =>
            format!(
                "provider base_url「{base_url}」是 AtomGit 网关，当前构建无法对其鉴权。请使用官方版本，\
                 或将该 provider 指向带 api_key 的标准 OpenAI 兼容端点。"
            ).into(),
        Msg::StreamStalled => "按 esc 可取消".into(),
        Msg::StreamRecoveryRunning { attempt, max_attempts } => format!(
            "流响应超时，正在从已保存进度安全续接（{attempt}/{max_attempts}）…"
        )
        .into(),
        Msg::StreamRecoverySucceeded => "✓ 已从流中断处恢复".into(),
        Msg::ConhostScrollHint =>
            "提示：经典 Windows 控制台功能受限——任务执行中无法上滚查看历史，字符与吉祥物也会降级显示。\
             换用 \x1b[1;96mWindows Terminal\x1b[0m 体验更佳。"
                .into(),
    }
}

#[cfg(test)]
mod codingplan_crypto_tests {
    use super::*;
    use crate::i18n::Msg;

    #[test]
    fn zh_official_build_required_mentions_official_and_releases() {
        let s = zh_cn(Msg::CpOfficialBuildRequired);
        assert!(s.contains("官方"));
        assert!(s.contains("releases") || s.contains("发布"));
    }

    #[test]
    fn zh_stale_clock_mentions_time() {
        let s = zh_cn(Msg::CpSignStaleClockSkew);
        assert!(s.contains("时间") || s.contains("时钟"));
    }

    #[test]
    fn zh_replay_persisted_is_non_empty() {
        let s = zh_cn(Msg::CpSignReplayPersisted);
        assert!(!s.is_empty());
    }

    #[test]
    fn zh_version_too_old_mentions_upgrade() {
        let s = zh_cn(Msg::CpSignVersionTooOld);
        assert!(s.contains("升级") || s.contains("更新"));
    }

    #[test]
    fn zh_upgrade_required_is_non_empty() {
        let s = zh_cn(Msg::CpUpgradeRequired);
        assert!(!s.is_empty());
    }

    #[test]
    fn zh_conhost_scroll_hint_recommends_windows_terminal() {
        let s = zh_cn(Msg::ConhostScrollHint);
        assert!(s.contains("Windows Terminal"));
        assert!(s.contains("滚"));
    }
}
