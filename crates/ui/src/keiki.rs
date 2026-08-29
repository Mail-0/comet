//! Keiki cloud integration layered onto the existing Zeron state and views.

use std::{future::Future, time::Duration};

use chrono::{DateTime, Utc};
use gpui::{App, AsyncApp, Context, Entity, Task, TaskExt, WeakEntity};
use keiki_api::{AuthorizationFlow, Client, StoredCredentials, TokenSet};
use keiki_model::{
    AgentTemplateSummary, ConversationDetail, ConversationLocator, ConversationMessage,
    ConversationTakeover, CreateAgentFromTemplate, CreateAgentResponse, MessageDirection,
};
use zeron_doc::parts::MessagePart;
use zeron_doc::schema::{MessageRole, SessionMessageEntry};
use zeron_proto::{Chat, Device, Space};

use crate::state::AppState;

pub const DEVICE_ID: &str = "keiki-cloud";
pub const AGENT_PREFIX: &str = "keiki-agent:";
pub const CHAT_PREFIX: &str = "keiki-conv:";
pub const CREDENTIAL_KEY: &str = "keiki://oauth";
pub const DEFAULT_API_URL: &str = "https://onkeiki.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    SignedOut,
    Loading,
    SignedIn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeikiConversationPending {
    Takeover,
    HandBack,
    Block,
    Send,
    Steer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeikiConversation {
    pub chat_id: String,
    pub blocked: bool,
    pub takeover: Option<ConversationTakeover>,
    pub pending: Option<KeikiConversationPending>,
    pub error: Option<String>,
    pub steer_reply: Option<String>,
}

impl KeikiConversation {
    pub fn new(chat_id: String) -> Self {
        Self {
            chat_id,
            blocked: false,
            takeover: None,
            pending: None,
            error: None,
            steer_reply: None,
        }
    }
}

pub fn takeover_live(conversation: &KeikiConversation) -> bool {
    conversation
        .takeover
        .as_ref()
        .and_then(|takeover| parse_timestamp(&takeover.expires_at))
        .is_some_and(|expires| expires > Utc::now())
}

pub fn default_api_url() -> String {
    std::env::var("KEIKI_API_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

pub fn agent_id(agent_id: &str) -> String {
    format!("{AGENT_PREFIX}{agent_id}")
}

pub fn chat_id(agent_id: &str, phone: &str) -> String {
    format!("{CHAT_PREFIX}{agent_id}:{phone}")
}

pub fn is_keiki_chat(id: &str) -> bool {
    id.starts_with(CHAT_PREFIX)
}

pub fn is_keiki_space(id: &str) -> bool {
    id.starts_with(AGENT_PREFIX)
}

pub fn conversation_locator(id: &str) -> Option<ConversationLocator> {
    let rest = id.strip_prefix(CHAT_PREFIX)?;
    let (agent_id, phone) = rest.split_once(':')?;
    (!agent_id.is_empty() && !phone.is_empty()).then(|| ConversationLocator {
        identity: phone.to_string(),
        agent_id: Some(agent_id.to_string()),
        api_key: None,
    })
}

pub fn map_device() -> Device {
    Device {
        id: DEVICE_ID.to_string(),
        name: "Keiki".to_string(),
        platform: "cloud".to_string(),
        last_seen_at: Some(Utc::now()),
        created_at: None,
        version: None,
    }
}

pub fn map_agent(agent: &keiki_model::AgentSummary) -> Space {
    Space {
        id: agent_id(&agent.id),
        device_id: DEVICE_ID.to_string(),
        path: agent.name.clone(),
        name: Some(agent.name.clone()),
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        created_at: parse_timestamp(&agent.created_at).unwrap_or_else(Utc::now),
    }
}

pub fn map_conversation(conversation: &keiki_model::ConversationSummary) -> Option<Chat> {
    let agent_id = conversation.agent_id.as_deref()?;
    let created_at = timestamp_or_now(
        &conversation.last_message_at,
        "lastMessageAt",
        "conversation",
    );
    Some(Chat {
        id: chat_id(agent_id, &conversation.phone),
        device_id: DEVICE_ID.to_string(),
        title: Some(
            conversation
                .contact_name
                .clone()
                .unwrap_or_else(|| conversation.phone.clone()),
        ),
        archived: false,
        cwd: None,
        branch: None,
        checkout_id: None,
        source_context: None,
        config: None,
        last_message_preview: Some(conversation.last_message.clone()),
        last_message_at: Some(created_at),
        created_at,
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some(crate::keiki::agent_id(agent_id)),
        last_seen_at: None,
        room_gen: None,
    })
}

pub fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&Utc));
    }
    let mut normalized = value.replacen(' ', "T", 1);
    if let Some(time_start) = normalized.find('T')
        && let Some(offset_start) =
            normalized[time_start + 1..]
                .char_indices()
                .rev()
                .find_map(|(index, character)| {
                    (character == '+' || character == '-').then_some(time_start + 1 + index)
                })
    {
        let offset = &normalized[offset_start..];
        if offset.len() == 3
            && offset.as_bytes()[1].is_ascii_digit()
            && offset.as_bytes()[2].is_ascii_digit()
        {
            normalized.push_str(":00");
        }
    }
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn timestamp_or_now(value: &str, field: &'static str, object: &'static str) -> DateTime<Utc> {
    parse_timestamp(value).unwrap_or_else(|| {
        tracing::warn!(
            field,
            object,
            value = %value,
            "Keiki timestamp could not be parsed; using current time"
        );
        Utc::now()
    })
}

