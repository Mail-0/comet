//! Keiki cloud integration layered onto the existing Zeron state and views.

use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task, TaskExt};
use keiki_api::{AuthorizationFlow, Client, OAUTH_REDIRECT_URI, StoredCredentials, TokenSet};
use keiki_model::{ConversationDetail, ConversationLocator, ConversationMessage, MessageDirection};
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
    let created_at = parse_timestamp(&conversation.last_message_at)?;
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
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn map_message(message: &ConversationMessage) -> Option<SessionMessageEntry> {
    let created_at = parse_timestamp(&message.created_at)?;
    Some(SessionMessageEntry {
        id: message.id.clone(),
        role: match message.direction {
            MessageDirection::Inbound => MessageRole::User,
            MessageDirection::Outbound => MessageRole::Assistant,
        },
        parts: vec![MessagePart::Text {
            id: format!("{}:text", message.id),
            text: message.content.clone(),
        }],
        created_at: created_at.timestamp_millis(),
        device_id: DEVICE_ID.to_string(),
        status: None,
        continuation_of: None,
    })
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
                cx.notify();
            });
            return;
        };
        let client = boot_state.read_with(cx, |state, _| state.keiki_client.clone());
        let Some(client) = client else {
            return;
        };
        match client.refresh_token(&credentials).await {
            Ok(tokens) => {
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
                    cx.notify();
                });
                poll(boot_state.downgrade(), cx).await;
            }
            Err(error) if error.is_invalid_refresh_token() => {
                boot_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::SignedOut;
                    cx.delete_credentials(CREDENTIAL_KEY)
                        .detach_and_log_err(&*cx);
                    cx.notify();
                });
            }
            Err(error) => {
                tracing::warn!(error = %error, "Keiki credential restore failed");
                boot_state.update(cx, |state, cx| {
                    state.keiki_status = SessionStatus::Error;
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

async fn poll(entity: gpui::WeakEntity<AppState>, cx: &mut gpui::AsyncApp) {
    loop {
        let context = entity
            .update(cx, |state, _| {
                Some((
                    state.keiki_client.clone()?,
                    state.keiki_token.clone()?,
                    state.keiki_credentials.clone()?,
                ))
            })
            .ok()
            .flatten();
        let Some((client, mut token, credentials)) = context else {
            return;
        };
        let agents = match client.list_agents(token.access_token()).await {
            Ok(agents) => agents,
            Err(error) if error.is_authentication_failure() => {
                match client.refresh_token(&credentials).await {
                    Ok(refreshed) => {
                        if let Err(error) =
                            persist_credentials(&credentials.client_id, &refreshed, &entity, cx)
                                .await
                        {
                            tracing::warn!(%error, "Keiki credential persistence failed");
                        }
                        token = refreshed.clone();
                        match entity.update(cx, |state, _| state.keiki_token = Some(refreshed)) {
                            Ok(()) => {}
                            Err(error) => {
                                tracing::warn!(%error, "Keiki token state update failed");
                            }
                        }
                        match client.list_agents(token.access_token()).await {
                            Ok(agents) => agents,
                            Err(error) => {
                                tracing::warn!(error = %error, "Keiki agent poll failed");
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "Keiki token refresh failed");
                        match entity.update(cx, |state, cx| {
                            state.keiki_status = SessionStatus::SignedOut;
                            cx.delete_credentials(CREDENTIAL_KEY)
                                .detach_and_log_err(&*cx);
                        }) {
                            Ok(()) => {}
                            Err(error) => {
                                tracing::warn!(%error, "Keiki sign-out state update failed");
                            }
                        }
                        return;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "Keiki agent poll failed");
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                continue;
            }
        };
        let conversations = match client.list_conversations(token.access_token()).await {
            Ok(conversations) => conversations,
            Err(error) => {
                tracing::warn!(error = %error, "Keiki conversation poll failed");
                Vec::new()
            }
        };
        let spaces = agents.iter().map(map_agent).collect();
        let chats = conversations.iter().filter_map(map_conversation).collect();
        match entity.update(cx, |state, cx| {
            state.devices.retain(|device| device.id != DEVICE_ID);
            state.spaces.retain(|space| !is_keiki_space(&space.id));
            state.chats.retain(|chat| !is_keiki_chat(&chat.id));
            state.apply_devices(vec![map_device()]);
            state.apply_spaces(spaces);
            state.apply_chats(chats);
            cx.notify();
        }) {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, "Keiki state update failed");
                return;
            }
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
                Some((state.keiki_client.clone()?, state.keiki_token.clone()?))
            })
            .ok()
            .flatten();
        let Some((client, token)) = context else {
            return;
        };
        match client.conversation(token.access_token(), &locator).await {
            Ok(detail) => {
                this.update(cx, |state, cx| {
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        state.apply_transcript(map_transcript(&detail));
                        cx.notify();
                    }
                })
                .ok();
            }
            Err(error) => tracing::warn!(%error, %chat_id, "Keiki transcript fetch failed"),
        }
    })
}

