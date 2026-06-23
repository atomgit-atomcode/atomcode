//! `/undo`: pure conversation truncation to before the nth user prompt.
//!
//! Coding-level conversation logic, relocated out of the bridge so the interactive
//! orchestration consolidates in L2 (the bridge-elimination roadmap). The async side —
//! persist the truncated snapshot + respawn the engine from it — stays with whoever owns
//! the runtime (the bridge today, the TUI driver after); this is just the pure math.

use atomcode_kernel::message::{Message, Role};

/// Result of a successful `/undo` truncation.
pub struct UndoPlan {
    pub truncated: Vec<Message>,
    pub restored_prompt: String,
    pub target_n: usize,
    pub prompts_before: usize,
}

/// Cut the conversation to BEFORE the `nth` REAL (non-synthetic) user prompt
/// (None = the last one), returning the truncated history + that prompt's text.
/// `Err((requested, available))` when out of range — mirrors v1's
/// `Conversation::undo_to_prompt`.
pub fn compute_undo(messages: &[Message], nth: Option<usize>) -> Result<UndoPlan, (usize, usize)> {
    let prompt_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User && !m.synthetic)
        .map(|(i, _)| i)
        .collect();
    let available = prompt_indices.len();
    let target = nth.unwrap_or(available);
    match target.checked_sub(1).and_then(|i| prompt_indices.get(i)) {
        Some(&idx) => Ok(UndoPlan {
            truncated: messages[..idx].to_vec(),
            restored_prompt: messages[idx].text.clone(),
            target_n: target,
            prompts_before: available,
        }),
        None => Err((target, available)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo() -> Vec<Message> {
        // system, user1, asst1, user2, asst2 — two real prompts.
        vec![
            Message::system("persona"),
            Message::user("first question"),
            Message::assistant("first answer", vec![]),
            Message::user("second question"),
            Message::assistant("second answer", vec![]),
        ]
    }

    #[test]
    fn bare_undo_drops_the_last_turn() {
        let p = compute_undo(&convo(), None).unwrap();
        assert_eq!(p.target_n, 2);
        assert_eq!(p.prompts_before, 2);
        assert_eq!(p.restored_prompt, "second question");
        // truncated to before user2 → system, user1, asst1.
        assert_eq!(p.truncated.len(), 3);
        assert_eq!(p.truncated.last().unwrap().text, "first answer");
    }

    #[test]
    fn undo_to_first_prompt_keeps_only_the_system_head() {
        let p = compute_undo(&convo(), Some(1)).unwrap();
        assert_eq!(p.restored_prompt, "first question");
        assert_eq!(p.truncated.len(), 1);
        assert_eq!(p.truncated[0].role, Role::System);
    }

    #[test]
    fn out_of_range_and_zero_fail_with_counts() {
        assert_eq!(compute_undo(&convo(), Some(3)).err(), Some((3, 2)));
        assert_eq!(compute_undo(&convo(), Some(0)).err(), Some((0, 2)));
        assert_eq!(compute_undo(&[], None).err(), Some((0, 0)));
    }

    #[test]
    fn synthetic_user_messages_are_not_prompts() {
        let mut msgs = convo();
        let mut note = Message::user("[PLAN MODE ACTIVATED] ...");
        note.synthetic = true;
        msgs.insert(3, note); // a synthetic note between the two real prompts
        let p = compute_undo(&msgs, None).unwrap();
        // Still 2 real prompts; the synthetic note must not shift the count/target.
        assert_eq!(p.prompts_before, 2);
        assert_eq!(p.restored_prompt, "second question");
    }
}