fn request_task_error(operation: &'static str, error: impl std::fmt::Display) -> keiki_api::Error {
    tracing::error!(operation, error = %error, "Keiki request task failed");
    keiki_api::Error::TaskFailed(format!("{operation}: {error}"))
}

pub(crate) async fn authorized<T, F, Fut>(
    entity: &WeakEntity<AppState>,
    client: Client,
    token: TokenSet,
    credentials: StoredCredentials,
    operation: &'static str,
    make_request: F,
    cx: &mut AsyncApp,
) -> Result<T, keiki_api::Error>
where
    T: Send + 'static,
    F: Fn(Client, String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<T, keiki_api::Error>> + Send + 'static,
{
    let access_token = token.access_token().to_string();
    let request = make_request.clone();
    let initial_client = client.clone();
    let request_task = cx.update(|cx| {
        gpui_tokio::Tokio::spawn(
            cx,
            async move { request(initial_client, access_token).await },
        )
    });
    let result = request_task
        .await
        .map_err(|error| request_task_error(operation, error))?;
    let Err(error) = result else {
        return result;
    };
    if !error.is_authentication_failure() {
        return Err(error);
    }

    let refresh_client = client.clone();
    let refresh_credentials = credentials.clone();
    let refresh_task = cx.update(|cx| {
        gpui_tokio::Tokio::spawn(cx, async move {
            refresh_client.refresh_token(&refresh_credentials).await
        })
    });
    let refreshed = match refresh_task.await {
        Ok(Ok(tokens)) => tokens,
        Ok(Err(error)) => {
            if let Err(cleanup_error) =
                expire_keiki_session(entity, Some("Keiki session expired"), cx).await
            {
                tracing::error!(%cleanup_error, "Keiki session expiry cleanup failed");
            }
            return Err(error);
        }
        Err(error) => {
            let refresh_error = request_task_error("Keiki token refresh", error);
            if let Err(cleanup_error) =
                expire_keiki_session(entity, Some("Keiki session expired"), cx).await
            {
                tracing::error!(%cleanup_error, "Keiki session expiry cleanup failed");
            }
            return Err(refresh_error);
        }
    };
    let refreshed_credentials = refreshed.stored_credentials(credentials.client_id.clone());
    persist_credentials(&credentials.client_id, &refreshed, entity, cx)
        .await
        .map_err(keiki_api::Error::Local)?;
    entity
        .update(cx, |state, _| {
            state.keiki_token = Some(refreshed.clone());
            state.keiki_credentials = Some(refreshed_credentials);
        })
        .map_err(|error| request_task_error("Keiki token state update", error))?;

    let retry_client = client;
    let retry_token = refreshed.access_token().to_string();
    let retry = make_request(retry_client, retry_token);
    cx.update(|cx| gpui_tokio::Tokio::spawn(cx, retry))
        .await
        .map_err(|error| request_task_error(operation, error))?
}

fn conversation_error(error: &keiki_api::Error) -> String {
    match error {
        keiki_api::Error::Api { message, .. } => message.clone(),
        _ => error.to_string(),
    }
}

fn set_conversation_error(
    state: &Entity<AppState>,
    chat_id: &str,
    message: String,
    cx: &mut AsyncApp,
) {
    state.update(cx, |state, cx| {
        if let Some(conversation) = state.keiki_conversation.as_mut()
            && conversation.chat_id == chat_id
            && state.selected_chat.as_deref() == Some(chat_id)
        {
            conversation.pending = None;
            conversation.error = Some(message);
            cx.notify();
        }
    });
}

fn selected_conversation(
    state: &Entity<AppState>,
    cx: &gpui::App,
) -> Option<(String, Client, TokenSet, StoredCredentials)> {
    let state = state.read(cx);
    let conversation = state.keiki_conversation()?;
    Some((
        conversation.chat_id.clone(),
        state.keiki_client.clone()?,
        state.keiki_token.clone()?,
        state.keiki_credentials.clone()?,
    ))
}

#[derive(Clone, Copy)]
enum ConversationAction {
    Takeover,
    HandBack,
    Block,
    Unblock,
    Send,
    Steer,
}

fn spawn_conversation_action<R: 'static>(
    state: Entity<AppState>,
    chat_id: String,
    action: ConversationAction,
    text: Option<String>,
    composer: Option<Entity<crate::composer::Composer>>,
    cx: &mut Context<R>,
) {
    let pending = match action {
        ConversationAction::Takeover => KeikiConversationPending::Takeover,
        ConversationAction::HandBack => KeikiConversationPending::HandBack,
        ConversationAction::Block | ConversationAction::Unblock => KeikiConversationPending::Block,
        ConversationAction::Send => KeikiConversationPending::Send,
        ConversationAction::Steer => KeikiConversationPending::Steer,
    };
    let started = state.update(cx, |state, cx| {
        let started = state.set_keiki_pending(&chat_id, pending);
        if started {
            cx.notify();
        }
        started
    });
    if !started {
        return;
    }
    let task_state = state.clone();
    cx.spawn(async move |_, cx| {
        let context = task_state.update(cx, |state, _| {
            Some((
                state.keiki_client.clone()?,
                state.keiki_token.clone()?,
                state.keiki_credentials.clone()?,
            ))
        });
        let Some((client, token, credentials)) = context else {
            set_conversation_error(
                &task_state,
                &chat_id,
                "Keiki credentials are unavailable".to_string(),
                cx,
            );
            return;
        };
        let Some(locator) = conversation_locator(&chat_id) else {
            set_conversation_error(
                &task_state,
                &chat_id,
                "The selected Keiki conversation is invalid".to_string(),
                cx,
            );
            return;
        };
        let request_locator = locator.clone();
        let request_text = text.unwrap_or_default();
        let result = match action {
            ConversationAction::Takeover => authorized(
                &task_state.downgrade(),
                client,
                token,
                credentials,
                "Keiki takeover",
                move |client, access_token| {
                    let locator = request_locator.clone();
                    async move {
                        client
                            .start_conversation_takeover(&access_token, &locator)
                            .await
                    }
                },
                cx,
            )
            .await
            .map(|takeover| ActionResult::Takeover(takeover)),
            ConversationAction::HandBack => authorized(
                &task_state.downgrade(),
                client,
                token,
                credentials,
                "Keiki hand back",
                move |client, access_token| {
                    let locator = request_locator.clone();
                    async move {
                        client
                            .end_conversation_takeover(&access_token, &locator)
                            .await
                    }
                },
                cx,
            )
            .await
            .map(|()| ActionResult::HandBack),
            ConversationAction::Block | ConversationAction::Unblock => {
                let blocked = matches!(action, ConversationAction::Block);
                authorized(
                    &task_state.downgrade(),
                    client,
                    token,
                    credentials,
                    "Keiki block update",
                    move |client, access_token| {
                        let locator = request_locator.clone();
                        async move {
                            client
                                .set_conversation_blocked(&access_token, &locator, blocked)
                                .await
                        }
                    },
                    cx,
                )
                .await
                .map(|response| ActionResult::Blocked(response.blocked))
            }
            ConversationAction::Send => {
                let refresh_client = client.clone();
                let refresh_token = token.clone();
                let refresh_credentials = credentials.clone();
                let sent = authorized(
                    &task_state.downgrade(),
                    client,
                    token,
                    credentials,
                    "Keiki message send",
                    move |client, access_token| {
                        let locator = request_locator.clone();
                        let text = request_text.clone();
                        async move {
                            client
                                .send_conversation_message(&access_token, &locator, text)
                                .await
                        }
                    },
                    cx,
                )
                .await;
                match sent {
                    Ok(response) => {
                        let fetch_locator = locator.clone();
                        let detail = authorized(
                            &task_state.downgrade(),
                            refresh_client,
                            refresh_token,
                            refresh_credentials,
                            "Keiki transcript refresh",
                            move |client, access_token| {
                                let locator = fetch_locator.clone();
                                async move { client.conversation(&access_token, &locator).await }
                            },
                            cx,
                        )
                        .await
                        .ok();
                        Ok(ActionResult::Sent { response, detail })
                    }
                    Err(error) => Err(error),
                }
            }
            ConversationAction::Steer => {
                let refresh_client = client.clone();
                let refresh_token = token.clone();
                let refresh_credentials = credentials.clone();
                let steered = authorized(
                    &task_state.downgrade(),
                    client,
                    token,
                    credentials,
                    "Keiki steer",
                    move |client, access_token| {
                        let locator = request_locator.clone();
                        let text = request_text.clone();
                        async move {
                            client
                                .steer_conversation(&access_token, &locator, text)
                                .await
                        }
                    },
                    cx,
                )
                .await;
                match steered {
                    Ok(response) => {
                        let fetch_locator = locator.clone();
                        let detail = match authorized(
                            &task_state.downgrade(),
                            refresh_client,
                            refresh_token,
                            refresh_credentials,
                            "Keiki transcript refresh",
                            move |client, access_token| {
                                let locator = fetch_locator.clone();
                                async move { client.conversation(&access_token, &locator).await }
                            },
                            cx,
                        )
                        .await
                        {
                            Ok(detail) => Some(detail),
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    %chat_id,
                                    "Keiki transcript refresh after steer failed"
                                );
                                None
                            }
                        };
                        Ok(ActionResult::Steered {
                            reply: response.reply,
                            detail,
                        })
                    }
                    Err(error) => Err(error),
                }
            }
        };
        task_state.update(cx, |state, cx| {
            let Some(conversation) = state.keiki_conversation.as_mut() else {
                return;
            };
            if conversation.chat_id != chat_id
                || state.selected_chat.as_deref() != Some(chat_id.as_str())
            {
                return;
            }
            conversation.pending = None;
            match result {
                Ok(ActionResult::Takeover(takeover)) => conversation.takeover = Some(takeover),
                Ok(ActionResult::HandBack) => conversation.takeover = None,
                Ok(ActionResult::Blocked(blocked)) => conversation.blocked = blocked,
                Ok(ActionResult::Sent { response, detail }) => {
                    conversation.takeover = Some(response.takeover);
                    if let Some(detail) = detail {
                        if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                            state.apply_transcript(map_transcript(&detail));
                        }
                    }
                    if let Some(composer) = composer {
                        composer.update(cx, |composer, cx| {
                            composer.clear_after_keiki_send(cx);
                        });
                    }
                }
                Ok(ActionResult::Steered { reply, detail }) => {
                    conversation.steer_reply = Some(reply);
                    if let Some(detail) = detail {
                        state.apply_transcript(map_transcript(&detail));
                    }
                }
                Err(error) => {
                    if matches!(
                        error,
                        keiki_api::Error::Api { status, .. } if status.as_u16() == 409
                    ) && matches!(action, ConversationAction::Send)
                    {
                        conversation.takeover = None;
                    }
                    conversation.error = Some(conversation_error(&error));
                }
            }
            cx.notify();
        });
    })
    .detach();
}

