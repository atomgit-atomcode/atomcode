//! Human-controlled Grape authorization. No credentials are printed or exposed as tools.
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum GrapeCli {
    /// Open Grape consent in the browser, then use its native tools in any frontend.
    Connect {
        #[arg(long, default_value = atomcode_auth::grape::DEFAULT_SERVER)]
        server: String,
        /// Print the consent URL without opening a browser (SSH/headless hosts).
        #[arg(long)]
        no_browser: bool,
    },
    /// Show connection identity without printing credentials or contacting Grape.
    Status,
    /// Revoke this connection at Grape and remove its local credential.
    Disconnect,
}
pub fn handle(command: GrapeCli) -> Result<()> {
    let dirs = atomcode_coding::config::product_dirs_from_env();
    match command {
        GrapeCli::Status => {
            let state = atomcode_auth::grape::load(dirs.user())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&match state {
                    Some(c) =>
                        serde_json::json!({"connected":true,"server":c.server,"username":c.username}),
                    None => serde_json::json!({"connected":false}),
                })?
            );
        }
        GrapeCli::Disconnect => {
            atomcode::grape_connect::online()?;
            let _lease = atomcode_auth::grape::lease(dirs.user())?;
            atomcode_auth::grape::disconnect(dirs.user())?;
            println!("Grape disconnected.");
        }
        GrapeCli::Connect { server, no_browser } => {
            atomcode::grape_connect::connect(
                dirs.user(),
                &server,
                no_browser,
                &|line| eprintln!("{line}"),
                &|| false,
            )?;
        }
    }
    Ok(())
}
