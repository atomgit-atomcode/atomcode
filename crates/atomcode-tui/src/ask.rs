//! Asking the person, through the screen they are already looking at.
//!
//! The `user-questions` seam is answered here rather than by the transcript,
//! and that split is deliberate: a stream producer that handled keystrokes
//! would not be a fold any more. So the asker owns the question, the host owns
//! the block and the keyboard, and they meet over one small mailbox.
//!
//! Nothing here decides *policy*. Whether a call needs asking about is the
//! approval row's business; this only knows how to put a question on a screen
//! and wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::UserQuestions;
use tokio::sync::oneshot;

/// A question waiting for an answer.
pub struct Pending {
    pub id: u64,
    pub question: String,
    pub options: Vec<String>,
    reply: oneshot::Sender<Option<String>>,
}

impl Pending {
    /// Deliver the answer. `None` is a refusal — every caller must read it that
    /// way, never as consent.
    pub fn answer(self, choice: Option<String>) {
        let _ = self.reply.send(choice);
    }
    /// What a number key picks, 1-based as the screen shows it.
    pub fn nth(&self, n: usize) -> Option<String> {
        self.options.get(n.wrapping_sub(1)).cloned()
    }
    /// What a letter picks, when an option starts with it.
    pub fn by_prefix(&self, c: char) -> Option<String> {
        let c = c.to_ascii_lowercase();
        self.options
            .iter()
            .find(|o| o.to_lowercase().starts_with(c))
            .cloned()
    }
}

/// The mailbox between the asker and the loop.
#[derive(Default)]
pub struct Asks {
    queue: Mutex<Vec<Pending>>,
    next: AtomicU64,
    wake: Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
}

impl Asks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The loop registers here so a question can wake it even mid-turn.
    pub fn notify_on(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        *self.wake.lock().expect("asks poisoned") = Some(tx);
    }

    /// The one currently on screen, if any.
    pub fn peek(&self) -> Option<(u64, String, Vec<String>)> {
        self.queue
            .lock()
            .expect("asks poisoned")
            .first()
            .map(|p| (p.id, p.question.clone(), p.options.clone()))
    }

    pub fn is_waiting(&self) -> bool {
        !self.queue.lock().expect("asks poisoned").is_empty()
    }

    /// Take the front question so it can be answered.
    pub fn take(&self) -> Option<Pending> {
        let mut q = self.queue.lock().expect("asks poisoned");
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    }

    /// Refuse everything still waiting. For shutdown: a caller blocked on an
    /// answer that is never coming would hold the turn open forever.
    pub fn refuse_all(&self) {
        let waiting: Vec<Pending> = self
            .queue
            .lock()
            .expect("asks poisoned")
            .drain(..)
            .collect();
        for p in waiting {
            p.answer(None);
        }
    }

    fn push(&self, question: String, options: Vec<String>) -> oneshot::Receiver<Option<String>> {
        let (reply, rx) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.lock().expect("asks poisoned").push(Pending {
            id,
            question,
            options,
            reply,
        });
        if let Some(tx) = self.wake.lock().expect("asks poisoned").as_ref() {
            let _ = tx.send(());
        }
        rx
    }
}

/// Fills `user-questions` by putting the question on the screen.
pub struct ScreenQuestions {
    asks: Arc<Asks>,
}

impl ScreenQuestions {
    pub fn new(asks: Arc<Asks>) -> Self {
        Self { asks }
    }
}

#[async_trait]
impl UserQuestions for ScreenQuestions {
    fn describe(&self) -> String {
        "the person at the terminal".into()
    }

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let opts = if options.is_empty() {
            vec!["yes".to_string(), "no".to_string()]
        } else {
            options.to_vec()
        };
        let rx = self.asks.push(question.to_string(), opts);
        // No timeout here on purpose: the person is right there, and a question
        // that expired while they were reading it would deny a call they were
        // about to allow. Shutdown refuses everything instead.
        rx.await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_question_reaches_the_screen_and_the_answer_comes_back() {
        let asks = Asks::new();
        let q = ScreenQuestions::new(asks.clone());
        let asking = tokio::spawn(async move {
            q.ask("Allow `write_file`?", &["yes".into(), "no".into()])
                .await
        });
        // The loop would see it here.
        for _ in 0..100 {
            if asks.is_waiting() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let (_, question, options) = asks.peek().expect("a question is waiting");
        assert!(question.contains("write_file"));
        assert_eq!(options, vec!["yes", "no"]);
        asks.take().unwrap().answer(Some("yes".into()));
        assert_eq!(asking.await.unwrap().as_deref(), Some("yes"));
    }

    #[tokio::test]
    async fn no_answer_is_a_refusal_not_a_hang() {
        let asks = Asks::new();
        let q = ScreenQuestions::new(asks.clone());
        let asking = tokio::spawn(async move { q.ask("Allow?", &[]).await });
        for _ in 0..100 {
            if asks.is_waiting() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // Shutting down must release the caller, refusing.
        asks.refuse_all();
        assert_eq!(asking.await.unwrap(), None, "never consent by default");
    }

    #[test]
    fn keys_pick_by_number_or_by_first_letter() {
        let asks = Asks::new();
        // The receiver is dropped on purpose: this test only cares how the
        // pending question answers key presses, not who is waiting on it.
        drop(asks.push("q".into(), vec!["yes".into(), "no".into()]));
        let p = asks.take().unwrap();
        assert_eq!(p.nth(1).as_deref(), Some("yes"));
        assert_eq!(p.nth(2).as_deref(), Some("no"));
        assert_eq!(p.nth(3), None, "out of range picks nothing");
        assert_eq!(p.nth(0), None, "the screen is 1-based; so is this");
        assert_eq!(p.by_prefix('N').as_deref(), Some("no"));
        assert_eq!(p.by_prefix('z'), None);
    }

    #[tokio::test]
    async fn questions_queue_rather_than_overwrite_each_other() {
        let asks = Asks::new();
        let a = ScreenQuestions::new(asks.clone());
        let b = ScreenQuestions::new(asks.clone());
        let one = tokio::spawn(async move { a.ask("first", &["y".into()]).await });
        let two = tokio::spawn(async move { b.ask("second", &["y".into()]).await });
        for _ in 0..100 {
            if asks.queue.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(asks.queue.lock().unwrap().len(), 2, "both are waiting");
        // Answered in order, so a person never answers one prompt for another.
        while let Some(p) = asks.take() {
            p.answer(Some("y".into()));
        }
        assert_eq!(one.await.unwrap().as_deref(), Some("y"));
        assert_eq!(two.await.unwrap().as_deref(), Some("y"));
    }
}