enum ActionResult {
    Takeover(ConversationTakeover),
    HandBack,
    Blocked(bool),
    Sent {
        response: keiki_api::SendConversationMessageResponse,
        detail: Option<ConversationDetail>,
    },
    Steered {
        reply: String,
        detail: Option<ConversationDetail>,
    },
}

pub fn take_over<R: 'static>(state: Entity<AppState>, cx: &mut Context<R>) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(state, chat_id, ConversationAction::Takeover, None, None, cx);
}

pub fn hand_back<R: 'static>(state: Entity<AppState>, cx: &mut Context<R>) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(state, chat_id, ConversationAction::HandBack, None, None, cx);
}

pub fn block<R: 'static>(state: Entity<AppState>, cx: &mut Context<R>) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(state, chat_id, ConversationAction::Block, None, None, cx);
}

pub fn unblock<R: 'static>(state: Entity<AppState>, cx: &mut Context<R>) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(state, chat_id, ConversationAction::Unblock, None, None, cx);
}

pub fn send<R: 'static>(
    state: Entity<AppState>,
    text: String,
    composer: Entity<crate::composer::Composer>,
    cx: &mut Context<R>,
) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(
        state,
        chat_id,
        ConversationAction::Send,
        Some(text),
        Some(composer),
        cx,
    );
}

