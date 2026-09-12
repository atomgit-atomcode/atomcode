//! Asking the person, as a tool the model can call.
//!
//! The `user-questions` seam has been here all along — it is how an approval
//! reaches a screen — but nothing pointed it at the *model*. So an agent that
//! needed a decision only its person could make had exactly two moves: guess,
//! or stop and say it was blocked. It guessed, because guessing looks like
//! progress. That is not a personality: `request_user_input` was never mounted,
//! and [`crate::exec`] hands tools a context whose `requester` is `None`, so
//! the capabilities tool would have refused anyway. There was no way to ask.
//!
//! This row is the way. It does not reach the kernel's request channel: it
//! calls the same seam the approval gate calls, so a question from the model
//! and a question about a risky call arrive at the person the same way, get
//! drawn by the same row, and land in the transcript as the same kind of fact.
//!
//! **Every question is a choice.** The seam answers with one of the options it
//! was given — there is no free-text reply for a front end to collect — and
//! that constraint is worth keeping rather than working around: a question
//! whose alternatives the model cannot name is a question it has not thought
//! through, and the answer to it would be a paragraph nobody can act on.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::seams::{Answer, Question, UserQuestionsSvc};

use super::tools::{contribute_prompt, mount};

/// How many alternatives a question may offer.
///
/// Two, because one is not a choice. Five, because a person answering with a
/// number is reading a list, and past five they are doing a search.
const FEWEST: usize = 2;
const MOST: usize = 5;

struct AskTool {
    ctx: Context,
}

#[derive(Deserialize)]
struct Args {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Put a choice to the person and wait for their answer.\n\
         \n\
         For a decision that is theirs rather than yours: which of two designs to build, \
         whether to change something they did not ask you to touch, which of several things \
         they meant. NOT for anything the repository can tell you — read it instead; a \
         question you could have answered by looking costs them attention you will need later \
         for a question you cannot.\n\
         \n\
         Every question is a choice: give the real alternatives, 2 to 5 of them, each one a \
         thing you would actually do next. If you cannot name them, you do not know enough to \
         ask yet. The answer comes back as the option they picked.\n\
         \n\
         They may not answer — nobody is at the screen, or they declined. That is not consent \
         to anything: decide it yourself, say which way you went and why, and carry on.\n\
         \n\
         Example: {\"question\":\"`config.toml` has no `[telemetry]` section. Add one, or leave \
         telemetry off?\",\"options\":[\"add the section with defaults\",\"leave it off\"]}"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, in one or two sentences. State what you are about to do either way."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The alternatives, 2 to 5. Each is what you would do next if it is picked."
                }
            },
            "required": ["question", "options"]
        })
    }

    /// Safe: it changes nothing. A question that had to be approved before it
    /// could be asked would be two prompts for one decision.
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return fail(format!("invalid arguments: {e}")),
        };
        let question = args.question.trim().to_string();
        if question.is_empty() {
            return fail("`question` is required");
        }
        let options: Vec<String> = args
            .options
            .into_iter()
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .collect();
        if options.len() < FEWEST || options.len() > MOST {
            return fail(format!(
                "`options` must name {FEWEST} to {MOST} alternatives; got {}. A question \
                 whose alternatives you cannot name is one to answer by reading the code.",
                options.len()
            ));
        }

        let Some(questions) = self.ctx.service::<UserQuestionsSvc>() else {
            return ok(NOBODY);
        };
        let asked = Question {
            prompt: question,
            options: options.iter().map(Answer::new).collect(),
            // A member's question is not the conversation's question, and the
            // person answering is owed the difference.
            asker: crate::agent::current_member_name(&self.ctx),
            about: None,
        };
        match questions.ask(&asked).await {
            Some(answer) => ok(format!("The person answered: {answer}")),
            None => ok(NOBODY),
        }
    }
}

/// What the model is told when no answer came.
///
/// Not an error: nothing failed. And explicitly not consent — the sentence has
/// to leave the model with a next move, or a question nobody answered becomes
/// a turn that stops for no reason a person ever asked for.
const NOBODY: &str = "No answer — nobody was available, or they declined. This is not agreement \
                      with any option: decide it yourself, say which way you went and why, and \
                      carry on.";

fn ok(text: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: text.into(),
        is_error: false,
        images: vec![],
    }
}

