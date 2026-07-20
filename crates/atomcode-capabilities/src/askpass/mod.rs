pub mod cache;
pub mod protocol;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod wrapper;

#[cfg(unix)]
use std::sync::OnceLock;
#[cfg(unix)]
static ASKPASS_ENV: OnceLock<server::AskpassEnv> = OnceLock::new();
#[cfg(unix)]
pub fn set_env(env: server::AskpassEnv) {
    let _ = ASKPASS_ENV.set(env);
}
#[cfg(unix)]
pub fn current_env() -> Option<&'static server::AskpassEnv> {
    ASKPASS_ENV.get()
}

#[cfg(all(test, unix))]
mod global_tests {
    use super::*;
    #[test]
    fn set_then_current_returns_env() {
        assert!(current_env().is_none());
        set_env(server::AskpassEnv {
            sock_path: "/tmp/x.sock".into(),
            token: "tok".into(),
            askpass_script: "/tmp/askpass.sh".into(),
        });
        let e = current_env().expect("set");
        assert_eq!(e.token, "tok");
    }
}