pub fn steer<R: 'static>(state: Entity<AppState>, text: String, cx: &mut Context<R>) {
    let Some((chat_id, ..)) = selected_conversation(&state, cx) else {
        return;
    };
    spawn_conversation_action(
        state,
        chat_id,
        ConversationAction::Steer,
        Some(text),
        None,
        cx,
    );
}

pub fn map_message(message: &ConversationMessage) -> Option<SessionMessageEntry> {
    let created_at = timestamp_or_now(&message.created_at, "createdAt", "message");
    Some(SessionMessageEntry {
        id: message.id.clone(),
        role: match message.direction {
            MessageDirection::Inbound => MessageRole::User,
            MessageDirection::Outbound => MessageRole::Assistant,
        },
        parts: vec![MessagePart::Text {
            id: format!("{}:text", message.id),
            text: transcript_message_text(message),
        }],
        created_at: created_at.timestamp_millis(),
        device_id: DEVICE_ID.to_string(),
        status: None,
        continuation_of: None,
    })
}

fn transcript_message_text(message: &ConversationMessage) -> String {
    if !message.internal {
        return message.content.clone();
    }
    let label = message
        .staff_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || "Internal turn".to_string(),
            |name| format!("Internal turn — {name}"),
        );
    format!("{label}\n\n{}", message.content)
}

pub fn map_transcript(detail: &ConversationDetail) -> Vec<SessionMessageEntry> {
    detail.messages.iter().filter_map(map_message).collect()
}

