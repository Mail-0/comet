use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::{
    AnyElement, App, AsyncApp, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window,
    WindowControlArea, div, prelude::FluentBuilder as _, px,
};
use keiki_api::{
    AgentConfig, AuthorizationFlow, Client, CreateAgentFromTemplate, OAUTH_REDIRECT_URI,
    StoredCredentials, TokenSet,
};
use keiki_model::{AgentStatus, AgentSummary, AgentTemplateSummary, sort_agents};

use crate::app_menus;
use crate::icons::{self, icon};
use crate::theme::Theme;

const CREDENTIALS_KEY: &str = "keiki://oauth";

pub struct Shell {
    client: Client,
    auth: AuthState,
    stored_credentials: Option<StoredCredentials>,
    agents: Vec<AgentSummary>,
    agent_error: Option<String>,
    agents_loading: bool,
    templates: Vec<AgentTemplateSummary>,
    template_error: Option<String>,
    templates_loading: bool,
    creator_open: bool,
    creating_template_id: Option<String>,
    creation_notice: Option<String>,
    selected_agent_id: Option<String>,
    selected_agent_config: Option<AgentConfig>,
    agent_config_error: Option<String>,
    agent_config_loading: bool,
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
            agent_error: None,
            agents_loading: false,
            templates: Vec::new(),
            template_error: None,
            templates_loading: false,
            creator_open: false,
            creating_template_id: None,
            creation_notice: None,
            selected_agent_id: None,
            selected_agent_config: None,
            agent_config_error: None,
            agent_config_loading: false,
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
                        this.load_agents(cx);
                        cx.notify();
                    });
                }
                Err(error) if error.is_invalid_refresh_token() => {
                    let deletion = cx.update(|cx| cx.delete_credentials(CREDENTIALS_KEY)).await;
                    let _ = this.update(cx, |this, cx| match deletion {
                        Ok(()) => {
                            this.auth = AuthState::SignedOut;
                            this.stored_credentials = None;
                            cx.notify();
                        }
                        Err(error) => {
                            this.auth = AuthState::Error(error.to_string());
                            cx.notify();
                        }
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
                    this.load_agents(cx);
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
            let revocation_error = if let Some(credentials) = credentials {
                client
                    .revoke_token(&credentials.refresh_token)
                    .await
                    .err()
                    .map(|error| error.to_string())
            } else {
                None
            };
            let deletion = cx.update(|cx| cx.delete_credentials(CREDENTIALS_KEY)).await;
            let _ = this.update(cx, |this, cx| match deletion {
                Ok(()) => {
                    this.auth = revocation_error.map_or(AuthState::SignedOut, |error| {
                        AuthState::Error(format!(
                            "Signed out locally, but Keiki could not revoke the session: {error}"
                        ))
                    });
                    this.agents.clear();
                    this.agent_error = None;
                    this.templates.clear();
                    this.template_error = None;
                    this.creator_open = false;
                    this.creating_template_id = None;
                    this.creation_notice = None;
                    this.selected_agent_id = None;
                    this.selected_agent_config = None;
                    this.agent_config_error = None;
                    this.agent_config_loading = false;
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

    fn load_agents(&mut self, cx: &mut Context<Self>) {
        let AuthState::SignedIn(tokens) = &self.auth else {
            return;
        };
        let access_token = tokens.access_token().to_owned();
        let client = self.client.clone();
        self.agents_loading = true;
        self.agent_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.list_agents(&access_token).await;
            let _ = this.update(cx, |this, cx| {
                this.agents_loading = false;
                match result {
                    Ok(agents) => {
                        let selection_exists =
                            this.selected_agent_id.as_ref().is_some_and(|selected| {
                                agents.iter().any(|agent| &agent.id == selected)
                            });
                        if !selection_exists {
                            this.selected_agent_id = agents.first().map(|agent| agent.id.clone());
                        }
                        this.agents = agents;
                        if let Some(selected) = this.selected_agent_id.clone() {
                            this.load_agent_config(selected, cx);
                        } else {
                            this.selected_agent_config = None;
                            this.agent_config_error = None;
                            this.agent_config_loading = false;
                        }
                    }
                    Err(error) if error.is_authentication_failure() => {
                        this.forget_session(cx);
                    }
                    Err(error) => {
                        this.agent_error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn forget_session(&mut self, cx: &mut Context<Self>) {
        let deletion = cx.delete_credentials(CREDENTIALS_KEY);
        self.auth = AuthState::SignedOut;
        self.stored_credentials = None;
        self.agents.clear();
        self.templates.clear();
        self.selected_agent_id = None;
        self.selected_agent_config = None;
        self.agent_config_error = None;
        self.agent_config_loading = false;
        self.creator_open = false;
        cx.spawn(async move |this, cx| {
            if let Err(error) = deletion.await {
                let _ = this.update(cx, |this, cx| {
                    this.auth = AuthState::Error(error.to_string());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_agent_creator(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.auth, AuthState::SignedIn(_)) {
            return;
        }
        self.creator_open = true;
        self.creation_notice = None;
        if self.templates.is_empty() {
            self.load_agent_templates(cx);
        } else {
            cx.notify();
        }
    }

    fn close_agent_creator(&mut self, cx: &mut Context<Self>) {
        self.creator_open = false;
        self.template_error = None;
        cx.notify();
    }

    fn load_agent_templates(&mut self, cx: &mut Context<Self>) {
        let AuthState::SignedIn(tokens) = &self.auth else {
            return;
        };
        let access_token = tokens.access_token().to_owned();
        let client = self.client.clone();
        self.templates_loading = true;
        self.template_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.list_agent_templates(&access_token).await;
            let _ = this.update(cx, |this, cx| {
                this.templates_loading = false;
                match result {
                    Ok(templates) => this.templates = templates,
                    Err(error) if error.is_authentication_failure() => {
                        this.forget_session(cx);
                    }
                    Err(error) => this.template_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn create_agent_from_template(&mut self, template_id: String, cx: &mut Context<Self>) {
        let AuthState::SignedIn(tokens) = &self.auth else {
            return;
        };
        if self.creating_template_id.is_some() {
            return;
        }
        let access_token = tokens.access_token().to_owned();
        let client = self.client.clone();
        self.creating_template_id = Some(template_id.clone());
        self.template_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .create_agent_from_template(
                    &access_token,
                    &CreateAgentFromTemplate {
                        template: template_id,
                        name: None,
                        line_number: None,
                    },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.creating_template_id = None;
                match result {
                    Ok(created) => {
                        this.selected_agent_id = Some(created.id);
                        this.creator_open = false;
                        this.creation_notice = (!created.missing_secrets.is_empty()).then(|| {
                            format!(
                                "Setup needed: add {} in Keiki Settings → Secrets.",
                                created
                                    .missing_secrets
                                    .iter()
                                    .map(|secret| secret.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        });
                        this.load_agents(cx);
                    }
                    Err(error) if error.is_authentication_failure() => {
                        this.forget_session(cx);
                    }
                    Err(error) => this.template_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn select_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        self.selected_agent_id = Some(agent_id.clone());
        self.creation_notice = None;
        self.load_agent_config(agent_id, cx);
    }

    fn load_agent_config(&mut self, agent_id: String, cx: &mut Context<Self>) {
        let AuthState::SignedIn(tokens) = &self.auth else {
            return;
        };
        let access_token = tokens.access_token().to_owned();
        let client = self.client.clone();
        self.selected_agent_config = None;
        self.agent_config_error = None;
        self.agent_config_loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.agent_config(&access_token, &agent_id).await;
            let _ = this.update(cx, |this, cx| {
                if this.selected_agent_id.as_deref() != Some(agent_id.as_str()) {
                    return;
                }
                this.agent_config_loading = false;
                match result {
                    Ok(config) => this.selected_agent_config = Some(config),
                    Err(error) if error.is_authentication_failure() => {
                        this.forget_session(cx);
                    }
                    Err(error) => this.agent_config_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
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
                        .bg(status_color(agent.status(), theme)),
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
                            .id("create-agent")
                            .size(px(26.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|button| button.bg(theme.glass_hover()))
                            .on_click(cx.listener(|this, _, _, cx| this.open_agent_creator(cx)))
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
                    .when(self.agents_loading, |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.text_muted)
                                .child("Loading agents…"),
                        )
                    })
                    .when_some(self.agent_error.as_ref(), |list, error| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.danger)
                                .child(SharedString::from(error.clone())),
                        )
                    })
                    .when(
                        self.agents.is_empty()
                            && !self.agents_loading
                            && self.agent_error.is_none(),
                        |list| {
                            list.child(
                                div()
                                    .px(px(8.0))
                                    .py(px(12.0))
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child("No agents yet. Create one from a Keiki template."),
                            )
                        },
                    ),
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
        if self.creator_open {
            return self.render_agent_creator(theme, cx);
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
                            .w(px(460.0))
                            .flex()
                            .flex_col()
                            .when_some(self.creation_notice.as_ref(), |content, notice| {
                                content.child(
                                    div()
                                        .mb(px(16.0))
                                        .px(px(12.0))
                                        .py(px(10.0))
                                        .rounded(px(8.0))
                                        .border_1()
                                        .border_color(theme.accent)
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .line_height(px(18.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(notice.clone())),
                                )
                            })
                            .when(self.agent_config_loading, |content| {
                                content.child(
                                    div()
                                        .text_center()
                                        .text_size(crate::typography::ui_rems(13.0))
                                        .text_color(theme.text_muted)
                                        .child("Loading agent configuration…"),
                                )
                            })
                            .when_some(self.agent_config_error.as_ref(), |content, error| {
                                content.child(
                                    div()
                                        .text_center()
                                        .text_size(crate::typography::ui_rems(13.0))
                                        .line_height(px(19.0))
                                        .text_color(theme.danger)
                                        .child(SharedString::from(error.clone())),
                                )
                            })
                            .when_some(self.selected_agent_config.as_ref(), |content, config| {
                                content.child(
                                    div()
                                        .w_full()
                                        .p(px(16.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(10.0))
                                        .rounded(px(12.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.surface_card)
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(14.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child("Agent configuration"),
                                        )
                                        .child(metadata_row(
                                            "Model",
                                            config.model.clone(),
                                            theme,
                                        ))
                                        .child(metadata_row(
                                            "Runtime",
                                            config.runtime.as_str().into(),
                                            theme,
                                        ))
                                        .child(metadata_row(
                                            "Line",
                                            config
                                                .line_number
                                                .clone()
                                                .unwrap_or_else(|| "Not assigned".into()),
                                            theme,
                                        ))
                                        .child(metadata_row(
                                            "Harness",
                                            config.harness.as_str().into(),
                                            theme,
                                        ))
                                        .child(metadata_row(
                                            "Limits",
                                            format!(
                                                "{} steps · {} history",
                                                config.max_steps, config.history_limit
                                            ),
                                            theme,
                                        ))
                                        .child(metadata_row(
                                            "Features",
                                            format!(
                                                "{} enabled",
                                                config.features.enabled_count()
                                            ),
                                            theme,
                                        )),
                                )
                            })
                            .when(
                                self.selected_agent_id.is_none() && !self.agents_loading,
                                |content| content.child(empty_agent_state(theme)),
                            )
                            .when_some(self.selected_agent_config.as_ref(), |content, _| {
                                content.child(
                                    div()
                                        .mt(px(14.0))
                                        .text_center()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .child("Conversations and live messaging are the next milestone."),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_agent_creator(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let template_rows = self.templates.iter().map(|template| {
            let template_id = template.id.clone();
            let creating = self.creating_template_id.as_deref() == Some(template.id.as_str());
            div()
                .id(SharedString::from(format!("template-{}", template.id)))
                .w_full()
                .px(px(14.0))
                .py(px(12.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_card)
                .cursor_pointer()
                .hover(|card| card.border_color(theme.accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.create_agent_from_template(template_id.clone(), cx);
                }))
                .child(
                    div()
                        .w(px(34.0))
                        .text_size(crate::typography::ui_rems(22.0))
                        .child(SharedString::from(template.emoji.clone())),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(SharedString::from(template.name.clone())),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .line_height(px(16.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(template.blurb.clone())),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(10.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(template.model.clone())),
                        ),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .child(if creating { "Creating…" } else { "Create" }),
                )
        });

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
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Create an agent"),
                    )
                    .child(
                        div()
                            .id("close-agent-creator")
                            .cursor_pointer()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .hover(|button| button.text_color(theme.text))
                            .on_click(cx.listener(|this, _, _, cx| this.close_agent_creator(cx)))
                            .child("Close"),
                    ),
            )
            .child(
                div()
                    .id("agent-templates")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(24.0))
                    .py(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .when(self.templates_loading, |content| {
                        content.child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child("Loading Keiki templates…"),
                        )
                    })
                    .when_some(self.template_error.as_ref(), |content, error| {
                        content.child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.danger)
                                .child(SharedString::from(error.clone())),
                        )
                    })
                    .children(template_rows),
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

fn metadata_row(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

fn empty_agent_state(theme: &Theme) -> impl IntoElement {
    div()
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
                .child("No agents yet"),
        )
        .child(
            div()
                .mt(px(6.0))
                .text_size(crate::typography::ui_rems(13.0))
                .line_height(px(19.0))
                .text_color(theme.text_muted)
                .child("Create an agent from a Keiki template to get started."),
        )
}

fn status_color(status: AgentStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        AgentStatus::NeedsAttention => theme.danger,
        AgentStatus::Running => theme.accent,
        AgentStatus::Offline => theme.border,
    }
}
