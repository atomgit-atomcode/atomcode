//! Whether a newer build is out, for the status row.
//!
//! Finding out is a network question and the answer's wording depends on how
//! this binary was installed (a package manager upgrades it, not `/upgrade`),
//! and neither is this screen's business: it cannot reach the network or the
//! filesystem (`gates/tui-layers.sh`). So the launcher answers, in the words the
//! row should show, and the screen only draws them. No row, no hint.

/// A persistent footer hint plus the full one-time explanation.
pub struct InstalledChange {
    pub hint: String,
    pub message: String,
}

/// The launcher's answer to "is there a newer build".
#[async_trait::async_trait]
pub trait UpdateCheck: Send + Sync + 'static {
    /// What the status row should say about a newer build, or `None` when
    /// there is none (or it could not be found out — this is a nudge, not a
    /// report, and silence is the right failure).
    async fn available(&self) -> Option<String>;
    /// Local installation check only; called periodically without network access.
    async fn local_change(&self) -> Option<InstalledChange> {
        None
    }
}

pub(crate) enum UpdateNotice {
    Available(String),
    Installed(InstalledChange),
}

/// Local replacement must not wait for the manifest, nor be overwritten by it.
pub(crate) async fn watch(
    check: std::sync::Arc<dyn UpdateCheck>,
    mut notify: impl FnMut(UpdateNotice),
    closed: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(closed);
    let remote = check.available();
    tokio::pin!(remote);
    let mut remote_done = false;
    let mut installed = None;
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(60));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = &mut closed => break,
            _ = ticks.tick() => {
                let change = tokio::select! {
                    biased;
                    _ = &mut closed => break,
                    change = check.local_change() => change,
                };
                if let Some(change) = change {
                    if installed.as_ref() != Some(&change.message) {
                        installed = Some(change.message.clone());
                        notify(UpdateNotice::Installed(change));
                    }
                }
            }
            hint = &mut remote, if !remote_done => {
                remote_done = true;
                if installed.is_none() {
                    if let Some(hint) = hint { notify(UpdateNotice::Available(hint)); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct Check {
        installed: AtomicBool,
    }
    #[async_trait::async_trait]
    impl UpdateCheck for Check {
        async fn available(&self) -> Option<String> {
            tokio::time::sleep(std::time::Duration::from_secs(180)).await;
            Some("upgrade".into())
        }
        async fn local_change(&self) -> Option<InstalledChange> {
            self.installed
                .load(Ordering::SeqCst)
                .then(|| InstalledChange {
                    hint: "restart".into(),
                    message: "restart once".into(),
                })
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn local_change_precedes_slow_network_and_is_not_overwritten_or_repeated() {
        let check = Arc::new(Check {
            installed: AtomicBool::new(false),
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = seen.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(watch(
            check.clone(),
            move |notice| {
                output.lock().unwrap().push(match notice {
                    UpdateNotice::Available(s) => s,
                    UpdateNotice::Installed(c) => c.hint,
                });
            },
            async move {
                let _ = stopped.await;
            },
        ));
        tokio::task::yield_now().await;
        check.installed.store(true, Ordering::SeqCst);
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock().unwrap(), ["restart"]);
        tokio::time::advance(std::time::Duration::from_secs(180)).await;
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock().unwrap(), ["restart"]);
        stop.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn ordinary_update_hint_survives_and_closing_cancels_a_pending_local_probe() {
        struct Pending;
        #[async_trait::async_trait]
        impl UpdateCheck for Pending {
            async fn available(&self) -> Option<String> {
                Some("upgrade".into())
            }
            async fn local_change(&self) -> Option<InstalledChange> {
                std::future::pending().await
            }
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(watch(Arc::new(Pending), |_| {}, async {
            let _ = stopped.await;
        }));
        tokio::task::yield_now().await;
        stop.send(()).unwrap();
        task.await.unwrap();

        let check = Arc::new(Check {
            installed: AtomicBool::new(false),
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = seen.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(watch(
            check,
            move |notice| {
                if let UpdateNotice::Available(s) = notice {
                    output.lock().unwrap().push(s);
                }
            },
            async move {
                let _ = stopped.await;
            },
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(180)).await;
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock().unwrap(), ["upgrade"]);
        stop.send(()).unwrap();
        task.await.unwrap();
    }
}

/// Presentation data for the launcher's update operation, unrelated to agent turns.
#[derive(Clone, Debug, Default)]
pub struct ProgressFrame {
    pub title: String,
    pub downloading: bool,
    pub pulse: u64,
    pub bytes: u64,
    pub total: u64,
    pub bytes_per_second: Option<f64>,
    pub remaining_seconds: Option<u64>,
}

impl ProgressFrame {
    /// ASCII is intentional: the same bar works in legacy terminals and SSH.
    pub fn download_line(&self, width: usize) -> String {
        use crate::i18n::{t, Msg};
        let pct = (self.total > 0)
            .then(|| ((u128::from(self.bytes) * 100 / u128::from(self.total)).min(100)) as usize);
        let count = if self.total == 0 {
            format!("{:.1} MiB", self.bytes as f64 / 1_048_576.0)
        } else {
            format!(
                "{:.1}/{:.1} MiB",
                self.bytes as f64 / 1_048_576.0,
                self.total as f64 / 1_048_576.0
            )
        };
        let speed = self
            .bytes_per_second
            .map(|n| format!("{:.1} MiB/s", n / 1_048_576.0))
            .unwrap_or_else(|| "— MiB/s".into());
        let eta = self
            .remaining_seconds
            .map(|seconds| t(Msg::UpgradeProgressEta { seconds }).into_owned())
            .unwrap_or_else(|| t(Msg::UpgradeProgressEtaUnknown).into_owned());
        let cells = width.saturating_sub(count.len() + 10).clamp(4, 24);
        let bar = match pct {
            Some(pct) => {
                let filled = cells * pct / 100;
                format!(
                    "[{}{}] {pct}%",
                    "#".repeat(filled),
                    "-".repeat(cells - filled)
                )
            }
            None => format!("[ ? {} ]", ['|', '/', '-', '\\'][self.pulse as usize % 4]),
        };
        let candidates = [
            format!("{bar} {count} · {speed} · {eta}"),
            format!("{bar} {count} · {speed}"),
            format!("{bar} {count}"),
            format!(
                "{} {count}",
                pct.map(|n| format!("{n}%")).unwrap_or_else(|| "?".into())
            ),
            pct.map(|n| format!("{n}%")).unwrap_or_else(|| "?".into()),
        ];
        candidates
            .into_iter()
            .find(|line| crate::width::str_width(line) <= width)
            .unwrap_or_else(|| if width > 0 { ".".into() } else { String::new() })
    }
}

/// A live card is amended in place and settled only on a terminal outcome.
pub trait ProgressDisplay: Send + Sync {
    fn draw(&self, frame: ProgressFrame, finished: bool);
    fn finish(&self);
}

atomcode_plexus::plexus_service!(ProgressDisplaySvc => dyn ProgressDisplay, "tui-upgrade-progress", Seam, "Live update download progress");

#[derive(Debug)]
pub(crate) struct ProgressBlock(pub ProgressFrame);

impl crate::block::Content for ProgressBlock {
    fn kind(&self) -> &'static str {
        "upgrade_progress"
    }
    fn content_hash(&self) -> crate::block::ContentHash {
        crate::block::hash_of(&[&format!("{:?}", self.0)])
    }
    fn lines(&self, ctx: &crate::block::RenderCtx) -> Vec<crate::frame::Line> {
        let text = if self.0.downloading {
            format!(
                "{}\n{}",
                self.0.title,
                self.0
                    .download_line(usize::from(ctx.width).saturating_sub(2))
            )
        } else {
            self.0.title.clone()
        };
        crate::content::CommandSaid {
            text,
            refused: false,
        }
        .lines(ctx)
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use crate::block::{Content, RenderCtx};
    #[test]
    fn download_layout_fits_every_terminal_width() {
        let frame = ProgressFrame {
            downloading: true,
            bytes: 28 * 1_048_576,
            total: 50 * 1_048_576,
            bytes_per_second: Some(3.2 * 1_048_576.0),
            remaining_seconds: Some(7),
            ..Default::default()
        };
        assert!(frame.download_line(120).contains("56%"));
        assert!(frame.download_line(120).contains("MiB/s"));
        for width in 0..120 {
            assert!(crate::width::str_width(&frame.download_line(width)) <= width);
        }
    }
    #[test]
    fn completed_download_is_not_a_success_until_the_phase_changes() {
        let block = ProgressBlock(ProgressFrame {
            title: "Downloading".into(),
            downloading: true,
            bytes: 100,
            total: 100,
            ..Default::default()
        });
        let lines: Vec<_> = block
            .lines(&RenderCtx::bare(100))
            .iter()
            .map(|line| line.plain())
            .collect();
        assert!(lines.iter().any(|line| line.contains("100%")));
        assert!(lines.iter().any(|line| line.contains("Downloading")));
        let verify = ProgressBlock(ProgressFrame {
            title: "Verifying".into(),
            ..Default::default()
        });
        assert!(!verify
            .lines(&RenderCtx::bare(100))
            .iter()
            .any(|line| line.plain().contains("100%")));
    }
}