pub fn start(state: Entity<AppState>, api_url: String, cx: &mut App) {
    let client = Client::new(api_url);
    state.update(cx, |state, _| {
        state.keiki_client = Some(client.clone());
        state.keiki_status = SessionStatus::Loading;
    });
    let boot_state = state.clone();
    let credentials_task = cx.read_credentials(CREDENTIAL_KEY);
    let task = cx.spawn(async move |cx| {
        let credentials = match credentials_task.await {
            Ok(Some((_, payload))) => serde_json::from_slice::<StoredCredentials>(&payload).ok(),
            _ => None,
        };
        let Some(credentials) = credentials else {
            boot_state.update(cx, |state, cx| {
                state.keiki_status = SessionStatus::SignedOut;
                state.keiki_error = None;
                cx.notify();
            });
            return;
        };
        let client = boot_state.read_with(cx, |state, _| state.keiki_client.clone());
        let Some(client) = client else {
            return;
        };
        let refresh_client = client.clone();
        let refresh_credentials = credentials.clone();
        let refresh = cx.update(|cx| {
            gpui_tokio::Tokio::spawn(cx, async move {
                refresh_client.refresh_token(&refresh_credentials).await
            })
        });
        match refresh.await {
            Ok(Ok(tokens)) => {
                if let Err(error) = persist_credentials(
                    &credentials.client_id,
                    &tokens,
                    &boot_state.downgrade(),
                    cx,
                )
                .await
                {
                    tracing::warn!(%error, "Keiki credential persistence failed");
                }
                boot_state.update(cx, |state, cx| {
                    state.keiki_credentials = Some(credentials);
                    state.keiki_token = Some(tokens);
                    state.keiki_status = SessionStatus::SignedIn;
                    state.keiki_error = None;
                    cx.notify();
                });
                poll(boot_state.downgrade(), cx).await;
            }
            Ok(Err(error)) if error.is_invalid_refresh_token() => {
                if let Err(cleanup_error) =
                    expire_keiki_session(&boot_state.downgrade(), None, cx).await
                {
                    tracing::warn!(%cleanup_error, "Keiki credential cleanup failed");
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "Keiki credential restore failed");
                boot_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::Error;
                    state.keiki_error = Some(error.to_string());
                    cx.notify();
                });
            }
            Err(error) => {
                tracing::warn!(error = %error, "Keiki credential restore task failed");
                boot_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::Error;
                    state.keiki_error = Some(error.to_string());
                    cx.notify();
                });
            }
        }
    });
    state.update(cx, |state, _| state.keiki_task = Some(task));
}

async fn persist_credentials(
    client_id: &str,
    tokens: &TokenSet,
    entity: &gpui::WeakEntity<AppState>,
    cx: &mut gpui::AsyncApp,
) -> Result<(), String> {
    let credentials = tokens.stored_credentials(client_id.to_string());
    let payload = serde_json::to_vec(&credentials).map_err(|error| error.to_string())?;
    let task = entity
        .update(cx, |_, cx| {
            cx.write_credentials(CREDENTIAL_KEY, "OAuth", &payload)
        })
        .map_err(|error| error.to_string())?;
    task.await.map_err(|error| error.to_string())
}

async fn expire_keiki_session(
    entity: &WeakEntity<AppState>,
    message: Option<&'static str>,
    cx: &mut AsyncApp,
) -> Result<(), keiki_api::Error> {
    let delete_credentials = entity
        .update(cx, |state, cx| {
            state.mark_keiki_signed_out(message.map(str::to_string));
            let delete_credentials = cx.delete_credentials(CREDENTIAL_KEY);
            cx.notify();
            delete_credentials
        })
        .map_err(|error| request_task_error("Keiki session expiry state update", error))?;
    delete_credentials
        .await
        .map_err(|error| request_task_error("Keiki credential deletion", error))
}

pub(crate) async fn refresh_keiki_snapshot(
    entity: gpui::WeakEntity<AppState>,
    cx: &mut gpui::AsyncApp,
) -> Result<(), keiki_api::Error> {
    let context = entity
        .update(cx, |state, _| {
            Some((
                state.keiki_client.clone()?,
                state.keiki_token.clone()?,
                state.keiki_credentials.clone()?,
            ))
        })
        .map_err(|error| request_task_error("Keiki state read", error))?
        .ok_or_else(|| keiki_api::Error::Local("Keiki credentials are unavailable".into()))?;
    let (client, token, credentials) = context;
    let agents = authorized(
        &entity,
        client.clone(),
        token,
        credentials,
        "Keiki agent list",
        |client, access_token| async move { client.list_agents(&access_token).await },
        cx,
    )
    .await?;
    let context = entity
        .update(cx, |state, _| {
            Some((
                state.keiki_client.clone()?,
                state.keiki_token.clone()?,
                state.keiki_credentials.clone()?,
            ))
        })
        .map_err(|error| request_task_error("Keiki state read", error))?
        .ok_or_else(|| keiki_api::Error::Local("Keiki credentials are unavailable".into()))?;
    let (client, token, credentials) = context;
    let conversations = authorized(
        &entity,
        client,
        token,
        credentials,
        "Keiki conversation list",
        |client, access_token| async move { client.list_conversations(&access_token).await },
        cx,
    )
    .await?;
    let spaces = agents.iter().map(map_agent).collect();
    let chats = conversations.iter().filter_map(map_conversation).collect();
    entity
        .update(cx, |state, cx| {
            state.apply_keiki_snapshot(spaces, chats);
            cx.notify();
        })
        .map_err(|error| request_task_error("Keiki snapshot apply", error))
}

