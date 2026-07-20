//! Conformance harnesses for the kernel's four EXTENSION SEAMS — the reusable
//! contract-checkers a third party runs against its OWN implementation of a seam.
//!
//! The kernel is an embeddable SDK whose behavior is extended by plugging code into
//! four seams: a [`LlmProvider`](crate::provider::LlmProvider), a
//! [`Tool`](crate::tool::Tool), a [`ToolMiddleware`](crate::middleware::ToolMiddleware),
//! and [`LifecycleHooks`](crate::hook::LifecycleHooks). Each seam carries a CONTRACT
//! (documented on its trait) that the kernel's turn loop RELIES ON — stable metadata
//! for prompt-cache safety, deterministic risk for approval caching, a stream that
//! terminates, a must-not-panic posture, a gating middleware that honors the request
//! timeout instead of parking forever. A third-party adapter / tool / middleware / hook
//! that violates one of these breaks the host in ways a normal unit test won't surface.
//!
//! Each `seam::check(impl) -> ConformanceReport` exercises the contract against an
//! arbitrary trait object and returns a structured pass/fail report. `assert_conformant`
//! turns a non-conformant report into a readable panic, so a downstream crate can gate
//! its own impls in one line:
//!
//! ```ignore
//! atomcode_kernel::conformance::tool::check(Arc::new(MyTool), &["{}"]).await.assert_conformant();
//! ```
//!
//! WHO RUNS THIS: L1 runs the provider harness against its real OpenAI-compat adapter;
//! an L2 specialization runs the tool/middleware/hook harnesses against its own
//! extensions; the kernel's own `tests/conformance.rs` runs all four against the
//! `testkit` doubles (a conforming double must pass; an adversarial double must FAIL a
//! specific named check — proving the harness has teeth).
//!
//! PANIC-CATCH NOTE: the "must-not-panic" contract is made EXECUTABLE by running each
//! injected call inside `catch_unwind`. This works under the default `cargo test`
//! (unwind) profile, where a panicking impl is caught and reported as a failed check.
//! Under the `panic = "abort"` release profile `catch_unwind` is a no-op and a panic
//! aborts the process — so run conformance under `cargo test`, not a release binary.

use std::future::Future;
use std::time::Duration;

pub mod hooks;
pub mod middleware;
pub mod provider;
pub mod tool;

/// Default per-call bound. A conforming injected call (tool execute, middleware
/// before/after, a hook method, draining a provider stream) must complete well
/// within this; exceeding it is itself a conformance failure (a park / infinite loop).
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// One named contract check and whether the subject honored it.
#[derive(Clone, Debug)]
pub struct CheckOutcome {
    /// Stable identifier of the contract clause (e.g. `"name_stable"`).
    pub name: &'static str,
    pub passed: bool,
    /// Human-readable explanation — the WHY of the contract on failure, or any
    /// observed value worth surfacing on success.
    pub detail: String,
}

/// The result of running one seam's harness against one implementation.
#[derive(Clone, Debug)]
pub struct ConformanceReport {
    /// The seam name (`"Tool"`, `"ToolMiddleware"`, `"LifecycleHooks"`, `"LlmProvider"`).
    pub seam: &'static str,
    /// A label for the subject under test (e.g. the tool's `name()`).
    pub subject: String,
    pub checks: Vec<CheckOutcome>,
}

impl ConformanceReport {
    pub fn new(seam: &'static str, subject: impl Into<String>) -> Self {
        Self {
            seam,
            subject: subject.into(),
            checks: Vec::new(),
        }
    }

    /// Append a check outcome.
    pub fn record(&mut self, name: &'static str, passed: bool, detail: impl Into<String>) {
        self.checks.push(CheckOutcome {
            name,
            passed,
            detail: detail.into(),
        });
    }

    /// True iff every recorded check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The failing checks (empty when conformant).
    pub fn failures(&self) -> Vec<&CheckOutcome> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// Panic with a readable, multi-line explanation if any check failed — the
    /// one-line gate a downstream crate uses to assert its impl is conformant.
    pub fn assert_conformant(&self) {
        if self.passed() {
            return;
        }
        let lines: Vec<String> = self
            .failures()
            .iter()
            .map(|c| format!("  ✗ {}: {}", c.name, c.detail))
            .collect();
        panic!(
            "{} conformance FAILED for `{}` ({} of {} checks failed):\n{}",
            self.seam,
            self.subject,
            self.failures().len(),
            self.checks.len(),
            lines.join("\n"),
        );
    }
}

/// Extract a readable message from a `catch_unwind` panic payload.
pub(crate) fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Run a future to completion, turning a panic into `Err(message)` instead of
/// unwinding through the harness (under the unwind test profile).
pub(crate) async fn catch_async<F: Future>(fut: F) -> Result<F::Output, String> {
    use futures::FutureExt;
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(v) => Ok(v),
        Err(e) => Err(panic_message(e)),
    }
}

/// Run a synchronous closure, turning a panic into `Err(message)`.
pub(crate) fn catch_sync<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Ok(v),
        Err(e) => Err(panic_message(e)),
    }
}

/// Await `fut` with a bound; `Err` on timeout (a park / runaway is a conformance fail).
pub(crate) async fn with_timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, String> {
    match tokio::time::timeout(d, fut).await {
        Ok(v) => Ok(v),
        Err(_) => Err(format!("did not complete within {d:?}")),
    }
}

/// Run a `()`-returning injected call under the default bound + panic-catch and
/// record one pass/fail check. The shared shape for the read-only / fire-and-forget
/// seam methods (most hook methods, middleware `after`) whose only contract is
/// "complete, bounded, without panicking".
pub(crate) async fn run_void<F: Future<Output = ()>>(
    report: &mut ConformanceReport,
    name: &'static str,
    fut: F,
) {
    match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(fut)).await {
        Ok(Ok(())) => report.record(name, true, ""),
        Ok(Err(p)) => report.record(
            name,
            false,
            format!("panicked: {p} — see the seam's must-not-panic PANIC CONTRACT"),
        ),
        Err(t) => report.record(name, false, t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_passed_and_failures() {
        let mut r = ConformanceReport::new("Tool", "echo");
        r.record("a", true, "");
        assert!(r.passed());
        assert!(r.failures().is_empty());
        r.record("b", false, "boom");
        assert!(!r.passed());
        assert_eq!(r.failures().len(), 1);
        assert_eq!(r.failures()[0].name, "b");
    }

    #[test]
    #[should_panic(expected = "Tool conformance FAILED for `echo`")]
    fn assert_conformant_panics_readably() {
        let mut r = ConformanceReport::new("Tool", "echo");
        r.record("name_stable", false, "changed across calls");
        r.assert_conformant();
    }

    #[tokio::test]
    async fn catch_async_turns_panic_into_err() {
        let caught = catch_async(async { panic!("nope") }).await;
        assert!(caught.is_err());
        assert!(caught.unwrap_err().contains("nope"));
    }

    #[test]
    fn catch_sync_turns_panic_into_err() {
        let caught = catch_sync(|| panic!("boom"));
        assert!(caught.is_err());
        assert!(caught.unwrap_err().contains("boom"));
    }

    #[tokio::test]
    async fn with_timeout_fails_on_park() {
        let r = with_timeout(Duration::from_millis(10), futures::future::pending::<()>()).await;
        assert!(r.is_err());
    }
}
