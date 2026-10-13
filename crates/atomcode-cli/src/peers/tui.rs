use async_trait::async_trait;
use atomcode_i18n::screen::{t, Msg};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
#[cfg(unix)]
use atomcode_tui::plugin::AgentClientSvc;
use atomcode_tui::plugin::CommandsSvc;
use serde_json::Value;
use std::sync::Arc;

pub const ROW: &str = "tui-local-peers";
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
pub struct PeerRow;
#[async_trait]
impl Plugin for PeerRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands", "tui-agent-client"]
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        ctx.require::<CommandsSvc>()
            .map_err(|e| e.to_string())?
            .add(Arc::new(PeerCommands))?;
        Ok(())
    }
}
struct PeerCommands;
#[async_trait]
impl CommandSet for PeerCommands {
    fn id(&self) -> &'static str {
        ROW
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "peers",
            "[list|pending|approve <id>|deny <id>|mode accept|hold|refuse]".into(),
            t(Msg::PeersAbout),
        )]
    }
    async fn run(&self, _: &str, args: &str, ctx: &Context) -> Outcome {
        #[cfg(unix)]
        {
            match run(args, ctx).await {
                Ok(text) => Outcome::Said(text),
                Err(error) => Outcome::Refused(error),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (args, ctx);
            Outcome::Refused(t(Msg::PeersUnsupported).into_owned())
        }
    }
}
#[cfg(unix)]
async fn run(args: &str, ctx: &Context) -> Result<String, String> {
    use super::local::{for_session, Inbound};
    let client = ctx.require::<AgentClientSvc>().map_err(|e| e.to_string())?;
    let hub =
        for_session(&client.session()).ok_or_else(|| t(Msg::PeersUnavailable).into_owned())?;
    let mut words = args.split_whitespace();
    let action = words.next().unwrap_or("list");
    let argument = words.next();
    if words.next().is_some() {
        return Err(t(Msg::PeersUsage).into_owned());
    }
    match (action, argument) {
        ("list", None) => {
            let info = hub.info().await?;
            let peers = hub.list().await?;
            let mut rows = vec![format!(
                "{} {} · {}",
                t(Msg::PeersSelf),
                info.address,
                info.mode.as_str()
            )];
            if peers.is_empty() {
                rows.push(t(Msg::PeersEmpty).into_owned());
            }
            rows.extend(peers.into_iter().map(|peer| {
                format!(
                    "{} · {:?} · {} · {}\n  {}",
                    safe_text(&peer.address),
                    peer.status,
                    peer.mode.as_str(),
                    safe_text(&peer.cwd),
                    safe_text(peer.title.as_deref().unwrap_or(""))
                )
            }));
            rows.push(t(Msg::PeersUsage).into_owned());
            Ok(rows.join("\n"))
        }
        ("pending", None) => {
            let pending = hub.pending()?;
            if pending.is_empty() {
                return Ok(t(Msg::PeersNoPending).into_owned());
            }
            Ok(pending
                .into_iter()
                .map(|entry| {
                    format!(
                        "{} · {}\n{}\n",
                        entry.message.id,
                        entry.message.from,
                        safe_text(&entry.message.text)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ("approve", Some(id)) => {
            let receipt = hub.approve(id).await?;
            Ok(format!(
                "{} {}",
                t(Msg::PeersReceipt),
                serde_json::to_string(&receipt).map_err(|e| e.to_string())?
            ))
        }
        ("deny", Some(id)) => {
            hub.deny(id)?;
            Ok(t(Msg::PeersDenied).into_owned())
        }
        ("mode", Some(mode)) => {
            let mode = match mode {
                "accept" => Inbound::Accept,
                "hold" => Inbound::Hold,
                "refuse" => Inbound::Refuse,
                _ => return Err(t(Msg::PeersUsage).into_owned()),
            };
            hub.set_mode(mode);
            Ok(t(Msg::PeersModeSet).into_owned())
        }
        _ => Err(t(Msg::PeersUsage).into_owned()),
    }
}
#[cfg(unix)]
fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
