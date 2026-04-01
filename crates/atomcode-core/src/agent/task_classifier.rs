//! Task classifier — determines task type from user message.
//!
//! Drives three decisions:
//! 1. Which system prompt rules to inject (dynamic prompt)
//! 2. Whether to force planning (three-stage turn loop)
//! 3. Whether to restrict first-turn tools (read-only)
//!
//! Tech-stack agnostic — classification based on natural language features only.

/// Task type determined from user message.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskType {
    /// Simple question — no tools needed or minimal reading.
    /// "这个是批量还是单个？", "为什么用 Redis？"
    Question,

    /// Single-file modification with clear target.
    /// "把 App.vue 的背景改成蓝色", "修改 SecurityConfig 加上 /api/tags"
    SingleFileEdit,

    /// Bug fix — something doesn't work, need to diagnose.
    /// "点击标签找不到文章", "报错 500", "页面空白"
    BugFix,

    /// Feature development — add new functionality, multi-file.
    /// "给发现页面加标签筛选", "实现用户注册功能"
    FeatureDev,

    /// Follow-up fix — user says previous attempt didn't work.
    /// "还是不行", "依然不行", "没有成功"
    FollowUp,

    /// Command execution — user wants to run something.
    /// "重启", "编译", "部署", "安装依赖"
    Command,
}

impl TaskType {
    /// Whether this task type should trigger forced planning.
    pub fn needs_planning(&self) -> bool {
        matches!(self, TaskType::BugFix | TaskType::FeatureDev)
    }

    /// Whether first-turn tools should be restricted to read-only.
    pub fn restrict_first_turn(&self) -> bool {
        matches!(self, TaskType::BugFix | TaskType::FeatureDev)
    }
}

/// Classify a user message into a task type.
/// Uses keyword matching + message length + structural features.
/// No LLM call — pure heuristic for zero latency.
pub fn classify(message: &str, has_previous_turns: bool) -> TaskType {
    let msg = message.trim();
    let lower = msg.to_lowercase();
    let char_count = msg.chars().count();

    // Rule 1: Follow-up — message indicating previous attempt failed.
    // Must have previous turns in conversation. Check before other rules
    // because "还是不行" also contains bug keywords.
    if has_previous_turns {
        let followup_keywords = [
            "还是不行", "依然不行", "没有成功", "还是不对", "还是报错",
            "还是没有", "依然没有", "还是空", "依然不能",
            "没用", "还是一样", "又错了", "又报错",
            "still broken", "still not", "doesn't work", "not working",
            "same error", "still failing",
        ];
        if followup_keywords.iter().any(|kw| lower.contains(kw)) {
            return TaskType::FollowUp;
        }
    }

    // Rule 2: Command — direct action request.
    if char_count < 30 {
        let command_keywords = [
            "重启", "编译", "部署", "安装", "启动", "运行", "构建",
            "restart", "compile", "deploy", "install", "start", "build", "run",
        ];
        if command_keywords.iter().any(|kw| lower.contains(kw)) {
            return TaskType::Command;
        }
    }

    // Rule 3: Question — asking for information, not action.
    // Must not contain bug indicators (prioritize bug fix over question).
    let has_bug_signal = lower.contains("不行") || lower.contains("报错")
        || lower.contains("失败") || lower.contains("空白") || lower.contains("空的")
        || lower.contains("不显示") || lower.contains("找不到") || lower.contains("不能")
        || lower.contains("没有显示") || lower.contains("不出来") || lower.contains("不准")
        || lower.contains("error") || lower.contains("fail") || lower.contains("empty")
        || lower.contains("broken") || lower.contains("missing");
    let is_question = (msg.ends_with('?') || msg.ends_with('？')
        || lower.contains("是什么") || lower.contains("为什么")
        || lower.contains("怎么来的") || lower.contains("如何")
        || lower.contains("是否") || lower.contains("有没有")
        || lower.contains("还是") // "A还是B" pattern = question
        || lower.contains("what ") || lower.contains("why ")
        || lower.contains("how ") || lower.contains("is there"))
        && !has_bug_signal;
    if is_question && char_count < 100 {
        return TaskType::Question;
    }

    // Rule 4: Single file edit — mentions a specific file.
    let has_file_ref = lower.contains(".vue") || lower.contains(".java")
        || lower.contains(".ts") || lower.contains(".py")
        || lower.contains(".rs") || lower.contains(".go")
        || lower.contains(".js") || lower.contains(".css")
        || lower.contains(".html") || lower.contains(".json")
        || lower.contains(".yml") || lower.contains(".xml");
    let is_simple_edit = has_file_ref && char_count < 100;
    if is_simple_edit {
        return TaskType::SingleFileEdit;
    }

    // Rule 5: Bug fix — something is broken.
    let bug_keywords = [
        "报错", "错误", "不行了", "失败", "空白", "不显示", "不工作",
        "找不到", "没有显示", "不能", "无法", "异常", "崩溃",
        "没有成功", "没成功", "空的", "不出来", "没有配置",
        "点击", "点了", "打开", // user actions that imply something failed
        "error", "fail", "broken", "crash", "blank", "empty",
        "not showing", "doesn't", "can't", "unable", "missing",
    ];
    if bug_keywords.iter().any(|kw| lower.contains(kw)) {
        return TaskType::BugFix;
    }

    // Rule 6: Feature development — building something new.
    let feature_keywords = [
        "实现", "添加", "创建", "新增", "开发", "功能", "设计",
        "重构", "优化", "改造", "升级", "迁移",
        "implement", "add", "create", "build", "develop", "feature",
        "refactor", "redesign", "migrate",
    ];
    if feature_keywords.iter().any(|kw| lower.contains(kw)) {
        return TaskType::FeatureDev;
    }

    // Rule 7: Long message without clear category → likely feature dev.
    if char_count > 100 {
        return TaskType::FeatureDev;
    }

    // Default: treat as bug fix (the most common case for weak models).
    TaskType::BugFix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_question() {
        assert_eq!(classify("这个是批量还是单个调用？", false), TaskType::Question);
        assert_eq!(classify("为什么用 Redis？", false), TaskType::Question);
    }

    #[test]
    fn test_single_file_edit() {
        assert_eq!(classify("把 App.vue 的背景改成蓝色", false), TaskType::SingleFileEdit);
    }

    #[test]
    fn test_bug_fix() {
        assert_eq!(classify("点击标签找不到文章", false), TaskType::BugFix);
        assert_eq!(classify("页面空白什么都没有", false), TaskType::BugFix);
    }

    #[test]
    fn test_follow_up() {
        assert_eq!(classify("还是不行", true), TaskType::FollowUp);
        assert_eq!(classify("依然不行", true), TaskType::FollowUp);
        // Without previous turns, not a follow-up
        assert_ne!(classify("还是不行", false), TaskType::FollowUp);
    }

    #[test]
    fn test_command() {
        assert_eq!(classify("重启", false), TaskType::Command);
        assert_eq!(classify("编译一下", false), TaskType::Command);
    }

    #[test]
    fn test_feature_dev() {
        assert_eq!(classify("给发现页面添加标签筛选功能", false), TaskType::FeatureDev);
    }
}
