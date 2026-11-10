//! Shared human authorization flow for CLI and the launcher's TUI row.
use anyhow::{bail, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub fn online() -> Result<()> {
    if atomcode_config::config::offline::is_offline_active() {
        bail!("Offline mode is active; Grape authorization requires a connection");
    }
    Ok(())
}

pub fn status(user_dir: &Path) -> Result<String> {
    Ok(match atomcode_auth::grape::load(user_dir)? {
        Some(c) => format!("Grape 已连接：{}\n服务：{}\n此为本地连接状态，未检查远端健康。\natomcode grape disconnect 断开连接", c.username, c.server),
        None => format!("Grape 尚未连接。使用 atomcode grape connect 连接支持授权的服务。\n内测阶段：请先在 {} 使用 GitCode 登录并申请内测资格。", atomcode_auth::grape::CLOUD_APP),
    })
}

pub fn connect(
    user_dir: &Path,
    server: &str,
    no_browser: bool,
    say: &dyn Fn(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    online()?;
    let _lease = atomcode_auth::grape::lease(user_dir)?;
    let previous = atomcode_auth::grape::load(user_dir)?;
    if let Some(previous) = &previous {
        if previous.server != atomcode_auth::grape::server_url(server)?.as_str() {
            bail!("A different Grape service is connected. Disconnect it before connecting another service.");
        }
    }
    if cancelled() {
        bail!("Grape authorization cancelled; previous connection unchanged");
    }
    let pending = atomcode_auth::grape::start(server)?;
    say(&format!("Grape 授权码：{}\n{}\n请使用 GitCode 登录并核对授权码。连接不会自动派发任务。\n云端处于内测阶段；若需申请资格，请先取消此次等待，获批后重新连接。在终端按 Ctrl+C 取消等待。", pending.user_code, pending.verification_url));
    if cancelled() {
        bail!("Grape authorization cancelled; previous connection unchanged");
    }
    if !no_browser && atomcode_auth::oauth::open_browser(&pending.verification_url).is_err() {
        say("请在浏览器打开上方地址继续。");
    }
    let deadline = Instant::now() + Duration::from_secs(pending.expires_in);
    let mut interval = pending.interval;
    loop {
        let next = Instant::now() + Duration::from_secs(interval);
        if next >= deadline {
            bail!("Grape authorization expired; previous connection unchanged");
        }
        while Instant::now() < next {
            if cancelled() {
                bail!("Grape authorization cancelled; previous connection unchanged");
            }
            std::thread::sleep(
                Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())),
            );
        }
        match pending.poll()? {
            atomcode_auth::grape::Poll::Pending => {}
            atomcode_auth::grape::Poll::SlowDown => interval = interval.saturating_add(5),
            atomcode_auth::grape::Poll::Connected(c) => {
                // A reply arriving after cancellation must never replace the local connection.
                if cancelled() {
                    if atomcode_auth::grape::revoke(&c).is_err() {
                        bail!("Grape authorization cancelled; previous connection unchanged. Remote revocation failed: revoke the new AtomCode key in Grape.");
                    }
                    bail!("Grape authorization cancelled; previous connection unchanged");
                }
                if let Err(error) = c.save(user_dir) {
                    if atomcode_auth::grape::revoke(&c).is_err() {
                        say("新凭据保存失败，远端撤销也未确认；请在 Grape 中撤销新建的 AtomCode 凭据。");
                    }
                    return Err(error);
                }
                if let Some(old) = &previous {
                    if atomcode_auth::grape::revoke(old).is_err() {
                        say("新连接已保存；旧凭据撤销未确认，请在 Grape 的 API Key 管理中撤销旧的 AtomCode 凭据。");
                    }
                }
                say(&format!(
                    "Grape 已连接：{}\n服务：{}\n现在可以查询项目、检索知识和交办已有 Issue。",
                    c.username, c.server
                ));
                return Ok(());
            }
        }
    }
}
