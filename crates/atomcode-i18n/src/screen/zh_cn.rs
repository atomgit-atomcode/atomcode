//! 屏幕的中文表。
//!
//! 这些句子原本写死在 `atomcode-tui` 里,搬过来时**逐字未改**:这一趟做的是「加一门
//! 语言」,不是「改措辞」。改写会连带动 golden 帧,把一次可审的迁移变成一次看不出
//! 哪里是迁移、哪里是新写的 diff。要调措辞,另起一条。

use super::messages::Msg;
use std::borrow::Cow;

pub(super) fn zh_cn(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        // ── 命令分发 ──
        Msg::CmdNoSuch { name } => {
            format!("没有 /{name} 这条命令,输入 /help 看有哪些").into()
        }
        Msg::CmdNoSuchDidYouMean { name, near } => format!("没有 /{name};你是指 {near}?").into(),

        // ── 布局错误 ──
        Msg::LayoutNoSuchModule { name, available } => {
            format!("没有叫 `{name}` 的模块;现有的是:{available}").into()
        }
        Msg::LayoutNotOnScreen { module } => format!("`{module}` 本来就不在屏幕上").into(),
        Msg::LayoutAlreadyOnScreen { module } => format!("`{module}` 已经在屏幕上了").into(),
        Msg::LayoutDrawnTwice { module } => {
            format!("`{module}` 被写了两遍:既在流尾部、又是独立面板,会被画两次").into()
        }

        // ── 当下 ──
        Msg::MomentQuitAgain => "再按 Ctrl+C 退出".into(),

        // ── 浮层 ──
        Msg::OverlayEmptyFile => "  (空文件)".into(),
        Msg::OverlayFilterHint => "输入以筛选".into(),
        Msg::OverlayNoMatch => "  没有匹配的".into(),

        // ── 补全菜单与策略介入 ──
        Msg::MenuFolder => "目录".into(),

        // ── 模型提供方面板 ──

        // ── 设置何时生效 ──
        Msg::AppliesImmediately => "立即".into(),
        Msg::AppliesNextTurn => "下一轮".into(),
        Msg::AppliesReload => "重新加载".into(),
        Msg::AppliesReprepare => "重建能力".into(),
        Msg::AppliesRestart => "重启后".into(),

        // ── 公共控件 ──
        Msg::ListEmpty => "（空）".into(),
        Msg::ListMore { count } => format!("  …还有 {count} 项").into(),

        // ── 时间与时长 ──
        Msg::LastedSeconds { s } => format!("{s} 秒").into(),
        Msg::LastedMinutes { m } => format!("{m} 分").into(),
        Msg::LastedMinutesSeconds { m, s } => format!("{m} 分 {s} 秒").into(),
        Msg::LastedHours { h } => format!("{h} 小时").into(),
        Msg::LastedHoursMinutes { h, m } => format!("{h} 小时 {m} 分").into(),

        // ── 工具树 ──
        Msg::ToolsStateOn => "模型能调".into(),
        Msg::ToolsStateOff => "本次会话关掉的".into(),
        Msg::ToolsStateExcluded => "配置排除的".into(),
        Msg::ToolsExcludedByConfig { name } => {
            format!("`{name}` 是这棵树的配置排除掉的 —— 改配置才能放回来").into()
        }
        Msg::ToolsTurningOff { name } => format!("正在关掉 {name}…").into(),
        Msg::ToolsTurningOn { name } => format!("正在放回 {name}…").into(),

        // ── 向导 ──
        Msg::WizardBack => " · ← 上一步".into(),
        Msg::WizardNoteKeys { back } => format!("enter 继续{back} · esc 放弃").into(),
        Msg::WizardChooseKeys { back } => format!("↑↓ 选 · enter 确定{back} · esc 放弃").into(),
        Msg::WizardTypeKeys { back } => format!("enter 确定{back} · esc 放弃").into(),
        Msg::WizardWaitSkippableKeys => "enter 跳过 · esc 放弃".into(),
        Msg::WizardWaitKeys => "esc 放弃".into(),
        Msg::WizardWaiting { spinner } => format!("  {spinner} 等待中").into(),

        // ── 模型向人提问 ──
        Msg::AskStepLimitQuestion => "这一轮已经跑了很多步。继续吗?".into(),
        Msg::AskStepLimitTitle => "步数上限".into(),
        Msg::AskTruncatedQuestion => "回答一直被截断,自动接续已经用尽。继续吗?".into(),
        Msg::AskTruncatedTitle => "输出截断".into(),
        Msg::AskContinue => "继续".into(),
        Msg::AskStop => "停下".into(),
        Msg::AskYes => "好".into(),
        Msg::AskNo => "不了".into(),
        Msg::AskAlwaysAllow => "总是允许".into(),
        Msg::AskMemberRequests { name } => format!("成员 {name} 请求 ").into(),
        Msg::AskLinesChars { lines, chars } => format!("{lines} 行 · {chars} 字").into(),

        // ── 提问面板 ──
        Msg::AskLegendChoose => "选择".into(),
        Msg::AskLegendConfirm => "确认".into(),
        Msg::AskFromMember { who } => format!("来自成员 {who}").into(),
        Msg::AskGrantWholeTool => "这个工具的全部调用".into(),
        Msg::AskGrantOnly { what } => format!("仅限 {what}").into(),
        Msg::AskTypeSomething => "自己输入…".into(),
        Msg::AskChatInstead => "改为直接对话".into(),
        Msg::AskSubmit => "提交".into(),
        Msg::AskNext => "下一题".into(),
        Msg::AskReviewTitle => "核对你的回答".into(),
        Msg::AskReviewReady => "确认提交这些回答吗？".into(),
        Msg::AskReviewSend => "提交回答".into(),
        Msg::AskReviewCancel => "取消".into(),
        Msg::AskUnanswered => "（未回答）".into(),
        Msg::AskLegendToggle => "勾选".into(),
        Msg::AskLegendSwitch => "切换题目".into(),
        Msg::AskLegendCaret => "移动光标".into(),

        // ── 输入行 ──
        Msg::InputAnswerKeys => "enter 送出 · esc 不给".into(),
        Msg::InputHistoryNth { nth, total } => format!("历史 {nth}/{total}").into(),
        Msg::InputClipboardImage => "剪贴板有图片 · ctrl+v 粘贴".into(),
        Msg::InputClipboardImageAltOrCommand => "剪贴板有图片 · ctrl+alt+v 或 /paste 粘贴".into(),
        Msg::InputSearchNth { query, nth, total } =>
            format!("搜索 '{query}' {nth}/{total}").into(),
        Msg::InputSearchNone { query } => format!("搜索 '{query}' 无匹配").into(),
        Msg::ComposerInterrupted => "已中断 · 接下来做什么？".into(),

        // ── 状态栏 ──
        Msg::StatusMember { name } => format!("成员 {name}").into(),
        Msg::StatusStopping => "停止中".into(),
        Msg::StatusBackground { running, waiting } => match waiting {
            0 => format!("后台 {running}").into(),
            waiting => format!("后台 {running} · {waiting} 等你").into(),
        },
        Msg::StatusGoal => "目标".into(),
        Msg::StatusLoop => "循环".into(),
        Msg::StatusRoundsHeld { kind, rounds, why } => {
            format!("{kind} 第 {rounds} 轮 · 停着:{why}").into()
        }
        Msg::StatusRounds { kind, rounds } => format!("{kind} 第 {rounds} 轮").into(),

        // ── 团队面板 ──
        Msg::TeamHeaderFocused { count } => {
            format!("团队 · {count} 名成员 · ↑↓ 选 · Enter 切换 · Esc 返回").into()
        }
        Msg::TeamHeader { count } => format!("团队 · {count} 名成员 · Tab 切换查看").into(),
        Msg::TeamLead => "主".into(),
        Msg::TeamViewing => " 正在看".into(),
        Msg::TeamWorkingRound { round } => format!("第 {round} 轮").into(),

        // ── 待办折叠块 ──
        Msg::TodoCounts {
            completed,
            in_progress,
            open,
        } => format!("({completed} 已完成, {in_progress} 进行中, {open} 待办)").into(),

        // ── 工具面板 ──
        Msg::ToolsPanelCounts { on, off } => format!("{on} 个能调 · {off} 个关掉的").into(),
        Msg::ToolsPanelNoneMounted => "  这棵树一个工具都没挂".into(),
        Msg::ToolsPanelNoMatch => "  没有匹配的工具".into(),
        Msg::ToolsPanelStopWaiting => "不等了".into(),
        Msg::ToolsLegendChoose => "选择".into(),
        Msg::ToolsLegendToggle => "开 / 关".into(),
        Msg::ToolsLegendTyping => "打字".into(),
        Msg::ToolsLegendFilter => "筛".into(),
        Msg::ToolsLegendClose => "收起".into(),

        Msg::RewindPanelAbout => "  把代码和 / 或对话，回到这一句之前…".into(),
        Msg::RewindPanelReading => "正在读这次会话走过的回合…".into(),
        Msg::RewindPanelGoing { turn } => format!("正在回到第 {turn} 回合之前…").into(),
        Msg::RewindPanelNoPoints => "  还没有能回到的回合".into(),
        Msg::RewindPanelStopWaiting => "不看了".into(),
        Msg::RewindPanelCurrent => "（当前）".into(),
        Msg::RewindPanelNoCodeChanges => "没有代码改动".into(),
        Msg::RewindPanelFiles { files } => format!("{files} 个文件").into(),
        Msg::RewindPanelScopeAsk => "  回到那儿，把什么一起带回去？".into(),
        Msg::RewindCodeNotEnabled => "设置 ATOMCODE_CODE_REWIND=1 开启工作区回退".into(),
        Msg::RewindCodeNoSession => "这次会话不落盘，工作区回不去".into(),
        Msg::RewindCodeFailed { why } => format!("工作区回不去：{why}").into(),
        Msg::RewindPanelTurnNoFiles => "这一回合没改过文件，能回的只有对话".into(),
        Msg::RewindLegendChoose => "挑一个".into(),
        Msg::RewindLegendContinue => "下一步".into(),
        Msg::RewindLegendGo => "就回这儿".into(),
        Msg::RewindLegendBack => "回上一步".into(),
        Msg::RewindLegendClose => "算了".into(),
        Msg::RewindPointsUnreadable { why } => format!("读不到回合：{why}").into(),
        Msg::RewindFailed { why } => format!("没回得去：{why}").into(),
        Msg::NoRewindPanel => "这个屏幕没有回退面板:启动器没有提供 `tui-panel-rewind`".into(),
        Msg::NoResumeStore => "这个屏幕删不掉会话:启动器没有提供 `tui-resume-store`".into(),
        Msg::NoPlaces => "这个屏幕存不了书签:启动器没有提供 `tui-places`".into(),
        Msg::CdBookmarked => "标过的".into(),
        Msg::CdRecent => "最近在这儿干过活".into(),
        Msg::CdPinned { dir } => format!("{dir} 已标上——`/cd` 会先给它").into(),
        Msg::CdUnpinned { dir } => format!("{dir} 的标记取消了").into(),
        Msg::ResumeDeleteArmed => "再按一次 Delete 删掉它".into(),
        Msg::ResumePreviewWaiting => "正在读它最后聊了什么…".into(),
        Msg::ResumeDeleted { id } => format!("会话 {id} 已删除").into(),
        Msg::ResumeDeleteFailed { why } => format!("没删掉:{why}").into(),
        Msg::NoResumePanel => "这个屏幕没有恢复面板:启动器没有提供 `tui-panel-resume`".into(),
        Msg::NoRewind => "这个屏幕回不了会话:启动器没有提供 `tui-rewind`".into(),
        Msg::ScreenNotConnectedRewind => "屏上没有 agent，没有回合可回".into(),

        // ── 对话里的旁注 ──
        Msg::TranscriptCompacted { through } => {
            format!("已把这里之前的对话压成一段摘要(到 #{through})").into()
        }
        Msg::TranscriptShortened { count } => {
            format!("模型看到的 {count} 处工具输出被就地换短了;这里显示的仍是原文").into()
        }
        Msg::TranscriptDropped { through } => {
            format!("到 #{through} 为止的工具结果没有再发给模型").into()
        }
        Msg::TranscriptRateLimited { until } => format!("被限速,等到 {until}").into(),
        Msg::TranscriptMemberEnded => "这个成员已经结束,不会再说话了".into(),

        // ── what each command is for (`commands.rs`) ──
        Msg::CmdAboutQuit => "退出".into(),
        Msg::CmdAboutReasoning => "思考:一行、全文、收起,循环".into(),
        Msg::CmdAboutTools => "工具输出:全部、单个摘要、成组摘要;不带参数则循环".into(),
        Msg::CmdAboutShowInject => "环境注入:收起、只留标签、全文,循环;不带名字则全部".into(),
        Msg::CmdAboutMouse => "把鼠标交还终端,或收回来".into(),
        Msg::CmdAboutKeys => "列出快捷键".into(),
        Msg::CmdAboutTodo => "展开或折叠计划清单".into(),
        Msg::CmdAboutTeam => "展开或折叠团队面板".into(),
        Msg::CmdAboutPaste => "把剪贴板(或一个文件)的内容放进输入框;Ctrl+V 被终端或系统拦下时用它".into(),
        Msg::CmdAboutConfig => "拉出设置面板:搜索、改值;esc 关".into(),
        Msg::CmdAboutProviderPanel => "拉出 provider 面板:账号与模型,增改删;⏎ 换过去,esc 关".into(),
        Msg::CmdAboutCopy => "复制模型最后一条回复里的代码块;N 指定第几块,all 全要".into(),
        Msg::CmdAboutSave => "把这段对话存成 markdown".into(),
        Msg::CmdAboutView => "开一个只读浮层看文件;不花一个回合,也不进对话".into(),
        Msg::CmdAboutCompact => "压缩历史,给上下文腾地方".into(),
        Msg::CmdAboutCancelAll => "停下这个会话与每个团队成员正在跑的回合;成员留在团队里".into(),
        Msg::CmdAboutContext => "这次会话用掉了多少；prompt 看它跑在哪份系统提示词上".into(),
        Msg::CmdAboutAgents => "这个会话底下有过的 agent:主与每个成员,含已停的;选一个切过去看它的对话".into(),
        Msg::CmdAboutTranscript => "把对话按模型看到的样子列出来".into(),
        Msg::CmdAboutClear => "开一个新会话:这段对话放下,换一条干净的".into(),
        Msg::CmdAboutSession => "开一个新会话(等于 /clear)".into(),
        Msg::CmdAboutResume => "回到一个存下的会话;不带 id 则挑一个".into(),
        Msg::CmdAboutEffort => "改这个会话的思考强度(与模型无关)".into(),
        Msg::CmdAboutUndo => "撤回最后一句话(或某一回合)及其后的一切,那句话放回输入框".into(),
        Msg::CmdAboutRewind => "回到某一回合之前:对话、工作区或两者;不带参数拉起面板,双击 Esc 也拉它".into(),
        Msg::CmdAboutModel => "这个会话从现在起用哪个模型;不带 id 则挑一个".into(),
        Msg::CmdAboutAutonomy => "现在有没有在自己干(goal / loop),跑到第几轮、用了多久".into(),
        Msg::CmdAboutRename => "给这个会话改个名字".into(),
        Msg::CmdAboutDiff => "这个会话把工作区改成了什么样;不带文件则列出改过的文件,选一个看它的改动".into(),
        Msg::CmdAboutMode => "改要不要问:plan 只看不动、ask 动手前问、edits 改文件不问、auto 全不问;不带参数则说现在是哪个".into(),
        Msg::CmdAboutCd => "换到另一个目录干活(会开一条新会话);pin / unpin 标记常去的地方".into(),
        Msg::CmdAboutPlan => "只看不动(等于 /mode plan)".into(),
        Msg::CmdAboutBuild => "动手前问一句(等于 /mode ask)".into(),
        Msg::CmdAboutAuto => "全不问(等于 /mode auto)".into(),
        Msg::CmdAboutStatus => "这次会话现在是什么状况:模型、模式、在哪、跑到第几回合".into(),
        Msg::CmdAboutCost => "这次会话用掉多少 token(等于 /context)".into(),
        Msg::CmdAboutUsage => "账号还剩多少额度,哪个窗口用完了、什么时候回来".into(),
        Msg::CmdAboutMcp => "MCP 服务器的状态;tools 列某个服务器挂上来的工具;withdraw 立刻撤下全部 MCP 工具".into(),
        Msg::CmdAboutLanguage => "模型用哪种语言回答;不带参数则说现在是哪个,以及可选哪些".into(),
        Msg::CmdAboutReload => "重新读取 skills、MCP 与配置,会话不变".into(),
        Msg::CmdAboutLogout => "把凭据拿出进程;会话留着".into(),
        Msg::CmdAboutLogin => "用现在配置的凭据重新登录".into(),
        Msg::CmdAboutWhoami => "现在是谁登录着".into(),
        Msg::CmdAboutThink => "要不要思考(与 /effort「思考多狠」是两个旋钮);不带参数则说现在是哪个".into(),
        Msg::CmdAboutLook => "把屏幕切到那个 agent".into(),
        Msg::CmdAboutHelp => "列出所有命令".into(),

        // ── what a command takes, as it is shown after the name (`commands.rs`) ──
        Msg::CmdTakesPath => "[路径]".into(),
        Msg::CmdTakesPathRequired => "<路径>".into(),
        Msg::CmdTakesFilename => "[文件名]".into(),
        Msg::CmdTakesFile => "[文件]".into(),
        Msg::CmdTakesSessionId => "[会话 id]".into(),
        Msg::CmdTakesSessionIdRequired => "<会话 id>".into(),
        Msg::CmdTakesTurn => "[回合]".into(),
        Msg::CmdTakesTurnScope => "[回合 [对话|代码|全部]]".into(),
        Msg::CmdTakesModelId => "[模型 id]".into(),
        Msg::CmdTakesName => "<名字>".into(),
        Msg::CmdTakesDirectory => "<目录 | pin | unpin>".into(),
        Msg::CmdTakesMcp => "[tools <服务器>|withdraw]".into(),
        Msg::CmdTakesLanguage => "[语言]".into(),

        // ── when the host refuses (`commands.rs`) ──
        Msg::HostBusy { reason } => format!("现在不行:{reason}").into(),
        Msg::HostNotFound => "找不到:会话已经换过,或者没有这个会话".into(),
        Msg::HostSessionInUse { id } => format!("会话 {id} 正在别处用着").into(),
        Msg::HostUnavailable => "宿主现在不可用".into(),
        Msg::HostStale => "对话在这之后又有了新的一轮,屏幕还没跟上;等它画出来再试一次".into(),
        Msg::HostNoProvider { reason } => format!("没有可用的模型:{reason}").into(),
        Msg::HostSaidSomethingElse { reply } => format!("宿主答了别的:{reply}").into(),

        // ── the screen's own commands (`commands.rs`) ──
        Msg::NoClipboard => "这块屏幕没有剪贴板".into(),
        Msg::NoAgent => "这块屏幕没接上 agent".into(),
        Msg::NoHost => "这块屏幕没接上宿主".into(),
        Msg::CommandCarriesNoPictures { count } =>
            format!("命令带不了图片,附着的 {count} 张没有送出去;把它们放在一条消息里发").into(),
        Msg::FoldUsage { name, other } =>
            format!("`/{name} {other}`?它只认:不带参数(切换)、`show`、`hide`").into(),
        Msg::DiffAdded => "新增".into(),
        Msg::DiffAddedStaged => "新增 · 已暂存".into(),
        Msg::DiffModified => "改过".into(),
        Msg::DiffModifiedStaged => "改过 · 已暂存".into(),
        Msg::DiffDeleted => "删了".into(),
        Msg::DiffDeletedStaged => "删了 · 已暂存".into(),
        Msg::DiffRenamed => "改名".into(),
        Msg::DiffUntracked => "没跟踪".into(),
        Msg::DiffConflicted => "有冲突".into(),
        Msg::ClipboardHasNothing => "剪贴板里没有能贴的东西;`/paste 路径` 可以贴一个文件 —— 是图就当附件,别的当文字".into(),
        Msg::FileIsEmpty { path } => format!("{path} 是空的").into(),
        Msg::FileUnreadable { path, error } => format!("读不了 {path}:{error}").into(),
        Msg::KeysHelp => "enter 发送 · shift+enter 换行(或 ctrl-j) · ctrl-d 退出 · ctrl-w 删词\n\
             当轮进行中:esc 或 ctrl-c 停止当轮,排队的话退回输入框 · ctrl-x 停止当轮,排队的话立刻发出\n\
             空闲时:esc 连按两下清空输入,输入已空再连按两下打开回退 · ctrl-c 清空输入,再按一次退出\n\
             上/下 在输入里移动游标,到头则翻历史 · 点击输入框定位游标\n\
             pgup/pgdn 与滚轮滚动对话\n\
             alt-r 思考(一行/全文/收起,循环) · ctrl-t 工具输出(全部/单个摘要/成组摘要,循环) · ctrl-l 重画屏幕\n\
             ctrl-r 搜索这个项目里以前打过的东西;继续打字缩小范围,再按 ctrl-r 往更老翻,enter 接受,esc 还回草稿\n\
             shift+tab 切下一个执行模式(plan/ask/edits/auto;没有补全菜单时) · /config 里 ui.mode_switch_key=tab 可改用 tab 切、tab 则只用于补全\n\
             /showinject [名字] 环境注入(默认不显示;不带名字则全部,all 含同伴报告)\n\
             拖动选中并复制 · esc 取消选中 · 点击思考或工具调用折叠展开那一个\n\
             ctrl-o 把鼠标交还终端(改用终端自己的框选)".into(),
        Msg::ToolOutputUnknown { what } => format!("没有 `{what}` 这种工具输出形态;可以写 full(全部)/head(前后各20行)/each(单个摘要)/group(成组摘要)").into(),
        Msg::InjectionUnknown { what, names } => format!("没有 `{what}` 这种注入;可以写 {names} 或 all").into(),
        Msg::CopyWhichBlock { count } => format!("有 {count} 块;`/copy N` 指定哪一块,`/copy all` 全要").into(),
        Msg::CopyNoSuchBlock { count, asked } => format!("只有 {count} 块,没有第 {asked} 块").into(),
        Msg::CopyNoBlocks => "最后一条回复里没有代码块".into(),
        Msg::CopiedLines { lines } => format!("复制了 {lines} 行").into(),
        Msg::SaveNothingYet => "这段对话还没有内容可存".into(),
        Msg::SavedTo { path } => format!("存到 {path}").into(),
        Msg::SaveWouldOverwrite { path } => format!("{path} 已经在那儿了,而且不是 .md —— 换个名字,或者自己先删掉").into(),
        Msg::SaveFailed { error } => format!("存不下:{error}").into(),
        Msg::AllowanceNear { label, percent } => format!("{label}额度已用 {percent}%").into(),
        Msg::AllowanceNearWithReset {
            label,
            percent,
            resets_in,
        } => format!("{label}额度已用 {percent}% · {resets_in}后恢复").into(),
        Msg::ViewWhichFile => "要看哪个文件?`/view 路径`".into(),
        Msg::ViewNotText { path } => format!("{path} 不是文本文件").into(),
        Msg::ViewTooBig { mb } => format!("只读了开头 {mb} MB").into(),
        Msg::ViewOnlyFirstLines { lines } => format!("只显示前 {lines} 行").into(),
        Msg::ViewLongLinesCut { lines } => format!("{lines} 行过长已截断").into(),

        // ── the conversation's own commands (`commands.rs`) ──
        Msg::LookWhichSession => "要切到哪个会话?".into(),
        Msg::CancelledTurn => "已停下当前回合".into(),
        Msg::CancelledTurnAndMembers { members } => format!("已停下当前回合,以及 {members} 个成员的").into(),
        Msg::NoCompaction => "这个 agent 没有压缩策略".into(),
        Msg::ContextCounts { turn, messages, facts } => format!("{turn} 轮 · {messages} 条模型可见消息 · {facts} 条事实").into(),
        Msg::NothingSaidYet => "还没有对话".into(),
        Msg::NoRoster => "这块屏幕没有 agent 名册:启动器没有提供 `tui-team-roster`".into(),
        Msg::AgentsLead => "主 · 这个会话本身".into(),
        Msg::AgentsStopped { name } => format!("{name} · 已停,日志还在").into(),
        Msg::AgentsNoneYet => "这个会话底下还没有别的 agent".into(),
        Msg::AgentsPickerHint => "看谁 · enter 切过去".into(),
        Msg::SessionNeedsNewerVersion { id } => format!("需要更新版本才能打开 · {id}").into(),
        Msg::SessionTurnsWhenWhere { turns, when, dir } => format!("{turns} 轮 · {when} · {dir}").into(),
        Msg::SessionTurnsWhen { turns, when } => format!("{turns} 轮 · {when}").into(),
        Msg::ResumeNoOthers => "没有别的存下的会话".into(),
        Msg::ResumePickerHint => "回到哪个会话 · enter 打开 · Delete 删掉".into(),

        // ── background sessions ──
        Msg::CmdAboutBg => "后台会话:不带参数把这个会话放到后台接着跑,带任务就新开一个去做;也能看、换、丢".into(),
        Msg::CmdTakesBg => "[<任务> | list | <N> | drop <N>]".into(),
        Msg::CmdAboutReview => "让另一个会话把这次改动审一遍——默认放后台跑,当前对话不停".into(),
        Msg::CmdTakesReview => "[deep | deep+verify] [staged | <base>]".into(),
        Msg::BgUsage => "用法:/bg · /bg <任务> · /bg list · /bg <N> · /bg drop <N>(/bg 即 /background)".into(),
        Msg::BgNoSuchSlot { slot, count } => format!("没有第 {slot} 号后台会话(一共 {count} 个)").into(),
        Msg::BgMoved { slot } => format!("刚才的会话在后台 [#{slot}] 接着跑,这里是新的会话").into(),
        Msg::BgStarted { slot } => format!("后台 [#{slot}] 开始做了").into(),
        Msg::BgDropped { slot } => format!("已丢掉后台 [#{slot}]").into(),
        Msg::BgTold { title } => format!("已发给「{title}」").into(),
        Msg::BgNoPanel => "这块屏幕没挂后台面板".into(),
        Msg::BgPanelMoved => "你的对话已移到后台 — enter 打开 · esc 回到它 · ctrl+c 两次退出".into(),
        Msg::BgPanelLooking => "后台会话 — enter 打开 · esc 回到这里 · ctrl+c 两次退出".into(),
        Msg::BgGroupNeedsInput => "需要你".into(),
        Msg::BgGroupWorking => "进行中".into(),
        Msg::BgGroupCompleted => "已完成".into(),
        Msg::BgPanelEmpty => "后台还没有会话".into(),
        Msg::BgPlaceholder => "描述一个任务,新开一个会话".into(),
        Msg::BgReplyTo { title } => format!("回复「{title}」").into(),
        Msg::BgLegend => "enter 打开 · space 回复 · ctrl+x 删除 · ? 快捷键".into(),
        Msg::BgKeys => "↑↓ 选 · enter 打开 · esc 回去 · ← 收起面板 · space 就地回复选中的 · ctrl+x 删除(跑着的先取消) · 在框里写任务再回车,新开一个后台会话".into(),
        Msg::BgNothingSaid => "(还没说什么)".into(),
        Msg::BgRefusedWhileSharing => "这段对话正在共享,先停掉共享再换会话".into(),
        Msg::BgRefusedWhileAsking => "它在等你回答,先答了再换".into(),
        Msg::BgRefusedWhileReconfiguring => "正在换模型,稍等再换".into(),
        Msg::BgQuitQuestion { count } => format!("{count} 个后台会话还在跑,退出会停掉它们(会话已保存,之后可以 /resume)").into(),
        Msg::BgReplyWaiting => "这个会话在等你回答,按 Enter 打开".into(),
        Msg::BgQuitConfirm => "退出,停掉它们".into(),
        Msg::BgQuitStay => "留下".into(),
        Msg::BgWaitingTip { slot, title } => format!("后台 [{slot}] {title} 在等你回答 · /bg {slot} 打开").into(),
        Msg::BgSlotsFull { most } => format!("后台已经放了 {most} 个会话,先丢掉一个(/bg drop <N>)").into(),

        // ── reasoning effort, undo and rewind (`commands.rs`) ──
        Msg::EffortAbout => "这个会话的思考强度".into(),
        Msg::EffortDefaultAbout => "交给端点决定".into(),
        Msg::EffortPickerTitle { level } => format!("思考强度 · 现在 {level} · enter 改").into(),
        Msg::EffortPickerTitleDefault => "思考强度 · 现在交给端点 · enter 改".into(),
        Msg::EffortUnknown { wanted, levels } => format!("未知强度 `{wanted}`;可选:{levels}, default").into(),
        Msg::EffortSet { wanted } => format!("思考强度 → {wanted}").into(),
        Msg::EffortCurrent { now, levels } => {
            format!("思考强度 · 当前 {now} · 可选:{levels}").into()
        }
        Msg::UndoLeadOnly => "撤销只对主会话:先切回「主」".into(),
        Msg::NotATurnNumber { what } => format!("`{what}` 不是回合号").into(),
        Msg::RewindScopeUnknown { what } => format!("`{what}` 不是范围;可选:对话、代码、全部").into(),
        Msg::RewindRestored { files } => format!("已还原 {files} 个文件").into(),

        // ── model, mode and working directory (`commands.rs`) ──
        Msg::ModelOnlyCurrent { current } => format!("当前模型:{current};没有别的可选").into(),
        Msg::ModelNoneConfigured => "没有配置可选的模型".into(),
        Msg::ModelPickerHint => "换成哪个模型 · enter 换过去".into(),
        Msg::ModelSet { wanted } => format!("模型 → {wanted}").into(),
        Msg::ModeWhatEachDoes => "plan 只看不动 · ask 动手前问 · edits 改文件不问 · auto 全不问".into(),
        Msg::ModeUnknown { what } => format!("`{what}` 不是一档;可选:plan、ask、edits、auto").into(),
        Msg::ModeSet { mode } => format!("现在是 {mode}").into(),
        Msg::CdUpOneLevel => "上一层".into(),
        Msg::CdStepInto => "进去看看".into(),
        Msg::CdStayHere => "就在这儿干活".into(),
        Msg::CdPickerHint { here } => format!("换到哪个目录 · 现在在 {here}").into(),
        Msg::CdMovedNewSession { directory, session } => format!("现在在 {directory} 里干活 · 新会话 {session}").into(),
        Msg::CdMoved { directory } => format!("现在在 {directory} 里干活").into(),

        // ── what changed, the language, and what is left on the account (`commands.rs`) ──
        Msg::DiffNoChangeIn { what } => format!("{what} 没有改动").into(),
        Msg::DiffNothingChanged => "这个会话还没有改过工作区里的文件".into(),
        Msg::DiffBinary => "二进制".into(),
        Msg::DiffPickerHint { count, added, removed } => format!("改过 {count} 个文件 · +{added} -{removed} · enter 看改动").into(),
        Msg::NoLanguageSetting => "这个宿主没有语言这一项".into(),
        Msg::LanguageNow { value, accepts } => format!("语言:{value} · 可选 {accepts} · `/language <值>` 改它").into(),
        Msg::LanguageSet { wanted, applies } => format!("语言:{wanted}({applies}生效)").into(),
        Msg::UsageNotCounted => "这个宿主不计额度".into(),
        Msg::UsageCallLimit { n } => format!(" · 上限 {n} 次").into(),
        Msg::UsageResetsIn { duration } => format!("{duration}后").into(),
        Msg::UsageResetsAt { at } => format!("{at} ").into(),
        Msg::UsageExhausted { label, when, cap } => format!("{label} 用完了 · {when}回来{cap}").into(),
        Msg::UsageLeft { label, cap } => format!("{label} 还有{cap}").into(),

        // ── running on its own, where the session stands, and who is signed in (`commands.rs`) ──
        Msg::AutonomyIdle => "现在没有在自己干".into(),
        Msg::AutonomyGoal { what } => format!("目标:{what}").into(),
        Msg::AutonomyLoop { what } => format!("循环:{what}").into(),
        Msg::AutonomyRoundOf { round, of } => format!("第 {round}/{of} 轮").into(),
        Msg::AutonomyRound { round } => format!("第 {round} 轮").into(),
        Msg::AutonomyLine { what, rounds, took } => format!("{what} · {rounds} · 已跑 {took}").into(),
        Msg::AutonomyHeld { line, why } => format!("{line} · 停着:{why}").into(),
        Msg::StatusNoModel => "没有挂模型".into(),
        Msg::StatusEffortDefault => "端点默认".into(),
        Msg::StatusSessionLine { session } => format!("会话 {session}").into(),
        Msg::StatusModelLine { model, effort } => format!("模型 {model} · 思考强度 {effort}").into(),
        Msg::StatusWhereLine { where_ } => format!("在 {where_}").into(),
        Msg::StatusAutonomyLine { what, round, took } => format!("在自己干:{what} · 第 {round} 轮 · 已跑 {took}").into(),
        Msg::VisionFailedBecause { reason } => format!("图片没认出来:{reason}").into(),
        Msg::ModelNotKept { error } => format!("但没存下来,重启后还是旧的那个:{error}").into(),
        Msg::RefusedStaleQuestion => "那个问题已经不等回答了".into(),
        Msg::RefusedNotRunning => "没有正在跑的回合可停".into(),
        Msg::RefusedUnavailable => "现在接不了 —— 没有可用的 provider,或者正在换".into(),
        Msg::RefusedUnsupported => "这个宿主答不了这条命令".into(),
        Msg::CostNothingYet => "这段对话还没花过 token".into(),
        Msg::CostTokens { prompt, completion, cached, rate, total } => format!(
            "  发出 {prompt} · 收回 {completion} · 共 {total}\n  其中命中缓存 {cached}（{rate}%）"
        )
        .into(),
        Msg::CostUnattributed { tokens } => {
            format!("归不到哪个模型名下的：{tokens}").into()
        }
        Msg::ShellTimedOut { secs } => format!("[跑了 {secs} 秒还没完,停了]").into(),
        Msg::ShellFailed { code } => format!("[退出码 {code}]").into(),
        Msg::ShellSaidNothing => "[没有输出]".into(),
        Msg::CompactionInterrupted => "压缩被打断了 —— 上下文还是原来那么长".into(),
        Msg::RuntimeStopped { how } => format!("运行时停了:{how}。这个会话不会再有新的东西。").into(),
        Msg::GoalMet { condition } => format!("目标达成:{condition}").into(),
        Msg::GoalGaveUp { condition } => {
            format!("目标停了,但没能判定它是否达成:{condition}").into()
        }
        Msg::WhoAmIUnnamed => "登录着,但宿主没说是谁".into(),
        Msg::WhoAmIStoredAt { path } => format!("凭据存在 {path}").into(),
        Msg::WhoAmINobody => "没有人登录;这份配置用的是自带的凭据".into(),
        Msg::ThinkingNow { value } => format!("思考:{value};改用 /think on 或 /think off").into(),
        Msg::NoThinkingSwitch => "这个宿主没有思考开关".into(),
        Msg::NotOnOrOff { what } => format!("`{what}` 不是 on 或 off").into(),
        Msg::ThinkingSet { value } => format!("思考:{value}").into(),
        Msg::RenameNeedsName => "要一个名字:/rename <名字>".into(),
        Msg::RenamedTo { title } => format!("这个会话现在叫「{title}」").into(),

        // ── MCP servers, reloading and signing in (`commands.rs`) ──
        Msg::McpNoneConfigured => "没有配置 MCP 服务器".into(),
        Msg::McpConnecting => "连接中".into(),
        Msg::McpConnected => "已连接".into(),
        Msg::McpUntrusted => "未信任项目,未启动".into(),
        Msg::McpNeedsAuthentication => "需要认证".into(),
        Msg::McpFailed { message } => format!("失败:{message}").into(),
        Msg::McpDisconnected => "已断开".into(),
        Msg::McpDisabled => "配置里已停用".into(),
        Msg::McpUnknownState => "未知".into(),
        Msg::McpWithdrawn => "已撤下全部 MCP 工具".into(),
        Msg::McpNeedsServerName => "要一个服务器名:/mcp tools <服务器>".into(),
        Msg::McpServerHasNoTools { server } => format!("{server} 没有挂上任何工具").into(),
        Msg::McpUnknownSubcommand { what } => format!("`/mcp {what}` 不认识;可用:/mcp、/mcp tools <服务器>、/mcp withdraw").into(),
        Msg::Reloaded => "已重新读取 skills、MCP 与配置".into(),
        Msg::SignedOut => "已登出;/login 重新登录".into(),
        Msg::SignedIn => "已登录".into(),

        // ── MCP 面板 (`mcp.rs`、`modules/mcp.rs`) ──
        Msg::McpPanelTitle => "管理 MCP 服务器".into(),
        Msg::McpPanelServers { n } => format!("{n} 个服务器").into(),
        Msg::McpPanelEmpty => "没有配置任何 MCP 服务器".into(),
        Msg::McpPanelNoMatch => "没有配置中的服务器匹配".into(),
        Msg::McpPanelUnavailable => "这个构建没挂 MCP 面板".into(),
        Msg::McpDetailPending => "正在取详情…".into(),
        // 服务器从哪儿来:产品表的 `/help` 来源列说的是同样两个地方,所以这两句读
        // 它的措辞,不在这儿再写一遍(`tests/tables.rs`)。
        Msg::McpGroupGlobal => {
            crate::product::t_with(crate::Locale::ZhCn, crate::product::Msg::HelpSourceGlobal)
        }
        Msg::McpGroupProject => {
            crate::product::t_with(crate::Locale::ZhCn, crate::product::Msg::HelpSourceProject)
        }
        Msg::McpGroupDriver => "外部传入".into(),
        Msg::McpLabelState => "状态".into(),
        Msg::McpLabelAuth => "认证".into(),
        Msg::McpLabelEndpoint => "地址".into(),
        Msg::McpLabelSource => "来源".into(),
        Msg::McpLabelTools { n } => format!("工具 {n} 个").into(),
        Msg::McpAuthNone => "不需要".into(),
        Msg::McpAuthAuthenticated => "已认证".into(),
        Msg::McpAuthNotAuthenticated => "未认证".into(),
        Msg::McpActionTrust => "信任这个项目".into(),
        Msg::McpActionUntrust => "取消信任".into(),
        Msg::McpActionLogin => "认证".into(),
        Msg::McpActionLogout => "登出".into(),
        Msg::McpActionEnable => "启用".into(),
        Msg::McpActionDisable => "停用".into(),
        Msg::McpLegendList => "↑/↓ 移动 · Enter 详情 · Esc 关闭".into(),
        Msg::McpLegendDetail => "↑/↓ 移动 · Enter 执行 · Esc 返回".into(),
        Msg::McpLegendBusy => "Esc 取消".into(),
        Msg::McpLegendBusyHide => "Esc 收起".into(),
        Msg::McpLegendCancelling => "正在取消… · Esc 收起".into(),
        Msg::McpSignInCancelled => "认证已取消".into(),

        // ── the toolbox and the plugins (`commands.rs`) ──
        Msg::CmdTakesToolbox => "[off <名字或 mcp__server__*> | on <同上>]".into(),
        Msg::CmdAboutToolbox => "工具箱:不带参数拉出面板(看有哪些、开关它);带参数直接关掉或放回".into(),
        Msg::NoToolCatalog => "这个屏幕没有接工具目录:启动器没有提供 `tui-tools`".into(),
        Msg::ToolboxUnknownVerb { what } => format!("不认识 `{what}`,只有 `off` 和 `on`").into(),
        Msg::ToolboxNeedsPattern { verb } => format!("`{verb}` 要一个名字或模式,例如 `mcp__github__*`").into(),
        Msg::ToolboxNothingMoved { pattern } => format!("没有工具因此改变 —— `{pattern}` 要么没匹配上,要么是配置排除掉的").into(),
        Msg::ToolboxPutBack { names } => format!("放回来了:{names}").into(),
        Msg::ToolboxTurnedOff { names } => format!("关掉了:{names}").into(),
        Msg::ToolboxNameJoiner => "、".into(),
        Msg::CmdTakesPlugin => "[list | install <名字> | uninstall <名字> | update <名字> | marketplace …]".into(),
        Msg::CmdAboutPlugin => "插件:不带参数拉出面板(装、卸、加市场);带参数直接做".into(),
        Msg::PluginNoSuch { typed } => format!("没有叫 {typed} 的插件").into(),
        Msg::PluginAmbiguous { name, lines } => format!("有好几个叫 {name} 的,说清是哪个:\n{lines}").into(),
        Msg::NoPluginPort => "这个屏幕没有接插件:启动器没有提供 `tui-plugins`".into(),
        Msg::NoMcpPort => "这个屏幕没有接 MCP:启动器没有提供 `tui-mcp`".into(),
        Msg::CmdTakesSetup => "[focus area,例如 hooks、mcp、skills、all]".into(),
        Msg::CmdAboutSetup => "分析这个项目、装好种子 skill,然后给出该配哪些自动化的建议".into(),
        Msg::NoSetupPort => "这个屏幕没有接种子安装:启动器没有提供 `tui-setup`,所以它装不了种子".into(),
        Msg::SetupJobDidNotStart { error } => format!("装种子的活没能派出去:{error}").into(),
        Msg::SetupFailed { error } => format!("装种子失败:{error}").into(),
        Msg::SetupInstalling => "正在装种子文件…".into(),
        Msg::SetupRunningSkill => "种子已就位,正在让模型分析这个项目…".into(),
        Msg::PluginNothingInstalled => "还什么都没装".into(),
        Msg::PluginInstalledList { lines } => format!("装着这些:\n{lines}").into(),
        Msg::PluginInstallWhich => "要装哪个?`/plugin install <名字>`".into(),
        Msg::PluginAlreadyInstalled { id } => format!("{id} 已经装着了。要重装先 `/plugin uninstall {id}`").into(),
        Msg::PluginInstalling { plugin, market } => format!("正在装 {plugin}@{market} …").into(),
        Msg::PluginUninstallWhich => "要卸哪个?`/plugin uninstall <名字>`".into(),
        Msg::PluginNotInstalled { typed } => format!("没装着叫 {typed} 的插件").into(),
        Msg::PluginUninstalling { id } => format!("正在卸 {id} …").into(),
        Msg::PluginUpdateWhich => "要更新哪个?`/plugin update <名字>`".into(),
        Msg::PluginUpdating { id } => format!("正在更新 {id} …").into(),
        Msg::MarketNoneYet => "一个市场都还没有".into(),
        Msg::MarketRow { name, source, plugins, installed } => format!("  {name}  {source}  {plugins} 个插件,装了 {installed}").into(),
        Msg::MarketList { lines } => format!("在册的市场:\n{lines}").into(),
        Msg::MarketAddWhich => "要加哪个?`/plugin marketplace add <地址>`".into(),
        Msg::MarketFetching { what } => format!("正在取 {what} …").into(),
        Msg::MarketRemoveWhich => "要删哪个?`/plugin marketplace remove <名字>`".into(),
        Msg::MarketRemoving { what } => format!("正在删市场 {what} …").into(),
        Msg::MarketUpdateWhich => "要更新哪个?`/plugin marketplace update <名字>`".into(),
        Msg::MarketUpdating { what } => format!("正在更新市场 {what} …").into(),
        Msg::MarketUnknownAction { what } => format!("`/plugin marketplace` 没有 {what} 这个动作;有 list、add、remove、update").into(),
        Msg::PluginUnknownAction { what } => format!("`/plugin` 没有 {what} 这个动作;有 list、install、uninstall、update、marketplace、reload,或者不带参数拉出面板").into(),
        Msg::ReloadFailedAfter { said, why } => format!("{said}\n但会话没能重新加载,新东西要等下次启动才生效:{why}").into(),
        Msg::MarkdownUser => "## 我".into(),
        Msg::MarkdownAssistant => "## 模型".into(),

        // ── the settings panel and what it shows about usage (`modules/settings.rs`) ──
        Msg::SettingsNoMatch => "  没有匹配的设置".into(),
        Msg::SettingsAbove { above } => format!("上面还有 {above} 行").into(),
        Msg::SettingsBelow { below, arrow } => format!("下面还有 {below} 行 {arrow}").into(),
        Msg::SettingsTitle => "设置".into(),
        Msg::AskingHost => "正在问宿主…".into(),
        Msg::UsageThisSession => "本会话".into(),
        Msg::UsageContextBar { percent, used, window } => format!("上下文已用 {percent}% · {used} / {window}").into(),
        Msg::UsageModelNote { model } => format!("模型 {model}").into(),
        Msg::UsageAllowanceHead => "额度".into(),
        Msg::UsageCallsUsedOfLimit { used, limit } => format!(" · {used} / {limit} 次").into(),
        Msg::UsageSpentPercent { percent, counted } => format!("用掉 {percent}%{counted}").into(),
        Msg::UsageWindowNotReported => "这个窗口没报用量".into(),
        Msg::UsageSpent => "用完了".into(),
        Msg::UsageResetsAtNote { at } => format!("{at} 重置").into(),
        Msg::UsagePlanHead { plan, state } => format!("{plan} · {state}").into(),
        Msg::UsagePlanTermPercent { percent } => format!("{percent}%").into(),

        // ── what this build is running as (`modules/settings.rs`) ──
        Msg::StatusRowVersion => "版本".into(),
        Msg::StatusRowSession => "会话".into(),
        Msg::StatusRowSessionId => "会话 id".into(),
        Msg::StatusRowDirectory => "目录".into(),
        Msg::StatusRowSignedIn => "登录".into(),
        Msg::StatusRowPlan => "订阅".into(),
        Msg::StatusRowUsage => "用量".into(),
        Msg::StatusNoAccount => "没有账号(用配置里的凭据)".into(),
        Msg::StatusWhoDetail { who, detail } => format!("{who} · {detail}").into(),
        Msg::StatusModelEffort { model, effort } => format!("{model} · 思考强度 {effort}").into(),
        Msg::StatusPlanExpires { at } => format!(" · 到期 {at}").into(),
        Msg::StatusPlanDaysLeft { remaining, total } => format!("（剩 {remaining}/{total} 天）").into(),
        Msg::StatusWindowSpent { percent } => format!("当前窗口用掉 {percent}%").into(),
        Msg::StatusWindowNotReported => "当前窗口没报用量".into(),
        Msg::StatusWindowResetsIn { duration } => format!(" · {duration}后重置").into(),
        Msg::McpTallyFailed { n } => format!("{n} 个连不上").into(),
        Msg::McpTallyUntrusted { n } => format!("{n} 个等信任").into(),
        Msg::McpTallyNeedsAuthentication { n } => format!("{n} 个待认证").into(),
        Msg::McpTallyConnecting { n } => format!("{n} 个连接中").into(),
        Msg::McpTallyConnected { n } => format!("{n} 个已连接").into(),
        Msg::McpTallyOff { n } => format!("{n} 个没连").into(),
        Msg::McpTallyDisabled { n } => format!("{n} 个已停用").into(),
        Msg::StatsNotKept => "这个宿主不记账".into(),

        // ── the account's figures (`modules/settings.rs`) ──
        Msg::StatsNoDaily => "没有按天的记录".into(),
        Msg::StatsDailyHead => "每天用掉多少".into(),
        Msg::StatsNoModels => "没有按模型的记录".into(),
        Msg::StatsRange { from, to } => format!("{from} 到 {to}").into(),
        Msg::StatsDays { n } => format!("{n} 天").into(),
        Msg::StatsNoneInPeriod => "这段时间没有用量".into(),
        Msg::StatsColTokens => "tokens".into(),
        Msg::StatsColRequests => "请求".into(),
        Msg::StatsColShare => "占比".into(),
        Msg::SettingUnset => "（未设置）".into(),
        Msg::SettingHintToggle => "回车 切换".into(),
        Msg::SettingHintEdit => "回车 编辑".into(),
        Msg::SettingsNotFound => " 未找到".into(),
        Msg::LegendSave => "保存".into(),
        Msg::LegendCancel => "取消".into(),
        Msg::LegendChangePage => "换页".into(),
        Msg::LegendPagesHere => "这页的分页".into(),
        Msg::LegendPageKeys => "翻页键".into(),
        Msg::LegendScroll => "滚动".into(),
        Msg::LegendClose => "关闭".into(),
        Msg::LegendPressAgainToReset => "再按一次恢复默认".into(),
        Msg::LegendAnyOtherKey => "其它键".into(),
        Msg::LegendSelect => "选择".into(),
        Msg::LegendEdit => "修改".into(),
        Msg::LegendRestoreDefault => "恢复默认".into(),
        Msg::LegendClearSearch => "清空搜索".into(),

        // ── the host loop: what it says while it works (`plugin.rs`) ──
        Msg::SwitchedToSession { session } => format!("已切换到会话 {session}").into(),
        Msg::TurnNotStored { message } => format!("这一回合没能存下来:{message}").into(),
        Msg::McpServerNotConfigured { server } => format!("配置里没有 MCP 服务器 {server}").into(),
        Msg::McpSignInLost => "认证中途断了,没有拿到结果".into(),
        Msg::McpSignedInReloadLater { server } => {
            format!("{server} 已认证,凭据已保存;当前有回合在跑,结束后执行 /mcp reload 连接它").into()
        }
        Msg::McpLoginUrl { server, url } => {
            format!("正在认证 MCP 服务器 {server}。浏览器没有打开的话,复制这个链接去打开:\n{url}").into()
        }
        Msg::McpSignInAsking { host } => format!("正在连接 {host}…").into(),
        Msg::McpSignInWaiting => "等待浏览器授权…".into(),
        Msg::MouseTakenBackAuto => "鼠标被终端收回了,已自动要回;若再次发生,ctrl-o 可手动切换".into(),
        Msg::ScreenNotConnectedProviders => "屏幕还没接上,改不了 provider".into(),
        Msg::NoProviderPort => "这个屏幕没有接 provider:启动器没有提供 `tui-providers`".into(),
        Msg::ProviderEdited { id } => format!("改好了 {id}").into(),
        Msg::ProviderProbePassed => "✓ 连通检测通过".into(),
        Msg::ProviderProbeFailed => "✗ 连通检测没通过 —— 原因和改法在对话里".into(),
        Msg::ProviderAddedAddModel { id } => format!("加好了 {id},给它添一个模型").into(),
        Msg::ProviderAdded { id } => format!("加好了 {id}").into(),
        Msg::ProviderDeletedWithModels { id } => format!("删了 {id},连同它下面的模型").into(),
        Msg::ProviderDeleted { id } => format!("删了 {id}").into(),
        Msg::ConfigWrittenReloadFailed { error } => format!("配置写下了,但会话没能重新加载:{error}").into(),
        Msg::ToolCatalogUnreadable { why } => format!("读不到工具目录:{why}").into(),
        Msg::ScreenNotConnectedTools => "屏幕还没接上,开关不了工具".into(),
        Msg::SwitchFailed { why } => format!("没成:{why}").into(),
        Msg::ScreenNotConnectedPlugins => "屏幕还没接上,改不了插件".into(),
        Msg::PluginJobCancelled => "不等了。它落地之后会自己收拾干净".into(),
        Msg::PluginInstallingAt { plugin, marketplace } => format!("正在装 {plugin}@{marketplace} …").into(),
        Msg::PluginUpdatingAt { plugin, marketplace } => format!("正在更新 {plugin}@{marketplace} …").into(),
        Msg::PluginUninstallingAt { plugin, marketplace } => format!("正在卸 {plugin}@{marketplace} …").into(),
        Msg::ReloadFailedAfterPlugin { why } => format!("但会话没能重新加载,新东西要等下次启动才生效:{why}").into(),
        Msg::ScreenNotConnectedSettings => "屏幕还没接上,改不了设置".into(),
        Msg::NoSettingsPort => "这个屏幕没有接设置:启动器没有提供 `tui-settings`".into(),
        Msg::SettingWrittenReloadFailed { why } => format!("设置已写入,但重新加载失败:{why}").into(),
        Msg::NowViewing { name } => format!("正在看 {name}").into(),
        Msg::PolicyBlockedNoWayOut => "策略边界挡下了这一步,而这次没有给出可选的走法".into(),
        Msg::PolicyQuestion => "这一步被策略挡下了。接下来怎么走?".into(),
        Msg::PolicyAsker => "策略".into(),
        Msg::NotDelivered { error } => format!("没有送达:{error}").into(),
        Msg::Compacted => "已压缩".into(),
        Msg::NothingWorthCompacting => "暂时没有值得压缩的".into(),
        Msg::CompactFailed { error } => format!("没压缩成：{error}").into(),
        Msg::ClipboardHasNoImage => "剪贴板里没有图片".into(),
        Msg::ImagePreviewFailed { reason } => format!("打不开图片：{reason}").into(),
        Msg::NoOpener => "这个界面不能打开文件".into(),
        Msg::ImageGone => "这张图已经找不到了".into(),
        Msg::ImageCorrupt => "图片数据损坏".into(),
        Msg::MouseTaken => "鼠标已收回:拖动选中并复制,点击思考或工具调用折叠展开那一个,滚轮滚动,esc 取消选中".into(),
        Msg::MouseHandedBack => "鼠标已交还终端:改用终端自己的框选(可跨 scrollback)。折叠用 ctrl-t,思考用 alt-r(默认不显示),滚动用 pgup/pgdn,ctrl-o 收回鼠标".into(),
        Msg::NoProviderPanel => "这个屏幕没有 provider 面板:启动器没有提供 `tui-panel-providers`".into(),
        Msg::NoPluginPanel => "这个屏幕没有插件面板:启动器没有提供 `tui-panel-plugins`".into(),
        Msg::NoToolPanel => "这个屏幕没有工具面板:启动器没有提供 `tui-panel-tools`".into(),
        Msg::CopiedSelection => "已复制选中的内容".into(),
        Msg::MenuCopySelection => "复制选中".into(),
        Msg::MenuCopySelectionAbout => "把选中的文字写到剪贴板".into(),
        Msg::MenuCopyAll => "复制全文".into(),
        Msg::MenuCopyAllAbout => "把输入框写到剪贴板".into(),
        Msg::MenuPaste => "粘贴".into(),
        Msg::MenuPasteAbout => "从剪贴板插入".into(),
        Msg::MenuClear => "清空".into(),
        Msg::MenuClearAbout => "丢掉草稿和附件".into(),
        Msg::MenuSend => "发送".into(),
        Msg::MenuSendAbout => "把这一条交给模型".into(),
        Msg::SelectionHasNoText => "选中的内容没有可复制的文字".into(),
        Msg::NothingToCopy => "没有可复制的内容".into(),
        Msg::ClipboardHasNoTextShort => "剪贴板里没有文本".into(),
        Msg::ModelCannotSeeImages { model } => format!("当前模型 `{model}` 看不了图片:贴进去也只会在发出去时被丢掉,所以没贴。\n换成能看图的模型再贴。").into(),
        Msg::ModelUnknownForImages => "还不知道这个 agent 用的是什么模型,图片没有去处,所以没贴。".into(),

        // ── installing a plugin: where it goes and what each action does (`plugins.rs`) ──
        Msg::ScopeUserName => "这台机器".into(),
        Msg::ScopeProjectName => "这个项目".into(),
        Msg::ScopeLocalName => "只有自己".into(),
        Msg::ScopeUserAbout => "装进 ~/.atomcode/plugins,哪个项目都能用".into(),
        Msg::ScopeProjectAbout => "装进 .atomcode/plugins,跟着仓库走、同事也有".into(),
        Msg::ScopeLocalAbout => "装进 .atomcode/plugins/local,不进 git,只有自己有".into(),
        Msg::ScopeUserShort => "机器".into(),
        Msg::ScopeLocalShort => "自己".into(),
        Msg::PluginTabAll => "全部".into(),
        Msg::PluginTabInstalled => "已装".into(),
        Msg::PluginTabMarkets => "市场".into(),
        Msg::PluginActionUpdateAbout => "从市场再取一遍,卸掉旧的装上新的".into(),
        Msg::PluginActionUninstallAbout => "拿掉它,它带来的技能和钩子一并没有".into(),
        Msg::MarketActionBrowse => "看它带的插件".into(),
        Msg::MarketActionRemove => "删掉这个市场".into(),
        Msg::MarketActionBrowseAbout => "回到全部页,只留它的".into(),
        Msg::MarketActionUpdateAbout => "再拉一次,看它有没有新插件".into(),
        Msg::MarketActionRemoveAbout => "连同从它装的插件一起拿掉".into(),

        // ── how the conversation reads (`content.rs`) ──
        Msg::ThoughtLines { gutter, n } => format!("{gutter} 思考 {n} 行").into(),
        Msg::ToolsRun { count } => format!("已执行了 {count} 个工具").into(),
        Msg::ToolsFailed { failed } => format!(" · {failed} 失败").into(),
        Msg::VerbSkill => "技能".into(),
        Msg::VerbMemory => "记忆".into(),
        Msg::OutcomeInterrupted => "已中断".into(),
        Msg::OutcomeFailedWith { first } => format!("失败 · {first}").into(),
        Msg::OutcomeLines { lines } => format!("{lines} 行").into(),
        Msg::RewindScopeConversation => "对话".into(),
        Msg::RewindScopeCode => "工作区".into(),
        Msg::RewindScopeBoth => "对话与工作区".into(),
        Msg::RewoundToTurn { what, turn } => format!("↶ 已把{what}撤回到第 {turn} 轮之前").into(),
        Msg::RewoundEarlier { what } => format!("↶ 已把{what}撤回到更早的一轮之前").into(),
        Msg::AskRefuseChoice => "拒绝".into(),
        Msg::TurnRounds { steps } => format!("{steps} 轮").into(),
        Msg::TurnTools { tools } => format!("{tools} 工具").into(),
        Msg::StopCancelled => "已中断".into(),
        Msg::StopMaxRounds => "已中断 · 轮数用完了".into(),
        Msg::StopByPolicy => "已中断 · 一条停止策略叫停(时限或预算)".into(),
        Msg::StopRunawayFuse => "已中断 · 兜底熔断,这棵树没挂停止策略".into(),
        Msg::StopToolLoop => {
            "已中断 · 模型反复同一步、没有进展 —— 换个说法或给点提示,再发一条消息继续".into()
        }
        Msg::StopPromptRejected => "已中断 · 输入被拒绝".into(),
        Msg::StopPolicyDenied => "已中断 · 安全策略拦下了这一步".into(),
        Msg::StopRateLimited => "已暂停 · 触发限流".into(),
        Msg::StopTimeout => "已中断 · 模型长时间没有回应".into(),
        Msg::StopInvariantViolated => "已中断 · 内部不变量被破坏,这条会话不宜再续".into(),
        Msg::StopMaxContinuations => "已中断 · 自动续跑次数用完了".into(),
        Msg::StopWithOpenItems { count } => {
            format!("已停下 · 任务清单还有 {count} 项没完成 —— 发一句「继续」接着做").into()
        }

        // ── the live strip and the folded-lines notes (`modules/live.rs`, `host.rs`) ──
        Msg::LiveStopping => "正在停止".into(),
        Msg::LiveRecognizingImage => "正在识别图片".into(),
        Msg::LiveWaiting => "正在等待模型".into(),
        Msg::LiveThinking => "正在思考".into(),
        Msg::LiveWriting => "正在回复".into(),
        Msg::LiveSilentFor { secs } => format!("已 {secs} 秒没有新内容").into(),
        Msg::LiveRunningTools { n } => format!("正在运行 {n} 个工具").into(),
        Msg::LiveElapsed { took } => format!("耗时 {took}").into(),
        Msg::LiveIn { tokens } => format!("入 {tokens}").into(),
        Msg::LiveOut { tokens } => format!("出 {tokens}").into(),
        Msg::LiveCached { hit } => format!("缓存 {hit}").into(),
        Msg::LiveStep { step } => format!("第 {step} 步").into(),
        Msg::FoldedLines { hidden } => format!("⋯ 已折叠 {hidden} 行，点击展开").into(),
        Msg::MoreBelow { arrow, lines } => format!(" {arrow} 还有 {lines} 行 · 点击回到底部 ").into(),

        // ── the plugin panel (`modules/plugins.rs`) ──
        Msg::AddMarketNotesHead => "可以是:".into(),
        Msg::AddMarketNoteHttps => "  · https://atomgit.com/某某/某仓库.git".into(),
        Msg::AddMarketNoteSsh => "  · git@atomgit.com:某某/某仓库.git".into(),
        Msg::AddMarketNoteLocal => "  · ./本地/某个目录".into(),
        Msg::PluginsNoMarketsYet => "  一个市场都还没有 —— 到「市场」页加一个".into(),
        Msg::PluginsNoMatch => "  没有匹配的插件".into(),
        Msg::PluginsNothingInstalled => "  还什么都没装".into(),
        Msg::PluginsNoMatchInstalled => "  装上的里头没有匹配的".into(),
        Msg::PluginsNoMatchMarkets => "  没有匹配的市场".into(),
        Msg::PluginFormScopeTitle { plugin, marketplace } => format!("装 {plugin}@{marketplace} —— 装到哪儿").into(),
        Msg::PluginFormAddMarketTitle => "加一个市场".into(),
        Msg::MarketPluginCount { n } => format!("{n} 个插件").into(),
        Msg::MarketInstalledCount { n } => format!("装了 {n}").into(),
        Msg::ArmedRemoveMarket => "再按一次 ^d 删掉这个市场".into(),
        Msg::ArmedRemoveMarketWithPlugins { n } => format!("再按一次 ^d 删掉它,连同从它装的 {n} 个插件").into(),
        Msg::ArmedUninstall => "再按一次 ^d 卸载".into(),
        Msg::FieldAddress => "地址".into(),
        Msg::LegendStopWaiting => "不等了".into(),
        Msg::LegendAdd => "加上".into(),
        Msg::LegendThisOne => "就这个".into(),
        Msg::LegendBack => "返回".into(),
        Msg::LegendPressAgain => "再按一次".into(),
        Msg::LegendInstallOrOpen => "装它 / 看它".into(),
        Msg::LegendUpdateOrRemove => "更新或卸载".into(),
        Msg::LegendOpen => "打开".into(),
        Msg::LegendAddMarket => "加市场".into(),
        Msg::LegendTakeAway => "拿掉".into(),

        // ── the provider panel (`modules/providers.rs`) ──
        Msg::ProvidersNoMatch => "  没有匹配的 provider".into(),
        Msg::ProviderFormEdit { id } => format!("改 {id}").into(),
        Msg::ProviderFormAddAccount => "添加 provider".into(),
        Msg::ProviderFormAddModelTo { account } => format!("给 {account} 添加模型").into(),
        Msg::ProviderFormAddModel => "添加模型".into(),
        Msg::ProviderModelCount { n } => format!("{n} 个模型").into(),
        Msg::ProviderHasKey => "有密钥".into(),
        Msg::ProviderNoKey => "没有密钥".into(),
        Msg::ProviderManaged => "登录管理".into(),
        Msg::ProviderUnconfigured => "未配置".into(),
        Msg::ProviderVision => "视觉".into(),
        Msg::ArmedDelete => "再按一次 ^d 删除".into(),
        Msg::FieldName => "名字".into(),
        Msg::FieldProtocol => "协议".into(),
        Msg::FieldKey => "密钥".into(),
        Msg::FieldVision => "看图".into(),
        Msg::FieldEffort => "思考强度".into(),
        Msg::FieldLevels => "可选强度".into(),
        Msg::FieldWindow => "上下文".into(),
        Msg::FieldUseAfterSaving => "存完就用".into(),
        Msg::VisionYes => "能".into(),
        Msg::VisionNo => "不能".into(),
        Msg::EffortUnsupported => "不支持".into(),
        Msg::YesWord => "是".into(),
        Msg::NoWord => "否".into(),
        Msg::KeyLeaveBlankToKeep => "（留空则不改）".into(),
        Msg::LegendNextField => "下一项".into(),
        Msg::LegendChangeValue => "改".into(),
        Msg::LegendPressAgainToDelete => "再按一次删除".into(),
        Msg::LegendSeeItsModels => "看它的模型".into(),
        Msg::LegendSwitchToIt => "换过去".into(),
        Msg::LegendChange => "修改".into(),

        // ── the host side of the screen (`atomcode-cli`) ──
        Msg::HostConfigNotEditable => "这个宿主的配置不能从屏幕上改".into(),
        Msg::HostHeld => "停着".into(),
        Msg::SourceConfigFile => "配置文件".into(),
        Msg::SourceSettings => "设置".into(),
        Msg::SourceInstructionFiles => "指令文件".into(),
        Msg::SourceMemoryFiles => "记忆文件".into(),
        Msg::HostNoEditableConfig => "这个宿主没有可改的配置".into(),
        Msg::HostNoModelCatalog => "这个宿主没有模型目录".into(),
        Msg::NameCannotBeEmpty => "名字不能是空的".into(),
        Msg::SettingThinking => "思考".into(),
        Msg::RuntimeAlreadyStopped => "这个会话的运行时已经停了".into(),
        Msg::NoProviderConfigured => "还没有配置任何 provider——先加一个才能开始".into(),
        Msg::LoginExpired => "登录已经失效，需要重新登录".into(),
        Msg::ProviderUnsupportedByBuild => "这个构建不支持所配置的 provider".into(),
        Msg::ScreenNotConnectedAgent => "屏幕还没接上 agent".into(),
        Msg::HostHasNoControl => "这个宿主没有控制面".into(),
        Msg::HostNotFoundShort => "找不到:会话已经换过了".into(),
        Msg::NoModelSelected => "现在没有选中的模型,这一项无处可写".into(),
        Msg::RetryCountFor { selection } => format!("{selection} 的重试次数").into(),
        Msg::NoSuchSetting { id } => format!("没有叫 `{id}` 的设置").into(),

        // ── the provider forms' refusals (`atomcode-cli`) ──
        Msg::ProviderNameRules => "给它起个名字:字母、数字、`-`、`_`、`.`".into(),
        Msg::ProtocolNeedsEndpoint => "这个协议没有默认地址,得填一个".into(),
        Msg::ManagedCannotEditUseLogin { id } => format!("{id} 归登录管理,这儿改不了;用 /login").into(),
        Msg::ManagedCannotDeleteUseLogout { id } => format!("{id} 归登录管理,这儿删不了;用 /logout").into(),
        Msg::NotInConfig { id } => format!("配置里没有 {id}").into(),
        Msg::AccountModelsManaged { account } => format!("{account} 的模型归登录管理").into(),
        Msg::ModelNameCannotBeEmpty => "模型名不能是空的".into(),
        Msg::ManagedCannotEdit { id } => format!("{id} 归登录管理,这儿改不了").into(),
        Msg::ManagedCannotDelete { id } => format!("{id} 归登录管理,这儿删不了").into(),

        // ── installing plugins, from the launcher's side (`atomcode-cli`) ──
        Msg::SeedMarketFetched { name, plugins } => format!("取下了自带的插件市场 {name}（{plugins} 个插件）").into(),
        Msg::SeedPluginInstalled { plugin, marketplace } => format!("装上了 {plugin}@{marketplace}").into(),
        Msg::UpdatedToday => "今天更新".into(),
        Msg::UpdatedYesterday => "昨天更新".into(),
        Msg::UpdatedDaysAgo { days } => format!("{days} 天前更新").into(),
        Msg::PluginInstalledVerb => "装好了".into(),
        Msg::PluginUpdatedVerb => "更新好了".into(),
        Msg::UninstallFailed { error } => format!("卸不掉:{error}").into(),
        Msg::Uninstalled { id } => format!("卸掉了 {id}").into(),
        Msg::MarketAddFailed { error } => format!("加不上:{error}").into(),
        Msg::CancelledNothingLeft { what } => format!("取消了,{what} 没有留下").into(),
        Msg::MarketCarriesNothing => "它没带插件".into(),
        Msg::MarketCarriesSome { n, names } => format!("它带着 {n} 个插件:{names} …").into(),
        Msg::MarketCarriesAll { n, names } => format!("它带着 {n} 个插件:{names}").into(),
        Msg::MarketAdded { name, source, carries } => format!("加上了市场 {name}（{source}）。{carries}。装还是要一个个装:`/plugin install <名字>`,或者在面板里按 ⏎").into(),
        Msg::MarketUpdateFailed { error } => format!("更新不了:{error}").into(),
        Msg::MarketUpdated { name, commit, plugins } => format!("市场 {name} 更新到 {commit},带着 {plugins} 个插件").into(),
        Msg::MarketRemoveFailed { error } => format!("删不掉:{error}").into(),
        Msg::MarketRemoved { name } => format!("删掉了市场 {name},连同从它装的插件").into(),
        Msg::MarketRemovedWithLeftovers { name, failed } => format!("删掉了市场 {name},但这几个没卸干净:{failed}").into(),
        Msg::PluginInstallFailed { error } => format!("没装上:{error}").into(),
        Msg::TallySkills { n } => format!("{n} 个技能").into(),
        Msg::TallyCommands { n } => format!("{n} 条命令").into(),
        Msg::TallyHooks => "钩子".into(),
        Msg::TallyBrought { what } => format!(",带来 {what}").into(),
        Msg::JobDidNotStart { error } => format!("这件活没跑起来:{error}").into(),
        Msg::ListJoiner => "、".into(),

        // ── signing in, from the screen (`atomcode-cli`) ──
        Msg::CmdAboutTuiLogin => "登录并配好 provider；已登录则刷新 codingplan 配置".into(),
        Msg::SetupThreadDied => "codingplan setup 线程崩了".into(),
        Msg::ConfigWrittenReloadFailedCli { error } => format!("配置已写入，但重新加载失败：{error}").into(),
        Msg::SignedInSettingUpProvider => "登录成了，正在配 provider…".into(),
        Msg::AlreadySignedInRefreshing => "已登录，正在刷新 codingplan 配置…".into(),
        Msg::TokenRejectedSigningInAgain => "服务端不认这个 token 了，重新登录一次…".into(),
        Msg::SignedInAgainRerunningSetup => "重新登录成了，重跑 setup…".into(),
        Msg::ConfigNotWritten { error } => format!("配置没写成：{error}").into(),
        Msg::LoginCouldNotStart { error } => format!("登录起不来：{error}").into(),
        Msg::LoginFailed { error } => format!("登录没成：{error}").into(),
        Msg::LoginNoAnswer => "登录没了回音".into(),
        Msg::TokenExchangeFailed { error } => format!("换 token 没成：{error}").into(),
        Msg::SignedInCredentialsNotSaved { error } => format!("登录成了，但凭据没写成：{error}").into(),
        Msg::ScanOrOpen => "用手机扫码，或在浏览器里打开：".into(),
        Msg::InternalError { error } => format!("internal error：{error}").into(),

        // ── the first-run wizard (`atomcode-cli`) ──
        Msg::OnboardIntroTitle => "先把这台机器配好".into(),
        Msg::OnboardIntroLine1 => "还没有可用的 provider，所以现在还不能开始干活。".into(),
        Msg::OnboardIntroLine2 => "选界面语言、定 provider 怎么来、看一眼配好了什么。".into(),
        Msg::OnboardIntroLine3 => "任何一步都可以按 esc 退出，之后 /onboarding 再来。".into(),
        Msg::OnboardLanguageTitle => "界面语言".into(),
        Msg::OnboardLanguageChinese => "中文".into(),
        Msg::OnboardLanguageFollowSystem => "跟随系统".into(),
        Msg::OnboardLanguageFollowSystemAbout => "按环境变量判断".into(),
        Msg::OnboardSetupTitle => "provider 怎么来".into(),
        Msg::OnboardSetupCodingPlan => "登录 CodingPlan".into(),
        Msg::OnboardSetupCodingPlanAbout => "扫一下码，免费额度跟着来".into(),
        Msg::OnboardSetupManual => "自己配一个".into(),
        Msg::OnboardSetupManualAbout => "手里有 API key，或者自己部署的模型".into(),
        Msg::OnboardSetupSkip => "先不配".into(),
        Msg::OnboardSetupSkipAbout => "这台机器仍然干不了活".into(),
        Msg::OnboardSetupByHand => "接下来开 provider 面板：API key 、地址、模型都在那里填".into(),
        Msg::OnboardWouldClearTitle => "这会清屏".into(),
        Msg::OnboardWouldClearLine1 => "这里已经有在进行的对话。登录成功之后会开一个新会话，现在屏上的东西就不在了。".into(),
        Msg::OnboardWouldClearLine2 => "回车继续，esc 算了。".into(),
        Msg::OnboardLoginTitle => "登录".into(),
        Msg::OnboardFetchingLoginUrl => "正在取登录地址…".into(),
        Msg::OnboardConfirmTitle => "配好了".into(),
        Msg::OnboardModalTitle => "开始之前".into(),
        Msg::OnboardAlreadyFinished => "引导已经结束了".into(),
        Msg::OnboardLanguageSet { language } => format!("界面语言：{language}").into(),
        Msg::OnboardLanguageNotWritten { error } => format!("界面语言没写进去：{error}").into(),
        Msg::OnboardLoginSkipped => "跳过了登录——还是没有 provider，/onboarding 可以再来".into(),
        Msg::OnboardSkipHint => "不想现在弄的话，回车跳过。".into(),
        Msg::OnboardSignedInConfigNotWritten { error } => format!("登录成了，但配置没写成：{error}").into(),
        Msg::CmdAboutOnboarding => "把这台机器配到能干活：语言、登录、看一眼结果".into(),
        Msg::CmdAboutOnboardingFinished => "引导走完了".into(),
        Msg::ClassicOnlyForNow { command } => format!(
            "/{command} 暂时只在经典界面里有：退出后用 `atomcode --classic` 打开（会话是同一份）。\
             新界面的版本在做。"
        )
        .into(),
        Msg::CmdAboutClassicOnly => "暂时只在经典界面里有".into(),
        Msg::LivesInTheCli { command, run } => {
            format!("/{command} 归命令行：退出后运行 `{run}`。").into()
        }
        Msg::CmdAboutInTheCli => "归命令行".into(),
        Msg::ShareNoModel => "还没有配好模型——先 /login 或 /model".into(),
        Msg::CmdAboutWebui => "把这个会话共享给浏览器(lan 暴露到局域网;stop 结束)".into(),
        Msg::WebuiTakes => "[lan | --host <地址> | stop]".into(),
        Msg::CmdAboutSync => "共享这个会话,但什么也不打开(off 停止)".into(),
        Msg::SyncTakes => "[off]".into(),
        Msg::CmdAboutDesktop => "打开桌面端".into(),
        Msg::CmdAboutApp => "把这个会话共享给手机 App(stop 结束)".into(),
        Msg::AppTakes => "[stop]".into(),
        Msg::AppRelayDisabled => "本部署关掉了远程访问(ATOMCODE_ENABLE_RELAY=0)".into(),
        Msg::AppRelayNotStarted { error, path } => {
            format!("中继客户端没启动起来({error});试过的路径是 `{path}`").into()
        }
        Msg::AppStopped => "手机已经连不到这个会话了。".into(),
        Msg::AppWasNotOn => "本来手机就连不到。".into(),
        Msg::AppPairTitle => "配对手机".into(),
        Msg::AppPairScan => "在 GitCode App 里:首页 → AtomCode → 扫一扫,对准这张码。".into(),
        Msg::AppPairType => "扫不了的话,把这串口令粘进 App:".into(),
        Msg::AppPaired => "配对码给出去了——App 扫到就进这个会话;断开用 `/app stop`。".into(),
        Msg::CmdAboutAppPaired => "配对屏关掉了".into(),
        Msg::RelayNeedsLogin => "中继客户端要从登录后的 release 里取——先 /login".into(),
        Msg::RelayUnsupportedPlatform { os, arch, dir } => {
            format!("{os}/{arch} 没有发布中继客户端——自己编一个放到 {dir}").into()
        }
        Msg::RelayDownloadOff { dir } => format!(
            "自动下载关着(ATOMCODE_RELAY_CLIENT_SKIP_DOWNLOAD=1)——把中继客户端放到 {dir}"
        )
        .into(),
        Msg::RelayDownloadFailed {
            error,
            dir,
            releases,
            install,
        } => format!(
            "中继客户端没下下来:{error}\n\n\
             自己装:\n\
             1. 打开 {releases}\n\
             2. 下对应平台的那个 binary\n\
             3. 存成 {dir}/atomcode-relay-client 并加上可执行权限\n\
             4. 重新 /app\n\n\
             或者一行装好:\n{install}"
        )
        .into(),
        Msg::ShareStarted => "这个会话已共享——浏览器或手机上看到的是同一段对话。".into(),
        Msg::ShareStopped => "已停止共享这个会话。".into(),
        Msg::ShareWasNotOn => "本来就没有在共享。".into(),
        Msg::DesktopOpening { name, path } => format!("正在打开 {name}({path})").into(),
        Msg::DesktopLaunchFailed { path, error } => format!("{path} 没能启动:{error}").into(),
        Msg::DesktopNotInstalled { url } => format!("这台机器上没装桌面端——{url}").into(),
        Msg::CmdAboutSchedule => "排了哪些定时任务，各自下次什么时候跑".into(),
        Msg::CmdAboutOpenRouter => "接上 OpenRouter 的免费模型".into(),
        Msg::OpenRouterTakes => "[api key]".into(),
        Msg::OpenRouterConnecting => "正在接 OpenRouter…".into(),
        Msg::OpenRouterAuthorise { url } => {
            format!("去浏览器里授权。没自动打开的话:{url}").into()
        }
        Msg::OpenRouterNoAnswer => "浏览器那边没有回应——取消了,或者超时了。".into(),
        Msg::OpenRouterNoFreeModels => "OpenRouter 没有返回可用的免费模型。".into(),
        Msg::OpenRouterConnected { added, default } => {
            format!("OpenRouter 接上了:新增 {added} 个免费模型,当前用的是 `{default}`。").into()
        }
        Msg::OpenRouterNotReloaded { error } => {
            format!("OpenRouter 接上并已保存,但这次会话没重载({error});下次启动生效。").into()
        }
        Msg::OpenRouterFailed { error } => format!("OpenRouter 没接上:{error}").into(),
        Msg::ScheduleNone => "还没有定时任务。用 `atomcode schedule add` 排一个。".into(),
        Msg::ScheduleTaskLine {
            id,
            title,
            next,
            last,
            state,
        } => format!("{id} · {title} · 下次 {next} · 上次 {last} · {state}").into(),
        Msg::ScheduleOn => "开".into(),
        Msg::ScheduleOff => "关".into(),
        Msg::ScheduleEditInTheCli => {
            "增删用 `atomcode schedule add` / `remove`；到点执行的是操作系统的调度器。".into()
        }
        Msg::CmdAboutProxy => "出站代理：跟随系统、固定当前代理，或不走代理".into(),
        Msg::ProxyTakes => "[follow_system | default_proxy | no_proxy]".into(),
        Msg::ProxyPickerTitle { current } => format!("出站代理（现在：{current}）").into(),
        Msg::ProxyFollowSystemAbout => "用启动时环境里的代理，缺的由系统代理补上（默认）".into(),
        Msg::ProxyDefaultProxyAbout { captured } => {
            format!("把这次启动时的代理固定写进配置，以后每次都用它：{captured}").into()
        }
        Msg::ProxyNoProxyAbout => "所有出站请求都不走代理".into(),
        Msg::ProxySet { summary } => format!("出站代理已改为 {summary}，模型连接已重建。").into(),
        Msg::ProxySetNotReconnected { summary, error } => format!(
            "出站代理已改为 {summary} 并已保存，但模型连接没有重建（{error}）；下次建连接时生效。"
        )
        .into(),
        Msg::ProxyUnknown { wanted } => {
            format!("没有叫「{wanted}」的代理模式：可选 follow_system、default_proxy、no_proxy。").into()
        }
        Msg::ProxySaveFailed { error } => format!("代理设置没写进配置文件：{error}").into(),
        Msg::SteeringQueued => "将在下一次工具调用后提交的消息（按 Ctrl+X 中断并立即发送）".into(),
        Msg::RemoteRan { command } => format!("（另一端执行了 {command}）").into(),
        Msg::RemoteDesktopOnly => "这条得在这台机器上敲。另一端能用的是：".into(),
        Msg::UsageUnknown { why } => format!("问不到还剩多少：{why}").into(),
        Msg::CopyHandedOver => "（交给了终端；它要是不让复制，这次就没有）".into(),
        Msg::ContextNoPrompt => "这个宿主没有系统提示词可说。".into(),
        Msg::ComposerSuggested { text } => format!("按 → 接着说：{text}").into(),
        Msg::FileTooBigToPaste { path, size, cap } => format!(
            "{path} 有 {size}，粘不进来（上限 {cap}）。编辑区里的东西每一轮都要重发；             让模型自己去读这个文件，它只会读要用的那几行。"
        )
        .into(),
    }
}
