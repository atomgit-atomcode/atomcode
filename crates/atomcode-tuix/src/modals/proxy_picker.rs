use anyhow::Result;
use atomcode_core::proxy::{self, ProxyMode};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, save_and_reload, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct ProxyPicker {
    pub selected: usize,
}

impl ProxyPicker {
    pub fn open(config: &atomcode_core::config::Config) -> Self {
        let selected = match config.network.proxy.mode {
            ProxyMode::FollowSystem => 0,
            ProxyMode::DefaultProxy => 1,
            ProxyMode::NoProxy => 2,
        };
        Self { selected }
    }
}

impl Modal for ProxyPicker {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                if self.selected < 2 {
                    self.selected += 1;
                }
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                match self.selected {
                    0 => {
                        ctx.config.network.proxy.mode = ProxyMode::FollowSystem;
                    }
                    1 => {
                        let pinned = proxy::ProxyConfig::capture_from_env();
                        ctx.config.network.proxy.mode = ProxyMode::DefaultProxy;
                        ctx.config.network.proxy.http = pinned.http;
                        ctx.config.network.proxy.https = pinned.https;
                        ctx.config.network.proxy.all = pinned.all;
                        ctx.config.network.proxy.no_proxy = pinned.no_proxy;
                    }
                    _ => {
                        ctx.config.network.proxy.mode = ProxyMode::NoProxy;
                    }
                }
                proxy::apply_process_proxy_config(&ctx.config.network.proxy);
                save_and_reload(ctx, renderer);
                renderer.render(UiLine::CommandOutput(format!(
                    "  Proxy mode: {}\n",
                    ctx.config.network.proxy.summary()
                )));
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let items = vec![
            (
                "follow_system".to_string(),
                "Follow current launch environment / system proxy state".to_string(),
            ),
            (
                "default_proxy".to_string(),
                "Pin the current proxy env and reuse it on later launches".to_string(),
            ),
            (
                "no_proxy".to_string(),
                "Disable proxy resolution for outbound HTTP clients".to_string(),
            ),
        ];
        let payload = MenuPayload {
            items,
            selected: self.selected,
            kind: crate::render::MenuKind::SlashCommand,
        };
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}