pub(crate) async fn list_agent_templates(
    entity: &WeakEntity<AppState>,
    cx: &mut AsyncApp,
) -> Result<Vec<AgentTemplateSummary>, keiki_api::Error> {
    let context = entity
        .update(cx, |state, _| {
            Some((
                state.keiki_client.clone()?,
                state.keiki_token.clone()?,
                state.keiki_credentials.clone()?,
            ))
        })
        .map_err(|error| request_task_error("Keiki template state read", error))?
        .ok_or_else(|| keiki_api::Error::Local("Keiki credentials are unavailable".into()))?;
    let (client, token, credentials) = context;
    authorized(
        entity,
        client,
        token,
        credentials,
        "Keiki template list",
        |client, access_token| async move { client.list_agent_templates(&access_token).await },
        cx,
    )
    .await
}

pub(crate) async fn create_agent_from_template(
    entity: &WeakEntity<AppState>,
    input: CreateAgentFromTemplate,
    cx: &mut AsyncApp,
) -> Result<CreateAgentResponse, keiki_api::Error> {
    let context = entity
        .update(cx, |state, _| {
            Some((
                state.keiki_client.clone()?,
                state.keiki_token.clone()?,
                state.keiki_credentials.clone()?,
            ))
        })
        .map_err(|error| request_task_error("Keiki agent state read", error))?
        .ok_or_else(|| keiki_api::Error::Local("Keiki credentials are unavailable".into()))?;
    let (client, token, credentials) = context;
    authorized(
        entity,
        client,
        token,
        credentials,
        "Keiki agent creation",
        move |client, access_token| {
            let input = input.clone();
            async move {
                client
                    .create_agent_from_template(&access_token, &input)
                    .await
            }
        },
        cx,
    )
    .await
}

async fn poll(entity: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp) {
    loop {
        let signed_in =
            match entity.update(cx, |state, _| state.keiki_status == SessionStatus::SignedIn) {
                Ok(signed_in) => signed_in,
                Err(error) => {
                    tracing::warn!(%error, "Keiki poll state read failed");
                    return;
                }
            };
        if !signed_in {
            return;
        }
        if let Err(error) = refresh_keiki_snapshot(entity.clone(), cx).await {
            tracing::warn!(error = %error, "Keiki poll failed");
            let signed_in =
                match entity.update(cx, |state, _| state.keiki_status == SessionStatus::SignedIn) {
                    Ok(signed_in) => signed_in,
                    Err(error) => {
                        tracing::warn!(%error, "Keiki poll state read failed");
                        return;
                    }
                };
            if !signed_in {
                return;
            }
            cx.background_executor()
                .timer(Duration::from_secs(10))
                .await;
            continue;
        }
        cx.background_executor()
            .timer(Duration::from_secs(15))
            .await;
    }
}

pub fn spawn_transcript_watch(cx: &mut Context<AppState>, chat_id: String) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let locator = match conversation_locator(&chat_id) {
            Some(locator) => locator,
            None => return,
        };
        let context = this
            .update(cx, |state, _| {
                Some((
                    state.keiki_client.clone()?,
                    state.keiki_token.clone()?,
                    state.keiki_credentials.clone()?,
                ))
            })
            .ok()
            .flatten();
        let Some((client, token, credentials)) = context else {
            return;
        };
        let request_locator = locator.clone();
        match authorized(
            &this,
            client,
            token,
            credentials,
            "Keiki transcript fetch",
            move |client, access_token| {
                let locator = request_locator.clone();
                async move { client.conversation(&access_token, &locator).await }
            },
            cx,
        )
        .await
        {
            Ok(detail) => {
                this.update(cx, |state, cx| {
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        state.set_keiki_conversation_detail(&chat_id, &detail);
                        state.apply_transcript(map_transcript(&detail));
                        cx.notify();
                    }
                })
                .ok();
            }
            Err(error) => {
                tracing::warn!(%error, %chat_id, "Keiki transcript fetch failed");
                this.update(cx, |state, cx| {
                    if let Some(conversation) = state.keiki_conversation.as_mut()
                        && conversation.chat_id == chat_id
                    {
                        conversation.error = Some(error.to_string());
                        cx.notify();
                    }
                })
                .ok();
            }
        }
    })
}