pub fn handle_callback(state: &mut AppState, callback: &str, cx: &mut Context<AppState>) {
    let Some(flow) = state.keiki_flow.take() else {
        return;
    };
    let Some(client) = state.keiki_client.clone() else {
        return;
    };
    let callback = callback.to_string();
    cx.spawn(async move |this, cx| {
        let result = async {
            let code = flow.authorization_code(&callback)?;
            let tokens = client.exchange_code(&flow, &code).await?;
            let credentials = flow.stored_credentials(&tokens);
            let payload =
                serde_json::to_vec(&credentials).map_err(|_| keiki_api::Error::InvalidContract)?;
            let write = this.update(cx, |_, cx| {
                cx.write_credentials(CREDENTIAL_KEY, "OAuth", &payload)
            });
            write
                .map_err(|_| keiki_api::Error::InvalidContract)?
                .await
                .map_err(|_| keiki_api::Error::InvalidContract)?;
            Ok::<_, keiki_api::Error>((tokens, credentials))
        }
        .await;
        let success = result.is_ok();
        this.update(cx, |state, cx| match result {
            Ok((tokens, credentials)) => {
                state.keiki_token = Some(tokens);
                state.keiki_credentials = Some(credentials);
                state.keiki_status = SessionStatus::SignedIn;
                cx.notify();
            }
            Err(error) => {
                state.keiki_status = SessionStatus::Error;
                tracing::warn!(error = %error, "Keiki sign-in failed");
                cx.notify();
            }
        })
        .ok();
        if success {
            poll(this.clone(), cx).await;
        }
    })
    .detach();
}

pub fn begin_sign_in(state: Entity<AppState>, cx: &mut Context<crate::shell::Shell>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let client = state.read_with(cx, |state, _| state.keiki_client.clone());
        let Some(client) = client else {
            return;
        };
        let result = async {
            client.discover_oauth().await?;
            let client_id = client.register_client(OAUTH_REDIRECT_URI).await?;
            let flow = AuthorizationFlow::new(client_id, OAUTH_REDIRECT_URI.to_string());
            let url = client.authorization_url(&flow)?;
            state.update(cx, |state, _| state.keiki_flow = Some(flow));
            Ok::<_, keiki_api::Error>(url)
        }
        .await;
        this.update(cx, |shell, cx| match result {
            Ok(url) => cx.open_url(url.as_str()),
            Err(error) => {
                shell.set_sidebar_notice(format!("Keiki sign in failed: {error}"));
                cx.notify();
            }
        })
        .ok();
    })
}

pub fn sign_out(state: Entity<AppState>, cx: &mut Context<crate::shell::Shell>) -> Task<()> {
    cx.spawn(async move |_, cx| {
        let credentials = state.read_with(cx, |state, _| {
            (state.keiki_client.clone(), state.keiki_token.clone())
        });
        if let (Some(client), Some(token)) = credentials {
            if let Err(error) = client.revoke_token(token.access_token()).await {
                tracing::warn!(%error, "Keiki token revocation failed");
            }
        }
        state.update(cx, |state, cx| {
            state.keiki_token = None;
            state.keiki_credentials = None;
            state.keiki_status = SessionStatus::SignedOut;
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
    fn parses_rfc3339_timestamps() {
        assert_eq!(
            parse_timestamp("2026-01-01T00:00:00+02:00")
                .unwrap()
                .timestamp(),
            1_767_218_400
        );
    }
}
