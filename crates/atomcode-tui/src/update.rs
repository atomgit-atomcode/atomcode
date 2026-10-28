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
