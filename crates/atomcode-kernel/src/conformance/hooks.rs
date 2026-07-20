//! Conformance harness for the [`LifecycleHooks`] seam.
//!
//! The kernel calls EVERY lifecycle method during a run, with the host process's full
//! ambient authority and NO panic isolation (see [`crate::hook`]). The seam's contract
//! for an individual hook is therefore: every method must COMPLETE, BOUNDED, WITHOUT
//! PANICKING on representative inputs — the must-not-panic posture made executable. The
//! return-shape obligations (`user_prompt_submit` → `Result`, `offer_continuation` → `Option`) are
//! compile-enforced; this harness covers the runtime ones. It exercises all twelve
//! methods once each with representative inputs (incl. `turn_complete`, the per-turn
//! terminal twin of `session_end`), and additionally checks the one universal STATE
//! obligation an individual hook can violate on its own: `on_model_response_preserves_meta`
//! (the assistant `meta` is kernel-owned — a hook may transform text / tool_calls but
//! must not fabricate or overwrite `meta`).

use super::{run_void, ConformanceReport};
use crate::event::StopReason;
use crate::hook::{LifecycleHooks, TurnCtx};
use crate::message::{Conversation, Message, MessageMeta};
use crate::provider::ChatOptions;
use crate::tool::ToolDef;
use std::sync::Arc;

fn sample_convo() -> Conversation {
    let mut c = Conversation::new();
    c.push(Message::system("you are a helpful agent"));
    c.push(Message::user("do the thing"));
    c.push(Message::assistant("on it", vec![]));
    c
}

fn sample_ctx() -> TurnCtx {
    TurnCtx {
        turn_id: 1,
        request_id: 1,
        round: 1,
        max_rounds: Some(3),
        ..Default::default()
    }
}

/// Run the lifecycle-hooks conformance suite — all twelve methods, bounded + panic-caught.
pub async fn check(hooks: Arc<dyn LifecycleHooks>) -> ConformanceReport {
    let mut r = ConformanceReport::new("LifecycleHooks", "<hooks>");
    let ctx = sample_ctx();

    // session_start — fresh + resumed flag both exercised.
    {
        let mut convo = sample_convo();
        run_void(&mut r, "session_start_fresh", async {
            hooks.session_start(&mut convo, false).await
        })
        .await;
    }
    {
        let mut convo = sample_convo();
        run_void(&mut r, "session_start_resumed", async {
            hooks.session_start(&mut convo, true).await
        })
        .await;
    }

    // user_prompt_submit — Ok|Err both valid; just must complete without panic.
    {
        let mut text = "a user prompt".to_string();
        run_void(&mut r, "user_prompt_submit", async {
            let _ = hooks.user_prompt_submit(&mut text).await;
        })
        .await;
    }

    // turn_start.
    {
        let mut convo = sample_convo();
        run_void(&mut r, "turn_start", async {
            hooks.turn_start(&mut convo).await
        })
        .await;
    }

    // pre_request — mutates the ephemeral outgoing messages.
    {
        let mut messages = sample_convo().messages;
        run_void(&mut r, "pre_request", async {
            hooks.pre_request(&mut messages, &ctx).await
        })
        .await;
    }

    // on_request — read-only observation of the final wire.
    {
        let messages = sample_convo().messages;
        let tools: Vec<ToolDef> = vec![ToolDef {
            name: "echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let options = ChatOptions::default();
        run_void(&mut r, "on_request", async {
            hooks.on_request(&messages, &tools, &options, &ctx).await
        })
        .await;
    }

    // on_text_delta — transform a streamed chunk in place.
    {
        let mut delta = "a streamed chunk".to_string();
        run_void(&mut r, "on_text_delta", async {
            hooks.on_text_delta(&mut delta).await
        })
        .await;
    }

    // on_reasoning_delta — symmetric twin on the reasoning channel.
    {
        let mut delta = "a reasoning chunk".to_string();
        run_void(&mut r, "on_reasoning_delta", async {
            hooks.on_reasoning_delta(&mut delta).await
        })
        .await;
    }

    // on_model_response — observe / transform the assembled assistant message. The
    // kernel-owned `meta` MUST survive untouched ("meta is kernel-owned — don't
    // fabricate it"): seed a sentinel, run the hook, assert it is preserved.
    {
        let sentinel = MessageMeta {
            round: 7,
            request_id: 42,
            ..Default::default()
        };
        let mut response = Message::assistant("the model reply", vec![]);
        response.meta = Some(sentinel.clone());
        run_void(&mut r, "on_model_response", async {
            hooks.on_model_response(&mut response).await
        })
        .await;
        r.record(
            "on_model_response_preserves_meta",
            response.meta.as_ref() == Some(&sentinel),
            "on_model_response must not fabricate or overwrite the kernel-owned `meta` field — it may transform text / tool_calls only",
        );
    }

    // offer_continuation — Some(continue) | None(stop); must complete.
    {
        let convo = sample_convo();
        run_void(&mut r, "offer_continuation", async {
            let _ = hooks.offer_continuation(&convo).await;
        })
        .await;
    }

    // turn_complete — pure observation of EVERY turn terminal; must complete.
    {
        let convo = sample_convo();
        run_void(&mut r, "turn_complete", async {
            hooks
                .turn_complete(&convo, &StopReason::Stopped, &ctx)
                .await
        })
        .await;
    }

    // on_error — pure observation.
    run_void(&mut r, "on_error", async {
        hooks.on_error("a tool returned is_error").await
    })
    .await;

    // session_end — pure observation on any exit path.
    {
        let convo = sample_convo();
        run_void(&mut r, "session_end", async {
            hooks.session_end(&convo).await
        })
        .await;
    }

    r
}