fn fail(text: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: text.into(),
        is_error: true,
        images: vec![],
    }
}

pub struct AskPlugin;

#[async_trait]
impl Plugin for AskPlugin {
    fn name(&self) -> &'static str {
        "tool-ask"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Resolved per call rather than captured: the row that answers
        // questions is the front end, and which front end is running is not
        // this row's business.
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "`ask_user`: put a choice to the person and wait, through the same seam approvals use"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        mount(
            ctx,
            vec![Arc::new(AskTool { ctx: ctx.clone() }) as Arc<dyn Tool>],
        )?;
        contribute_prompt(
            ctx,
            "tool-ask",
            54,
            "When a decision is the person's rather than yours — which of two designs, whether \
             to touch something they did not ask about, which of several things they meant — \
             use `ask_user` rather than picking for them. Anything the repository can answer, \
             answer by reading it.",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::UserQuestions;
    use std::sync::Mutex;

    /// A person who always takes the last option, and remembers what they were
    /// asked.
    struct Picky(Arc<Mutex<Vec<Question>>>);

    #[async_trait]
    impl UserQuestions for Picky {
        fn describe(&self) -> String {
            "scripted human (takes the last option)".into()
        }
        async fn ask(&self, question: &Question) -> Option<String> {
            self.0
                .lock()
                .expect("asked poisoned")
                .push(question.clone());
            question.options.last().map(|a| a.value.clone())
        }
    }

    struct Silent;

    #[async_trait]
    impl UserQuestions for Silent {
        fn describe(&self) -> String {
            "nobody".into()
        }
        async fn ask(&self, _question: &Question) -> Option<String> {
            None
        }
    }

    fn tool_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            cancel: Default::default(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    async fn ask_with(provider: Arc<dyn UserQuestions>, args: &str) -> (ToolResult, Vec<Question>) {
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        let _held = ctx.provide::<UserQuestionsSvc>(provider).expect("provide");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tool = AskTool { ctx: ctx.clone() };
        let result = tool.execute(args, &tool_ctx()).await;
        let seen = asked.lock().expect("asked poisoned").clone();
        (result, seen)
    }

    #[tokio::test]
    async fn the_answer_is_the_option_the_person_picked() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        let _held = ctx
            .provide::<UserQuestionsSvc>(Arc::new(Picky(asked.clone())))
            .expect("provide");
        let tool = AskTool { ctx };
        let result = tool
            .execute(
                r#"{"question":"which one?","options":["the first","the second"]}"#,
                &tool_ctx(),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("the second"), "{}", result.content);
        let seen = asked.lock().expect("asked poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].prompt, "which one?");
        assert_eq!(seen[0].values(), vec!["the first", "the second"]);
        assert!(seen[0].about.is_none(), "a question is not an approval");
    }

    #[tokio::test]
    async fn no_answer_is_not_agreement_and_not_an_error() {
        let (result, _) = ask_with(
            Arc::new(Silent),
            r#"{"question":"which one?","options":["a","b"]}"#,
        )
        .await;
        assert!(!result.is_error, "nothing failed: {result:?}");
        assert!(
            result.content.contains("not agreement"),
            "{}",
            result.content
        );
        // The model must be left with a move. A refusal that ends the turn is
        // how "I asked and nobody answered" becomes "the agent stopped".
        assert!(result.content.contains("carry on"), "{}", result.content);
    }

    #[tokio::test]
    async fn a_question_with_nothing_to_choose_between_is_refused() {
        for args in [
            r#"{"question":"what should I do?","options":[]}"#,
            r#"{"question":"ok?","options":["yes"]}"#,
            r#"{"question":"pick","options":["a","b","c","d","e","f"]}"#,
        ] {
            let (result, seen) = ask_with(Arc::new(Silent), args).await;
            assert!(result.is_error, "{args} should be refused: {result:?}");
            assert!(seen.is_empty(), "and never reach the person");
        }
    }

    #[tokio::test]
    async fn with_no_front_end_at_all_it_says_so_rather_than_hanging() {
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let tool = AskTool { ctx: app.context() };
        let result = tool
            .execute(r#"{"question":"which?","options":["a","b"]}"#, &tool_ctx())
            .await;
        assert!(!result.is_error);
        assert!(
            result.content.contains("not agreement"),
            "{}",
            result.content
        );
    }
}
