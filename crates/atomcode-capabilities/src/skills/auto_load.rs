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

pub fn task_reason(prompt: &str) -> Result<(), &'static str> {
    if prompt.len() > 32 * 1024 {
        return Err("prompt_budget");
    }
    let text = task_text(prompt);
    if [
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
    {
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
    if let Err(reason) = task_reason(prompt) {
        return Selection::Skip(reason);
    }
    let mut candidates = Vec::new();
    for skill in registry.all() {
        let rank = match skill.auto_load {
            AutoLoad::Never => continue,
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