async fn complete_callback(
    state: WeakEntity<AppState>,
    flow: AuthorizationFlow,
    client: Client,
    callback: String,
    cx: &mut AsyncApp,
) {
    let result = async {
        let code = flow.authorization_code(&callback)?;
        let exchange_flow = flow.clone();
        let exchange_client = client.clone();
        let exchange = cx.update(|cx| {
            gpui_tokio::Tokio::spawn(cx, async move {
                exchange_client.exchange_code(&exchange_flow, &code).await
            })
        });
        let tokens = exchange
            .await
            .map_err(|error| request_task_error("authorization-code exchange", error))??;
        let credentials = flow.stored_credentials(&tokens);
        let payload = serde_json::to_vec(&credentials)
            .map_err(|error| keiki_api::Error::Local(error.to_string()))?;
        let write = state.update(cx, |_, cx| {
            cx.write_credentials(CREDENTIAL_KEY, "OAuth", &payload)
        });
        write
            .map_err(|error| request_task_error("credential write setup", error))?
            .await
            .map_err(|error| request_task_error("credential write", error))?;
        Ok::<_, keiki_api::Error>((tokens, credentials))
    }
    .await;
    let success = result.is_ok();
    state
        .update(cx, |state, cx| match result {
            Ok((tokens, credentials)) => {
                state.keiki_token = Some(tokens);
                state.keiki_credentials = Some(credentials);
                state.keiki_status = SessionStatus::SignedIn;
                state.keiki_error = None;
                cx.notify();
            }
            Err(error) => {
                state.keiki_status = SessionStatus::Error;
                state.keiki_error = Some(error.to_string());
                tracing::warn!(error = %error, "Keiki sign-in failed");
                cx.notify();
            }
        })
        .ok();
    if success {
        poll(state, cx).await;
    }
}

pub fn handle_callback(state: &mut AppState, callback: &str, cx: &mut Context<AppState>) {
    let Some(flow) = state.keiki_flow.take() else {
        return;
    };
    let Some(client) = state.keiki_client.clone() else {
        return;
    };
    let state = cx.entity().downgrade();
    let callback = callback.to_string();
    cx.spawn(async move |_, cx| {
        complete_callback(state, flow, client, callback, cx).await;
    })
    .detach();
}

pub fn begin_sign_in(state: Entity<AppState>, cx: &mut Context<crate::shell::Shell>) {
    state.update(cx, |state, cx| {
        state.keiki_status = SessionStatus::Loading;
        state.keiki_error = None;
        state.keiki_flow = None;
        cx.notify();
    });
    let task_state = state.clone();
    let task = cx.spawn(async move |this, cx| {
        let client = task_state.read_with(cx, |state, _| state.keiki_client.clone());
        let Some(client) = client else {
            return;
        };
        let result = async {
            let listener_task =
                cx.update(|cx| gpui_tokio::Tokio::spawn(cx, keiki_api::bind_loopback_listener()));
            let (listener, redirect_uri) = listener_task
                .await
                .map_err(|error| request_task_error("OAuth loopback listener", error))??;
            let discovery_client = client.clone();
            let discovery = cx.update(|cx| {
                gpui_tokio::Tokio::spawn(cx, async move { discovery_client.discover_oauth().await })
            });
            discovery
                .await
                .map_err(|error| request_task_error("OAuth discovery", error))??;
            let registration_client = client.clone();
            let registration_redirect_uri = redirect_uri.clone();
            let registration = cx.update(|cx| {
                gpui_tokio::Tokio::spawn(cx, async move {
                    registration_client
                        .register_client(&registration_redirect_uri)
                        .await
                })
            });
            let client_id = registration
                .await
                .map_err(|error| request_task_error("OAuth client registration", error))??;
            let flow = AuthorizationFlow::new(client_id, redirect_uri.clone());
            let url = client.authorization_url(&flow)?;
            task_state.update(cx, |state, _| state.keiki_flow = Some(flow));
            let callback_task = cx.update(|cx| {
                gpui_tokio::Tokio::spawn(
                    cx,
                    keiki_api::wait_for_loopback_callback(listener, redirect_uri),
                )
            });
            Ok::<_, keiki_api::Error>((url, callback_task))
        }
        .await;
        let (url, callback_task) = match result {
            Ok(result) => result,
            Err(error) => {
                this.update(cx, |shell, cx| {
                    task_state.update(cx, |state, cx| {
                        state.keiki_status = SessionStatus::Error;
                        state.keiki_error = Some(error.to_string());
                        cx.notify();
                    });
                    shell.set_sidebar_notice(format!("Keiki sign in failed: {error}"));
                    cx.notify();
                })
                .ok();
                return;
            }
        };
        this.update(cx, |_, cx| cx.open_url(url.as_str())).ok();
        let callback = match callback_task
            .await
            .map_err(|error| request_task_error("OAuth loopback callback", error))
        {
            Ok(Ok(callback)) => callback,
            Ok(Err(error)) => {
                task_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::Error;
                    state.keiki_error = Some(error.to_string());
                    cx.notify();
                });
                return;
            }
            Err(error) => {
                task_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::Error;
                    state.keiki_error = Some(error.to_string());
                    cx.notify();
                });
                return;
            }
        };
        let flow = task_state.update(cx, |state, _| state.keiki_flow.take());
        let Some(flow) = flow else {
            return;
        };
        complete_callback(task_state.downgrade(), flow, client, callback, cx).await;
    });
    state.update(cx, |state, _| state.keiki_task = Some(task));
}

