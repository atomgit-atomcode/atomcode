//! Conservative task classification. This selects at most one static workflow;
//! it neither executes a skill nor grants authority to its tools.
use super::{AutoLoad, Skill, SkillRegistry};
use std::sync::Arc;

pub enum Selection {
    Load(Arc<Skill>),
    Skip(&'static str),
}

/// Exclude quoted examples, code and blockquotes: a report embedded in a
/// translation/documentation request is not itself a debugging instruction.
fn task_text(prompt: &str) -> String {
    let mut out = String::new();
    let mut fence = None;
    for line in prompt.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            let mark = &line[..3];
            if fence == Some(mark) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(mark);
            }
            continue;
        }
        if fence.is_some() || line.starts_with('>') {
            continue;
        }
        let mut quote = None;
        let mut previous = ' ';
        for ch in line.chars() {
            if let Some(end) = quote {
                if ch == end {
                    quote = None;
                }
            } else if matches!(ch, '"' | '`' | '“' | '‘')
                || (ch == '\'' && !previous.is_alphanumeric())
            {
                quote = Some(match ch {
                    '“' => '”',
                    '‘' => '’',
                    other => other,
                });
            } else {
                out.extend(ch.to_lowercase());
            }
            previous = ch;
        }
        out.push('\n');
    }
    out
}

fn action_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        !phrase.is_ascii()
            || at == 0
            || !text[..at]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

