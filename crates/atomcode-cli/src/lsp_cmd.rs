//! One-shot diagnostics using the exact registered model tool, never a second LSP client.
use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::json;

#[derive(Subcommand)]
pub enum LspCli {
    /// Initialize a configured local server and optionally run one read-only query.
    Probe {
        #[arg(long)]
        file: std::path::PathBuf,
        /// One-based line. Required with --method.
        #[arg(long, requires = "method", requires = "col", value_parser = clap::value_parser!(u32).range(1..))]
        line: Option<u32>,
        /// One-based Unicode character column (not byte or UTF-16 offset).
        #[arg(long, requires = "line", value_parser = clap::value_parser!(u32).range(1..))]
        col: Option<u32>,
        #[arg(long, requires = "line")]
        method: Option<ProbeMethod>,
        /// Ignore initialization options for this probe only.
        #[arg(long)]
        no_lsp_init_options: bool,
        /// Protocol trace detail for this probe (may contain source text in verbose mode).
        #[arg(long)]
        trace: Option<ProbeTrace>,
    },
}
#[derive(Clone, Copy, ValueEnum)]
pub enum ProbeTrace {
    Messages,
    Verbose,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum ProbeMethod {
    Definition,
    References,
    Hover,
}

pub async fn run(
    command: LspCli,
    config_path: &std::path::Path,
    working_dir: std::path::PathBuf,
) -> Result<()> {
    use atomcode_kernel::tool::{ToolContext, ToolRegistry};
    let LspCli::Probe {
        file,
        line,
        col,
        method,
        no_lsp_init_options,
        trace,
    } = command;
    let config =
        atomcode_config::Config::load_strict(config_path).context("load LSP configuration")?;
    if !config.lsp.enabled {
        bail!("Local LSP is disabled. Set [lsp] enabled = true before probing.");
    }
    let mut policy = atomcode_coding::config::lsp_settings_from_config(&config.lsp);
    let dirs = atomcode_coding::config::product_dirs_from_env();
    if let Some(trace) = trace {
        policy.trace = match trace {
            ProbeTrace::Messages => {
                atomcode_capabilities::codeintel::lsp_policy::LspTrace::Messages
            }
            ProbeTrace::Verbose => atomcode_capabilities::codeintel::lsp_policy::LspTrace::Verbose,
        };
    }
    policy.config_dir = Some(dirs.user().to_path_buf());
    policy.trace_file = Some(dirs.user().join("logs/lsp.log"));
    policy.init_options_off = no_lsp_init_options
        || std::env::var("ATOMCODE_LSP_INIT_OPTIONS")
            .is_ok_and(|value| value.eq_ignore_ascii_case("off"));
    let mut registry = ToolRegistry::new();
    if !atomcode_capabilities::codeintel::register_lsp_tool(&mut registry, &policy) {
        bail!("Local LSP is unavailable in this build");
    }
    let tool = registry
        .mount(&["lsp"])
        .get("lsp")
        .context("LSP tool registration failed")?;
    let ctx = ToolContext {
        working_dir,
        cancel: Default::default(),
        progress: Default::default(),
        requester: None,
    };
    let cancel = ctx.cancel.clone();
    let work = async {
        let status = tool
            .execute(
                &json!({"operation":"probe","backend":"local","file_path":file}).to_string(),
                &ctx,
            )
            .await;
        if status.is_error
            || serde_json::from_str::<serde_json::Value>(&status.content)
                .ok()
                .is_some_and(|value| value["status"] == "configured_disabled")
        {
            println!("{}", status.content);
            bail!("LSP initialization probe failed");
        }
        let mut output = json!({"connection": serde_json::from_str::<serde_json::Value>(&status.content).unwrap_or(json!(status.content))});
        let mut failed = false;
        if let Some(method) = method {
            let operation = match method {
                ProbeMethod::Definition => "definition",
                ProbeMethod::References => "references",
                ProbeMethod::Hover => "hover",
            };
            let result = tool.execute(&json!({"operation":operation,"backend":"local","file_path":file,"line":line,"character":col}).to_string(), &ctx).await;
            failed = result.is_error
                || result.content.starts_with("LSP unavailable:")
                || serde_json::from_str::<serde_json::Value>(&result.content)
                    .ok()
                    .is_some_and(|value| {
                        matches!(
                            value["status"].as_str(),
                            Some("configured_disabled" | "unsupported")
                        )
                    });
            output["query"] = json!({"method":operation,"is_error":failed,"result":result.content});
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
        if failed {
            bail!("LSP query failed; see probe output");
        }
        Ok(())
    };
    tokio::select! {
        result = work => result,
        _ = tokio::signal::ctrl_c() => { cancel.cancel(); bail!("LSP probe cancelled"); }
    }
}