pub fn sign_out(state: Entity<AppState>, cx: &mut Context<crate::shell::Shell>) -> Task<()> {
    cx.spawn(async move |_, cx| {
        let credentials = state.read_with(cx, |state, _| {
            (state.keiki_client.clone(), state.keiki_token.clone())
        });
        if let (Some(client), Some(token)) = credentials {
            let access_token = token.access_token().to_string();
            let revoke = cx.update(|cx| {
                gpui_tokio::Tokio::spawn(
                    cx,
                    async move { client.revoke_token(&access_token).await },
                )
            });
            match revoke.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "Keiki token revocation failed"),
                Err(error) => tracing::warn!(%error, "Keiki token revocation task failed"),
            };
        }
        state.update(cx, |state, cx| {
            state.mark_keiki_signed_out(None);
            cx.delete_credentials(CREDENTIAL_KEY)
                .detach_and_log_err(&*cx);
            cx.notify();
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_round_trip() {
        let id = chat_id("agent-1", "+15551234");
        assert_eq!(conversation_locator(&id).unwrap().identity, "+15551234");
        assert_eq!(
            conversation_locator(&id).unwrap().agent_id.as_deref(),
            Some("agent-1")
        );
    }

    #[test]
    fn message_roles_follow_direction() {
        let message = ConversationMessage {
            id: "m".into(),
            direction: MessageDirection::Inbound,
            content: "hello".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            trace_id: None,
            trace_duration_ms: None,
            trace_status: None,
            trace_model: None,
            trace_tokens_in: None,
            trace_tokens_out: None,
            trace_total_steps: None,
            trace_error: None,
            internal: false,
            staff_name: None,
        };
        assert_eq!(map_message(&message).unwrap().role, MessageRole::User);
    }

    #[test]
    fn internal_messages_are_labeled_with_staff_name() {
        let message = ConversationMessage {
            id: "m".into(),
            direction: MessageDirection::Inbound,
            content: "I can help with that.".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            trace_id: None,
            trace_duration_ms: None,
            trace_status: None,
            trace_model: None,
            trace_tokens_in: None,
            trace_tokens_out: None,
            trace_total_steps: None,
            trace_error: None,
            internal: true,
            staff_name: Some("Devin".into()),
        };
        let entry = map_message(&message).expect("internal messages map");
        assert_eq!(entry.role, MessageRole::User);
        assert_eq!(
            entry.parts,
            vec![MessagePart::Text {
                id: "m:text".into(),
                text: "Internal turn — Devin\n\nI can help with that.".into(),
            }]
        );
    }

    #[test]
    fn parses_rfc3339_timestamps() {
        assert_eq!(
            parse_timestamp("2026-01-01T00:00:00+02:00")
                .unwrap()
                .timestamp(),
            1_767_218_400
        );
    }

    #[test]
    fn parses_postgres_timestamps_with_fractional_seconds() {
        assert_eq!(
            parse_timestamp("2026-08-28 16:58:29.714004+00")
                .unwrap()
                .to_rfc3339(),
            "2026-08-28T16:58:29.714004+00:00"
        );
    }

    #[test]
    fn parses_postgres_timestamps_without_fractional_seconds() {
        assert_eq!(
            parse_timestamp("2026-08-28 16:58:29+00")
                .unwrap()
                .to_rfc3339(),
            "2026-08-28T16:58:29+00:00"
        );
    }

    #[test]
    fn parses_space_separated_timestamps_with_trailing_z() {
        assert_eq!(
            parse_timestamp("2026-08-28 16:58:29.7Z")
                .unwrap()
                .to_rfc3339(),
            "2026-08-28T16:58:29.700+00:00"
        );
    }

    #[test]
    fn rejects_invalid_timestamps() {
        assert_eq!(parse_timestamp("not-a-timestamp"), None);
    }

    #[test]
    fn conversation_with_invalid_timestamp_is_kept() {
        let conversation: keiki_model::ConversationSummary =
            serde_json::from_value(serde_json::json!({
                "phone": "+15551234",
                "contactName": null,
                "agentName": "Support",
                "agentId": "agent-1",
                "apiKey": "redacted-test-key",
                "lastMessage": "Hello",
                "lastMessageAt": "not-a-timestamp",
                "lastDirection": "inbound",
                "messageCount": 1,
                "isActive": true,
                "hasErrors": false
            }))
            .unwrap();

        assert!(map_conversation(&conversation).is_some());
    }
}
