use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::{
    AnyElement, App, AsyncApp, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window,
    WindowControlArea, div, prelude::FluentBuilder as _, px,
};
use keiki_api::{AuthorizationFlow, Client, OAUTH_REDIRECT_URI, StoredCredentials, TokenSet};
use keiki_model::{AgentStatus, AgentSummary, sort_agents};

use crate::app_menus;
use crate::icons::{self, icon};
use crate::theme::Theme;

const CREDENTIALS_KEY: &str = "keiki://oauth";

pub struct Shell {
    client: Client,
    auth: AuthState,
    stored_credentials: Option<StoredCredentials>,
    agents: Vec<AgentSummary>,
    selected_agent_id: Option<String>,
    focus_handle: FocusHandle,
    _open_url_task: Task<()>,
}

#[derive(Debug, Clone)]
enum AuthState {
    Restoring,
    SignedOut,
    Starting,
    Authorizing(AuthorizationFlow),
    SignedIn(TokenSet),
    Error(String),
}

impl Shell {
    pub fn new(
        api_base_url: String,
        open_urls: Arc<Mutex<Vec<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let open_url_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let urls = open_urls
                    .lock()
                    .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
                    .unwrap_or_default();
                for url in urls {
                    let _ = this.update(cx, |this, cx| this.handle_callback(url, cx));
                }
            }
        });
        let shell = Self {
            client: Client::new(api_base_url),
            auth: AuthState::Restoring,
            stored_credentials: None,
            agents: Vec::new(),
            selected_agent_id: None,
            focus_handle,
            _open_url_task: open_url_task,
        };
        shell.restore_session(cx);
        shell
    }

    fn restore_session(&self, cx: &mut Context<Self>) {
        let read_credentials = cx.read_credentials(CREDENTIALS_KEY);
        cx.spawn(async move |this, cx| {
            let stored = match read_credentials.await {
                Ok(Some((_, bytes))) => serde_json::from_slice::<StoredCredentials>(&bytes).ok(),
                Ok(None) => None,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.auth = AuthState::Error(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(stored) = stored else {
                let _ = this.update(cx, |this, cx| {
                    this.auth = AuthState::SignedOut;
                    cx.notify();
                });
                return;
            };
            let client = match this.read_with(&*cx, |this, _| this.client.clone()) {
                Ok(client) => client,
                Err(_) => return,
            };
            match client.refresh_token(&stored).await {
                Ok(tokens) => {
                    let rotated = tokens.stored_credentials(stored.client_id);
                    if let Err(error) = write_credentials(&rotated, cx).await {
                        let _ = this.update(cx, |this, cx| {
                            this.auth = AuthState::Error(error.to_string());
                            cx.notify();
                        });
                        return;
                    }
                    let _ = this.update(cx, |this, cx| {
                        this.stored_credentials = Some(rotated);
                        this.auth = AuthState::SignedIn(tokens);
                        cx.notify();
                    });
                }
                Err(error) if error.is_invalid_refresh_token() => {
                    let _ = cx.update(|cx| cx.delete_credentials(CREDENTIALS_KEY)).await;
                    let _ = this.update(cx, |this, cx| {
                        this.auth = AuthState::SignedOut;
                        this.stored_credentials = None;
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.auth = AuthState::Error(error.to_string());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        if matches!(self.auth, AuthState::Starting | AuthState::Authorizing(_)) {
            return;
        }
        self.auth = AuthState::Starting;
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                client.discover_oauth().await?;
                let client_id = client.register_client(OAUTH_REDIRECT_URI).await?;
                let flow = AuthorizationFlow::new(client_id, OAUTH_REDIRECT_URI.into());
                let url = client.authorization_url(&flow)?;
                Ok::<_, keiki_api::Error>((flow, url))
            }
            .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((flow, url)) => {
                    this.auth = AuthState::Authorizing(flow);
                    cx.open_url(url.as_str());
                    cx.notify();
                }
                Err(error) => {
                    this.auth = AuthState::Error(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn handle_callback(&mut self, callback: String, cx: &mut Context<Self>) {
        let AuthState::Authorizing(flow) = &self.auth else {
            return;
        };
        let flow = flow.clone();
        let code = match flow.authorization_code(&callback) {
            Ok(code) => code,
            Err(error) => {
                self.auth = AuthState::Error(error.to_string());
                cx.notify();
                return;
            }
        };
        self.auth = AuthState::Starting;
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let tokens = client.exchange_code(&flow, &code).await?;
                let stored = flow.stored_credentials(&tokens);
                write_credentials(&stored, cx).await?;
                Ok::<_, anyhow::Error>((tokens, stored))
            }
            .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((tokens, stored)) => {
                    this.stored_credentials = Some(stored);
                    this.auth = AuthState::SignedIn(tokens);
                    cx.notify();
                }
                Err(error) => {
                    this.auth = AuthState::Error(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let credentials = self.stored_credentials.take();
        self.auth = AuthState::Starting;
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Some(credentials) = credentials {
                let _ = client.revoke_token(&credentials.refresh_token).await;
            }
            let deletion = cx.update(|cx| cx.delete_credentials(CREDENTIALS_KEY)).await;
            let _ = this.update(cx, |this, cx| match deletion {
                Ok(()) => {
                    this.auth = AuthState::SignedOut;
                    this.agents.clear();
                    this.selected_agent_id = None;
                    cx.notify();
                }
                Err(error) => {
                    this.auth = AuthState::Error(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn select_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        self.selected_agent_id = Some(agent_id);
        cx.notify();
    }

    fn render_sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut agents = self.agents.clone();
        sort_agents(&mut agents);
        let agent_rows = agents.into_iter().map(|agent| {
            let id = agent.id.clone();
            let selected = self.selected_agent_id.as_deref() == Some(agent.id.as_str());
            div()
                .id(SharedString::from(format!("agent-{}", agent.id)))
                .h(px(44.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .when(selected, |row| row.bg(theme.glass_hover()))
                .hover(|row| row.bg(theme.glass_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_agent(id.clone(), cx);
                }))
                .child(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(status_color(agent.status, theme)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from(agent.name)),
                )
        });

        div()
            .w(px(256.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.glass())
            .child(
                div()
                    .h(px(44.0))
                    .px(px(16.0))
                    .when(cfg!(target_os = "macos"), |header| header.pl(px(80.0)))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .window_control_area(WindowControlArea::Drag)
                    .child(icon(icons::BOT).size(px(17.0)).text_color(theme.text))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Keiki"),
                    ),
            )
            .child(
                div()
                    .h(px(40.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child("AGENTS"),
                    )
                    .child(
                        div()
                            .size(px(26.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_color(theme.text_muted)
                            .child(
                                icon(icons::PLUS)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(8.0))
                    .children(agent_rows)
                    .when(self.agents.is_empty(), |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.text_muted)
                                .child("Agent sync is not connected yet."),
                        )
                    }),
            )
            .when_some(
                match &self.auth {
                    AuthState::SignedIn(tokens) => Some(tokens.scope()),
                    _ => None,
                },
                |sidebar, scope| {
                    sidebar.child(
                        div()
                            .mx(px(8.0))
                            .mb(px(8.0))
                            .px(px(10.0))
                            .h(px(38.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .child(format!("Connected · {scope}")),
                            )
                            .child(
                                div()
                                    .id("sign-out")
                                    .cursor_pointer()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text)
                                    .hover(|button| button.text_color(theme.accent))
                                    .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                                    .child("Sign out"),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    fn render_content(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if !matches!(self.auth, AuthState::SignedIn(_)) {
            return self.render_auth(theme, cx);
        }
        let title = self
            .selected_agent_id
            .as_ref()
            .and_then(|selected| self.agents.iter().find(|agent| &agent.id == selected))
            .map(|agent| agent.name.as_str())
            .unwrap_or("Conversations");
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(
                div()
                    .h(px(44.0))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .window_control_area(WindowControlArea::Drag)
                    .text_size(crate::typography::ui_rems(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(title.to_owned())),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(360.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .text_center()
                            .child(
                                div()
                                    .size(px(48.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(14.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.surface_card)
                                    .child(
                                        icon(icons::CHAT_ROUND_LINE)
                                            .size(px(22.0))
                                            .text_color(theme.text_muted),
                                    ),
                            )
                            .child(
                                div()
                                    .mt(px(16.0))
                                    .text_size(crate::typography::ui_rems(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("No conversation selected"),
                            )
                            .child(
                                div()
                                    .mt(px(6.0))
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .line_height(px(19.0))
                                    .text_color(theme.text_muted)
                                    .child(
                                        "Connect your Keiki account to load agents and start live conversations.",
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_auth(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (title, message, action) = match &self.auth {
            AuthState::Restoring => (
                "Connecting to Keiki",
                "Restoring your secure desktop session.",
                None,
            ),
            AuthState::SignedOut => (
                "Connect your Keiki account",
                "Sign in through your browser to manage agents and conversations.",
                Some("Sign in"),
            ),
            AuthState::Starting => (
                "Connecting to Keiki",
                "Preparing a secure browser authorization request.",
                None,
            ),
            AuthState::Authorizing(_) => (
                "Finish signing in",
                "Approve the desktop connection in your browser, then return to Keiki.",
                None,
            ),
            AuthState::SignedIn(_) => unreachable!(),
            AuthState::Error(error) => ("Could not connect", error.as_str(), Some("Try again")),
        };
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.bg)
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_center()
                    .child(
                        div()
                            .size(px(52.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(15.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_card)
                            .child(icon(icons::BOT).size(px(24.0)).text_color(theme.accent)),
                    )
                    .child(
                        div()
                            .mt(px(18.0))
                            .text_size(crate::typography::ui_rems(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .mt(px(8.0))
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(20.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(message.to_owned())),
                    )
                    .when_some(action, |content, label| {
                        content.child(
                            div()
                                .id("sign-in")
                                .mt(px(20.0))
                                .px(px(16.0))
                                .h(px(36.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(8.0))
                                .cursor_pointer()
                                .bg(theme.accent_strong)
                                .text_color(theme.on_accent)
                                .text_size(crate::typography::ui_rems(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .hover(|button| button.opacity(0.9))
                                .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                                .child(label),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        div()
            .size_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.font_sans.clone())
            .track_focus(&self.focus_handle)
            .on_action(|_: &app_menus::Minimize, window, _| {
                window.minimize_window();
            })
            .on_action(|_: &app_menus::Zoom, window, _| {
                window.zoom_window();
            })
            .on_action(|_: &app_menus::CloseWindow, window, _| {
                window.remove_window();
            })
            .child(self.render_sidebar(&theme, cx))
            .child(self.render_content(&theme, cx))
    }
}

async fn write_credentials(
    credentials: &StoredCredentials,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(credentials)?;
    cx.update(|cx: &mut App| cx.write_credentials(CREDENTIALS_KEY, "OAuth", &bytes))
        .await?;
    Ok(())
}

fn status_color(status: AgentStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        AgentStatus::NeedsAttention => theme.danger,
        AgentStatus::Running => theme.accent,
        AgentStatus::Idle => theme.text_muted,
        AgentStatus::Offline => theme.border,
    }
}