fn opted_out(text: &str) -> bool {
    [
        "[no-auto-skills]",
        "不要用skill",
        "不要用 skill",
        "不要使用skill",
        "不要使用 skill",
        "不要加载技能",
        "不要使用技能",
        "不要用技能",
        "不要调用技能",
        "不要调用skill",
        "不要调用 skill",
        "不用技能",
        "不使用技能",
        "do not use skill",
        "don't use skill",
        "without skill",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Select guidance for a clear UI task, never infer permission to operate a
/// browser from a mention in a question, code change or pasted example.
pub fn browser_task_reason(prompt: &str) -> Result<(), &'static str> {
    if prompt.len() > 32 * 1024 {
        return Err("prompt_budget");
    }
    let text = task_text(prompt);
    if opted_out(&text) {
        return Err("user_opt_out");
    }
    if [
        "不用浏览器",
        "不要使用浏览器",
        "不要操作浏览器",
        "without browser",
        "do not use the browser",
    ]
    .iter()
    .any(|word| text.contains(word))
    {
        return Err("browser_opt_out");
    }
    let mut instruction = text.trim_start();
    for prefix in ["请帮我", "帮我", "请", "please ", "could you ", "can you "] {
        if let Some(rest) = instruction.strip_prefix(prefix) {
            instruction = rest.trim_start();
        }
    }
    if [
        "不要",
        "禁止",
        "不允许",
        "先别",
        "别用",
        "don't ",
        "do not ",
        "never ",
    ]
    .iter()
    .any(|prefix| instruction.starts_with(prefix))
    {
        return Err("browser_opt_out");
    }
    if [
        "如何",
        "怎么",
        "是否",
        "能否",
        "解释",
        "说明",
        "介绍",
        "翻译",
        "总结",
        "写文档",
        "写个",
        "实现",
        "开发",
        "增加",
        "添加",
        "修复",
        "排查",
        "调试",
        "优化",
        "what ",
        "why ",
        "how ",
        "explain ",
        "describe ",
        "translate ",
        "summarize ",
        "write ",
        "implement ",
        "develop ",
        "add ",
        "fix ",
        "debug ",
        "investigate ",
    ]
    .iter()
    .any(|prefix| instruction.starts_with(prefix))
        || ["如何", "怎么", "是否", "能否", "会不会", "原理"]
            .iter()
            .any(|word| text.contains(word))
        || (["增加", "添加", "实现", "修复", "重构", "优化", "开发"]
            .iter()
            .any(|word| text.contains(word))
            && ["功能", "支持", "逻辑", "组件", "测试", "事件"]
                .iter()
                .any(|word| text.contains(word)))
        || text.contains('?')
        || text.contains('？')
        || [
            "代码",
            "源码",
            "脚本",
            "code change",
            "source code",
            "implement a",
            "write a script",
            "playwright",
            "selenium",
            "puppeteer",
            "applescript",
            "osascript",
        ]
        .iter()
        .any(|word| text.contains(word))
    {
        return Err("not_browser_instruction");
    }
    let target = [
        "网页",
        "网站",
        "页面",
        "浏览器",
        "browser",
        "website",
        "webpage",
        "web page",
        "https://",
        "http://",
    ]
    .iter()
    .any(|word| {
        if !word.is_ascii() || word.ends_with("://") {
            return text.contains(word);
        }
        text.match_indices(word).any(|(at, _)| {
            let boundary = |ch: char| !ch.is_ascii_alphanumeric() && ch != '_';
            (at == 0 || text[..at].chars().next_back().is_some_and(boundary))
                && text[at + word.len()..].chars().next().is_none_or(boundary)
        })
    });
    let action = [
        "点击",
        "双击",
        "单击",
        "拖动",
        "滚动",
        "输入",
        "填写",
        "按下",
        "切换",
        "打开",
        "操作",
        "控制",
        "查看",
        "截取",
        "截图",
        "click ",
        "drag ",
        "scroll ",
        "type ",
        "fill ",
        "press ",
        "switch ",
        "open ",
        "operate ",
        "control ",
        "inspect ",
        "capture ",
        "take a screenshot",
    ]
    .iter()
    .any(|p| action_phrase(&text, p));
    if target && action {
        Ok(())
    } else {
        Err("no_browser_match")
    }
}

pub fn task_reason(prompt: &str) -> Result<(), &'static str> {
    if prompt.len() > 32 * 1024 {
        return Err("prompt_budget");
    }
    let text = task_text(prompt);
    if opted_out(&text) {
        return Err("user_opt_out");
    }
    let mut instruction = text.trim_start();
    for prefix in ["请帮我", "帮我", "请", "please ", "could you ", "can you "] {
        if let Some(rest) = instruction.strip_prefix(prefix) {
            instruction = rest.trim_start();
        }
    }
    if [
        "翻译",
        "总结",
        "格式化",
        "重命名",
        "改名",
        "生成发布日志",
        "translate ",
        "summarize ",
        "format ",
        "rename ",
        "write release notes",
    ]
    .iter()
    .any(|prefix| {
        instruction
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.starts_with("后") && !rest.starts_with("之后"))
    }) {
        return Err("simple_task");
    }
    let action = [
        "修复",
        "排查",
        "调试",
        "查原因",
        "定位",
        "fix ",
        "investigate ",
        "diagnose ",
    ]
    .iter()
    .any(|needle| action_phrase(&text, needle))
        || text.trim_start().starts_with("debug ");
    if [
        "错别字",
        "typo",
        "仅修改文案",
        "只改文案",
        "只改注释",
        "comment only",
        "修复文档",
        "修正文档",
        "fix documentation",
        "fix this documentation",
        "fix the documentation",
        "fix docs",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        return Err("simple_task");
    }
    let simple = [
        "翻译",
        "总结",
        "格式化",
        "重命名",
        "改名",
        "写文档",
        "发布日志",
        "translate",
        "summarize",
        "format only",
        "formatting only",
        "rename",
        "write docs",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    if simple && !action {
        return Err("simple_task");
    }
    let failure = [
        "测试失败",
        "编译失败",
        "运行报错",
        "test failed",
        "tests failed",
        "test is failing",
        "failing test",
        "build failed",
        "panic:",
        "traceback",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    let symptom = [
        "报错",
        "bug",
        "故障",
        "错误",
        "不生效",
        "没生效",
        "失败",
        "算错",
        "不对",
        "卡住",
        "崩溃",
        "闪退",
        "crash",
        "regression",
        "wrong",
        "error",
        "exception",
        "fails",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    let question = [
        "是不是",
        "为什么",
        "怎么会",
        "帮我查",
        "帮我看看",
        "请检查",
        "why ",
        "please check",
        "help me",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    if failure || (symptom && (action || question)) {
        Ok(())
    } else {
        Err("no_confident_match")
    }
}

pub fn select(registry: &SkillRegistry, prompt: &str) -> Selection {
    let browser_task = browser_task_reason(prompt).is_ok();
    let skills = registry.all();
    let browser = browser_task && skills.iter().any(|s| s.auto_load == AutoLoad::BrowserUse);
    if !browser {
        if let Err(reason) = task_reason(prompt) {
            return Selection::Skip(if browser_task {
                "no_eligible_skill"
            } else {
                reason
            });
        }
    }
    let mut candidates = Vec::new();
    for skill in skills {
        let rank = match skill.auto_load {
            AutoLoad::Never => continue,
            AutoLoad::BrowserUse if browser => 2,
            AutoLoad::BrowserUse => continue,
            _ if browser => continue,
            AutoLoad::Debugging => 2,
            AutoLoad::Unspecified => {
                let known = skill.name.rsplit(':').next() == Some("systematic-debugging")
                    || skill.name == "atomcode-workflows:debug";
                let description = skill.description.to_lowercase();
                if !known
                    || !["debug", "bug", "test fail", "调试", "故障"]
                        .iter()
                        .any(|word| description.contains(word))
                {
                    continue;
                }
                1
            }
        };
        candidates.push((rank, skill));
    }
    let Some(best) = candidates.iter().map(|(rank, _)| *rank).max() else {
        return Selection::Skip("no_eligible_skill");
    };
    let mut best = candidates.into_iter().filter(|(rank, _)| *rank == best);
    let skill = best.next().unwrap().1;
    if best.next().is_some() {
        Selection::Skip("ambiguous_skills")
    } else {
        Selection::Load(skill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn skill(name: &str, policy: AutoLoad) -> Skill {
        Skill {
            name: name.into(),
            display_name: None,
            description: "Investigate bugs and failed tests".into(),
            template: "DEBUG_WORKFLOW_BODY: establish evidence before changing code.".into(),
            allowed_tools: vec!["bash".into()],
            user_invocable: true,
            auto_load: policy,
            args_requirement: Default::default(),
            skill_dir: "/skills/debug".into(),
            source_path: "/skills/debug/SKILL.md".into(),
        }
    }
    #[test]
    fn clear_reports_match_and_simple_or_quoted_tasks_do_not() {
        for text in [
            "测试失败了，帮我查原因并修好",
            "第一次重试等了200ms，是不是算错了",
            "修复后仍然报错，请排查",
            "Why does this test fail with an error?",
            "Fix this regression",
            "格式化后程序崩溃，请修复",
            "修复重命名后崩溃的问题",
        ] {
            assert_eq!(task_reason(text), Ok(()), "{text}");
        }
        for text in [
            "重命名单个文件",
            "仅格式化当前文件",
            "Translate 'test failed' into Chinese",
            "请总结这段日志：\n```\ntest failed panic:\n```",
            "翻译：\n> 测试失败了，修复bug",
            "请说明 `test failed` 的意思",
            "第一次重试等了200ms",
            "Explain this prefix error",
            "修复错误提示里的错别字",
            "Fix this documentation error",
            "帮我总结这次修复的bug",
            "Please summarize these test failures",
            "翻译测试失败的原因并保留fix这个词",
        ] {
            assert!(task_reason(text).is_err(), "{text}");
        }
    }
    #[test]
    fn explicit_opt_out_is_unquoted_and_direct_change_is_not_an_opt_out() {
        for text in [
            "[no-auto-skills] 测试失败，帮我修复",
            "不要用技能，修复这个bug",
            "Don't use skills. Fix this error",
        ] {
            assert_eq!(task_reason(text), Err("user_opt_out"));
        }
        assert_eq!(task_reason("修复这个bug，直接改"), Ok(()));
        assert_eq!(
            task_reason("修复这个bug，日志里的文字是\"不要用技能\""),
            Ok(())
        );
    }
    #[test]
    fn explicit_metadata_wins_and_ties_do_not_pick_an_arbitrary_skill() {
        let registry = SkillRegistry::new();
        registry.register(skill("systematic-debugging", AutoLoad::Unspecified));
        assert!(
            matches!(select(&registry, "修复这个bug"), Selection::Load(s) if s.name == "systematic-debugging")
        );
        registry.register(skill("team:diagnose", AutoLoad::Debugging));
        assert!(
            matches!(select(&registry, "修复这个bug"), Selection::Load(s) if s.name == "team:diagnose")
        );
        registry.register(skill("other:debug", AutoLoad::Debugging));
        assert!(matches!(
            select(&registry, "修复这个bug"),
            Selection::Skip("ambiguous_skills")
        ));
        let disabled = SkillRegistry::new();
        disabled.register(skill("systematic-debugging", AutoLoad::Never));
        assert!(matches!(
            select(&disabled, "修复这个bug"),
            Selection::Skip("no_eligible_skill")
        ));
    }
    #[test]
    fn clear_browser_tasks_choose_only_the_browser_policy() {
        let registry = SkillRegistry::new();
        registry.register(skill("debug", AutoLoad::Debugging));
        registry.register(skill("browser-use", AutoLoad::BrowserUse));
        for prompt in [
            "帮我在网页中点击登录按钮",
            "请操作桌面浏览器，填写当前窗口的搜索框",
            "Please click the submit button in the browser",
            "Could you open https://example.com and fill the search box",
            "查看当前网页的状态",
            "Take a screenshot of the current browser page",
            "在浏览器中打开文档并滚动到页面底部",
            "打开 https://example.test 并截图",
            "打开网页并填写表单，不要提交",
        ] {
            assert_eq!(browser_task_reason(prompt), Ok(()), "{prompt}");
            assert!(
                matches!(select(&registry, prompt), Selection::Load(s) if s.name == "browser-use"),
                "{prompt}"
            );
        }
        assert!(
            matches!(select(&registry, "修复 Safari 点击按钮报错的问题"), Selection::Load(s) if s.name == "debug")
        );
        registry.register(skill("second-browser", AutoLoad::BrowserUse));
        assert!(matches!(
            select(&registry, "点击浏览器的按钮"),
            Selection::Skip("ambiguous_skills")
        ));
    }

    #[test]
    fn absent_browser_policy_keeps_existing_debug_selection() {
        let registry = SkillRegistry::new();
        registry.register(skill("debug", AutoLoad::Debugging));
        assert!(
            matches!(select(&registry, "在浏览器窗口点击按钮后报错，请排查"), Selection::Load(s) if s.name == "debug")
        );
    }

    #[test]
    fn browser_mentions_examples_code_and_refusals_do_not_trigger() {
        for prompt in [
            "如何在 Safari 中点击按钮？",
            "这个应用如何点击按钮",
            "Safari 点击按钮会不会关闭窗口",
            "帮我解释浏览器点击的实现原理",
            "翻译：在浏览器中点击提交",
            "请总结这段操作说明：在浏览器中点击按钮",
            "说明如下：\n> 点击浏览器的按钮",
            "记录示例：\n```\n点击浏览器的按钮\n```",
            "Explain 'click the browser button'",
            "帮我实现浏览器点击按钮的代码",
            "Add browser click automation support",
            "给浏览器增加点击支持",
            "为窗口添加点击事件",
            "使用 Playwright 点击浏览器按钮",
            "帮我修复应用窗口点击崩溃的问题",
            "Do not click the browser button",
            "Don't operate Safari",
            "不要操作桌面浏览器",
            "禁止点击应用按钮",
            "不用桌面，只说明如何点击浏览器",
            "[no-auto-skills] 点击浏览器按钮",
            "不要用技能，点击浏览器按钮",
            "Open SafariUtils and click something",
            "修改 site/index.html",
            "打开 Finder 窗口",
            "点击系统设置中的按钮",
            "Take a screenshot of the current app window",
        ] {
            assert!(browser_task_reason(prompt).is_err(), "{prompt}");
        }
        let registry = SkillRegistry::new();
        registry.register(skill("browser-use", AutoLoad::Never));
        assert!(matches!(
            select(&registry, "点击浏览器按钮"),
            Selection::Skip("no_eligible_skill")
        ));
        let empty = SkillRegistry::new();
        assert!(matches!(
            select(&empty, "点击浏览器按钮"),
            Selection::Skip("no_eligible_skill")
        ));
    }

    #[test]
    fn static_expansion_is_bounded_and_never_executes_dynamic_blocks() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("should-not-exist");
        let mut s = skill("debug", AutoLoad::Debugging);
        s.template = format!("!`echo injected > {}`", marker.display());
        assert_eq!(
            s.expand_static_for_injection("session"),
            Err("dynamic_shell")
        );
        assert!(!marker.exists());
        s.template = "Use $ARGUMENTS to fix the issue".into();
        assert_eq!(
            s.expand_static_for_injection("session"),
            Err("arguments_required")
        );
        s.template = "x".repeat(12 * 1024 + 1);
        assert_eq!(s.expand_static_for_injection("session"), Err("body_budget"));
        s.template = "Session ${CLAUDE_SESSION_ID}; resources ${CLAUDE_SKILL_DIR}".into();
        let expanded = s.expand_static_for_injection("session-1").unwrap();
        assert!(
            expanded.contains("session-1")
                && expanded.contains("/skills/debug")
                && expanded.contains("installed at")
        );
    }
    #[test]
    fn disk_metadata_is_explicit_and_bad_values_disable_only_automatic_loading() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("systematic-debugging");
        std::fs::create_dir(&dir).unwrap();
        for (value, expected) in [
            ("debugging", AutoLoad::Debugging),
            ("browser-use", AutoLoad::BrowserUse),
            ("off", AutoLoad::Never),
            ("typo", AutoLoad::Never),
            ("debugging\nauto-load: debugging", AutoLoad::Never),
        ] {
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: systematic-debugging\ndescription: Investigate bugs\nauto-load: {value}\n---\nBODY")).unwrap();
            let registry = SkillRegistry::load(&[root.path().to_path_buf()]);
            let loaded = registry.get("systematic-debugging").unwrap();
            assert_eq!(loaded.auto_load, expected);
            assert_eq!(loaded.expand("", ""), "BODY");
        }
    }
}
