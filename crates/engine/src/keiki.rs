use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use keiki_api::redact_conversation_credentials as public_value;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_harness::{BoundConversation, BoundConversationSource};
use zeron_proto::{HarnessId, SessionStatus};
use zeron_rpc::{RpcError, RpcReply};

use crate::keiki_store::{BOUND_PREFIX, Binding, Owner, Store};
use crate::{Auth, CopilotCredentialHolder, DocHost, SessionsEngine, WorkspaceHost};

#[derive(Clone)]
pub(crate) struct KeikiConversations(Arc<Inner>);

struct Inner {
    auth: Auth,
    store: Store,
    workspace: WorkspaceHost,
    docs: DocHost,
    sessions: SessionsEngine,
}

pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        "ListKeikiAgents"
            | "ListKeikiAgentGroups"
            | "GetKeikiAgentGroupRoster"
            | "ListKeikiConversations"
            | "OpenKeikiConversation"
            | "NewKeikiConversation"
            | "LoadKeikiHistory"
            | "WatchKeikiSidebar"
            | "WatchKeikiConversation"
            | "SetKeikiTakeover"
    )
}

const SIDEBAR_POLL_SECONDS: u64 = 15;
const RETRY_SECONDS: u64 = 10;

fn transcript_poll_seconds(detail: &Value) -> u64 {
    if !detail["activeTurnStartedAt"].is_null()
        || !detail["liveness"].is_null()
        || detail["asks"]
            .as_array()
            .is_some_and(|asks| asks.iter().any(|ask| ask["status"] == "pending"))
    {
        3
    } else {
        10
    }
}

fn draft_detail(binding: &Binding) -> Value {
    json!({"phone":binding.conversation_id,"meta":{"agentId":binding.agent_id,"messageCount":0},"messages":[],"before":null,"draft":true,"pendingQuestions":[]})
}

#[cfg(test)]
#[test]
fn polling_matches_comet_idle_live_and_retry_intervals() {
    assert_eq!(SIDEBAR_POLL_SECONDS, 15);
    assert_eq!(RETRY_SECONDS, 10);
    assert_eq!(transcript_poll_seconds(&json!({})), 10);
    assert_eq!(transcript_poll_seconds(&json!({"liveness":"working"})), 3);
    assert_eq!(
        transcript_poll_seconds(&json!({"activeTurnStartedAt":"now"})),
        3
    );
    assert_eq!(
        transcript_poll_seconds(&json!({"asks":[{"status":"pending"}]})),
        3
    );
}

fn owner(auth: &Auth) -> Result<Owner, String> {
    let revision = auth.scope_revision();
    let state = auth.state();
    if revision != auth.scope_revision() {
        return Err("Keiki account or organization changed".into());
    }
    Ok(Owner {
        account: state.user().ok_or("Sign in to Keiki")?.id.clone(),
        org: state.org_id().ok_or("Choose a Keiki organization")?.into(),
        revision,
    })
}

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    value[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{key} is required")))
}
fn limit(params: &Value, max: u64) -> Result<u32, RpcError> {
    let limit = params
        .get("limit")
        .map_or(Some(50), Value::as_u64)
        .filter(|n| *n > 0 && *n <= max)
        .ok_or_else(|| RpcError::BadParams(format!("limit must be 1..{max}")))?;
    Ok(limit as u32)
}
fn offset(params: &Value, name: &str) -> Result<u32, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .and_then(|n| n.try_into().ok())
            .ok_or_else(|| RpcError::BadParams(format!("invalid {name}"))),
    }
}

impl KeikiConversations {
    pub fn new(
        auth: Auth,
        dir: &Path,
        workspace: WorkspaceHost,
        docs: DocHost,
        sessions: SessionsEngine,
        holder: &CopilotCredentialHolder,
    ) -> Self {
        let inner = Arc::new(Inner {
            auth,
            store: Store::new(dir),
            workspace,
            docs,
            sessions,
        });
        let source: Arc<dyn BoundConversationSource> = inner.clone();
        holder.set_bound_source(Arc::downgrade(&source));
        Self(inner)
    }

    pub fn authorize_chat(&self, chat: &str) -> Result<(), RpcError> {
        if chat.starts_with(BOUND_PREFIX) {
            let scope = owner(&self.0.auth).map_err(failed)?;
            self.0.store.get(&scope, chat).map_err(failed)?;
        }
        Ok(())
    }

    pub fn visible_chats(&self) -> watch::Receiver<Vec<zeron_proto::Chat>> {
        self.visible_rows(self.0.workspace.watch_chats(), |row| &row.id)
    }

    pub fn visible_sessions(
        &self,
        rows: watch::Receiver<Vec<zeron_proto::Session>>,
    ) -> watch::Receiver<Vec<zeron_proto::Session>> {
        self.visible_rows(rows, |row| &row.chat_id)
    }

    fn visible_rows<T: Clone + Send + Sync + 'static>(
        &self,
        mut rows: watch::Receiver<Vec<T>>,
        chat: fn(&T) -> &str,
    ) -> watch::Receiver<Vec<T>> {
        let mut auth = self.0.auth.watch_state();
        let this = self.clone();
        let filter = |rows: &Vec<T>| {
            rows.iter()
                .filter(|r| this.authorize_chat(chat(r)).is_ok())
                .cloned()
                .collect()
        };
        let (tx, rx) = watch::channel(filter(&rows.borrow_and_update()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    r = rows.changed() => if r.is_err() { break; },
                    r = auth.changed() => if r.is_err() { break; },
                }
                let filtered = rows
                    .borrow_and_update()
                    .iter()
                    .filter(|r| this.authorize_chat(chat(r)).is_ok())
                    .cloned()
                    .collect();
                tx.send_replace(filtered);
            }
        });
        rx
    }

    pub fn guard_stream(
        &self,
        chat: String,
        stream: BoxStream<'static, Value>,
    ) -> BoxStream<'static, Value> {
        if !chat.starts_with(BOUND_PREFIX) {
            return stream;
        }
        let this = self.clone();
        let expected = owner(&self.0.auth);
        let auth = self.0.auth.watch_scope();
        futures::stream::unfold(
            (this, chat, stream, auth, expected),
            |(this, chat, mut stream, mut auth, expected)| async move {
                loop {
                    this.0.check(expected.as_ref().ok()?).ok()?;
                    this.authorize_chat(&chat).ok()?;
                    tokio::select! {
                        biased;
                        changed = auth.changed() => if changed.is_err() { return None; },
                        item = stream.next() => {
                            this.authorize_chat(&chat).ok()?;
                            this.0.check(expected.as_ref().ok()?).ok()?;
                            return item.map(|item| (item, (this, chat, stream, auth, expected)));
                        }
                    }
                }
            },
        )
        .boxed()
    }

    pub async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        // Refresh/restoration precedes scope capture, never follows a client-supplied account.
        self.0.auth.access_token().await.map_err(failed)?;
        let scope = owner(&self.0.auth).map_err(failed)?;
        let client = keiki_api::Client::new(self.0.auth.api_url());
        let answer = match method {
            "ListKeikiAgents" => {
                self.0
                    .request(&scope, |token| {
                        Ok(client.list_agents_authenticated_request(token))
                    })
                    .await?
            }
            "ListKeikiAgentGroups" => {
                self.0
                    .request(&scope, |token| {
                        Ok(client.list_agent_groups_authenticated_request(token))
                    })
                    .await?
            }
            "GetKeikiAgentGroupRoster" => {
                let id = text(&params, "groupId")?;
                self.0
                    .request(&scope, |token| {
                        Ok(client.agent_group_roster_authenticated_request(token, id))
                    })
                    .await?
            }
            "ListKeikiConversations" => {
                let agent = text(&params, "agentId")?;
                let limit = limit(&params, 100)?;
                let offset = offset(&params, "offset")?;
                let mut page = self
                    .0
                    .request(&scope, |token| {
                        client
                            .list_agent_conversations_authenticated_request(
                                token, agent, limit, offset,
                            )
                            .map_err(failed)
                    })
                    .await?;
                let count = page["conversations"].as_array().map_or(0, Vec::len);
                page["nextOffset"] = if count == limit as usize {
                    json!(offset.saturating_add(limit))
                } else {
                    Value::Null
                };
                page
            }
            "OpenKeikiConversation" | "NewKeikiConversation" => {
                let agent = text(&params, "agentId")?;
                let draft = method == "NewKeikiConversation";
                let conversation = if draft {
                    format!("api:{agent}:{}", uuid::Uuid::new_v4())
                } else {
                    text(&params, "conversationId")?.to_owned()
                };
                let agents = self
                    .0
                    .request(&scope, |token| {
                        Ok(client.list_agents_authenticated_request(token))
                    })
                    .await?;
                if !agents["agents"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|a| a["id"] == agent))
                {
                    return Err(failed("Agent is not available in this organization"));
                }
                let detail = if draft {
                    None
                } else {
                    Some(self.0.page(&scope, agent, &conversation, 50, None).await?)
                };
                self.0.check(&scope)?;
                let binding = self
                    .0
                    .store
                    .bind(&scope, agent, &conversation, draft)
                    .map_err(failed)?;
                if self
                    .0
                    .workspace
                    .chat(&binding.chat_id)
                    .map_err(failed)?
                    .is_none()
                {
                    self.0
                        .workspace
                        .create_chat(
                            &binding.chat_id,
                            None,
                            Some(self.0.workspace.device_id()),
                            None,
                            None,
                        )
                        .map_err(failed)?;
                }
                self.0.workspace.set_chat_harness_session(
                    &binding.chat_id,
                    &binding.chat_id,
                    "",
                    Some(HarnessId::Copilot),
                );
                if let Some(detail) = detail {
                    self.0.import(&scope, &binding, &detail)?;
                }
                json!({"chatId": binding.chat_id, "binding": binding})
            }
            "LoadKeikiHistory" => {
                let binding = self.0.binding(&scope, text(&params, "chatId")?)?;
                if binding.draft {
                    limit(&params, 500)?;
                    return Ok(RpcReply::Value(draft_detail(&binding)));
                }
                let before = params
                    .get("before")
                    .filter(|v| !v.is_null())
                    .map(|_| offset(&params, "before"))
                    .transpose()?;
                let mut detail = self
                    .0
                    .page(
                        &scope,
                        &binding.agent_id,
                        &binding.conversation_id,
                        limit(&params, 500)?,
                        before,
                    )
                    .await?;
                self.0.import(&scope, &binding, &detail)?;
                detail["pendingQuestions"] = self
                    .0
                    .store
                    .questions(&scope, &binding.chat_id)
                    .map_err(failed)?;
                detail
            }
            "WatchKeikiSidebar" => return Ok(RpcReply::Stream(self.sidebar(scope, params))),
            "WatchKeikiConversation" => {
                let binding = self.0.binding(&scope, text(&params, "chatId")?)?;
                return Ok(RpcReply::Stream(self.conversation(scope, binding)));
            }
            "SetKeikiTakeover" => {
                let binding = self.0.binding(&scope, text(&params, "chatId")?)?;
                let locator = locator(&binding);
                let enabled = params["enabled"]
                    .as_bool()
                    .ok_or_else(|| RpcError::BadParams("enabled is required".into()))?;
                self.0
                    .request(&scope, |token| {
                        client
                            .conversation_action_authenticated_request(
                                if enabled {
                                    reqwest::Method::POST
                                } else {
                                    reqwest::Method::DELETE
                                },
                                token,
                                &locator,
                                keiki_api::ConversationAction::Takeover,
                            )
                            .map_err(failed)
                    })
                    .await?
            }
            _ => return Err(RpcError::UnknownMethod(method.into())),
        };
        self.0.check(&scope)?;
        Ok(RpcReply::Value(public_value(answer)))
    }

    fn sidebar(&self, scope: Owner, params: Value) -> BoxStream<'static, Value> {
        let this = self.clone();
        let auth = self.0.auth.watch_scope();
        futures::stream::unfold((this, scope, params, auth, false, false), |(this, scope, params, mut auth, wait, ended)| async move {
            if ended { return None; }
            if wait {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(SIDEBAR_POLL_SECONDS)) => {},
                    _ = auth.changed() => {},
                }
            }
            if this.0.check(&scope).is_err() {
                return Some((json!({"reset":true,"agents":[],"groups":[],"conversations":[]}), (this, scope, params, auth, false, true)));
            }
            let snapshot = tokio::select! {
                biased;
                _ = auth.changed() => Err(failed("Keiki account or organization changed")),
                snapshot = this.snapshot(&scope, &params) => snapshot,
            };
            let failed = snapshot.is_err();
            let value = snapshot.unwrap_or_else(|e| json!({"error":e.to_string()}));
            // Comet retries failed sidebar polls after ten seconds.
            if failed {
                tokio::select! { _ = tokio::time::sleep(Duration::from_secs(RETRY_SECONDS)) => {}, _ = auth.changed() => {} }
            }
            Some((value, (this, scope, params, auth, !failed, false)))
        }).boxed()
    }

    async fn snapshot(&self, scope: &Owner, params: &Value) -> Result<Value, RpcError> {
        let client = keiki_api::Client::new(self.0.auth.api_url());
        let agents = self
            .0
            .request(scope, |token| {
                Ok(client.list_agents_authenticated_request(token))
            })
            .await?;
        let groups = self
            .0
            .request(scope, |token| {
                Ok(client.list_agent_groups_authenticated_request(token))
            })
            .await?;
        let recent = self
            .0
            .request(scope, |token| {
                client
                    .list_conversations_authenticated_request(token, 20)
                    .map_err(failed)
            })
            .await?;
        let mut pages = Vec::new();
        let mut rosters = Vec::new();
        if let Some(expanded) = params["expandedGroups"].as_array() {
            if expanded.len() > 20 {
                return Err(RpcError::BadParams(
                    "Expand at most twenty groups per watch".into(),
                ));
            }
            for group in expanded {
                let id = group
                    .as_str()
                    .ok_or_else(|| RpcError::BadParams("Invalid expanded group".into()))?;
                let roster = self
                    .0
                    .request(scope, |token| {
                        Ok(client.agent_group_roster_authenticated_request(token, id))
                    })
                    .await?;
                rosters.push(json!({"groupId":id,"roster":roster}));
            }
        }
        if let Some(expanded) = params["expandedAgents"].as_array() {
            if expanded.len() > 20 {
                return Err(RpcError::BadParams(
                    "Expand at most twenty agents per watch".into(),
                ));
            }
            for agent in expanded {
                let id = agent
                    .as_str()
                    .ok_or_else(|| RpcError::BadParams("Invalid expanded agent".into()))?;
                let page = self
                    .0
                    .request(scope, |token| {
                        client
                            .list_agent_conversations_authenticated_request(token, id, 50, 0)
                            .map_err(failed)
                    })
                    .await?;
                pages.push(json!({"agentId":id,"page":page}));
            }
        }
        self.0.check(scope)?;
        Ok(public_value(
            json!({"agents":agents["agents"],"groups":groups["groups"],"conversations":recent["conversations"],"pages":pages,"rosters":rosters}),
        ))
    }

    fn conversation(&self, scope: Owner, binding: Binding) -> BoxStream<'static, Value> {
        let this = self.clone();
        let auth = self.0.auth.watch_scope();
        futures::stream::unfold((this, scope, binding, auth, 0u64), |(this, scope, binding, mut auth, seconds)| async move {
            if seconds > 0 { tokio::select! { _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}, _ = auth.changed() => {} } }
            if this.0.check(&scope).is_err() { return None; }
            let result = if this.0.binding(&scope, &binding.chat_id).ok()?.draft {
                Ok(draft_detail(&binding))
            } else {
                tokio::select! {
                    biased;
                    _ = auth.changed() => return None,
                    detail = this.0.page(&scope, &binding.agent_id, &binding.conversation_id, 100, None) => detail,
                }
            };
            let (value, seconds) = match result {
                Ok(mut detail) => {
                    detail["pendingQuestions"] = this.0.store.questions(&scope, &binding.chat_id).unwrap_or_else(|_| json!([]));
                    let seconds = transcript_poll_seconds(&detail);
                    let import = if detail["draft"] == true { Ok(()) } else { this.0.import(&scope, &binding, &detail) };
                    match import {
                        Ok(()) => (public_value(detail), seconds),
                        Err(e) => (json!({"error":e.to_string()}), 10),
                    }
                }
                Err(e) => (json!({"error":e.to_string()}), 10),
            };
            Some((value, (this, scope, binding, auth, seconds)))
        }).boxed()
    }
}

fn locator(binding: &Binding) -> keiki_api::ConversationLocator {
    keiki_api::ConversationLocator {
        identity: binding.conversation_id.clone(),
        agent_id: Some(binding.agent_id.clone()),
        api_key: None,
    }
}

impl Inner {
    fn check(&self, expected: &Owner) -> Result<(), RpcError> {
        if owner(&self.auth).as_ref() != Ok(expected) {
            return Err(failed("Keiki account or organization changed"));
        }
        Ok(())
    }

    fn binding(&self, scope: &Owner, chat: &str) -> Result<Binding, RpcError> {
        self.check(scope)?;
        self.store.get(scope, chat).map_err(failed)
    }

    async fn authenticated(&self, scope: &Owner) -> Result<String, RpcError> {
        self.check(scope)?;
        let token = self.auth.access_token().await.map_err(failed)?;
        self.check(scope)?;
        Ok(token)
    }

    async fn request(
        &self,
        scope: &Owner,
        build: impl FnOnce(&str) -> Result<reqwest::RequestBuilder, RpcError>,
    ) -> Result<Value, RpcError> {
        let token = self.authenticated(scope).await?;
        let response = build(&token)?.send().await.map_err(failed)?;
        self.check(scope)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.auth.reject_access_token(&token);
            return Err(failed("Keiki authorization expired"));
        }
        if !response.status().is_success() {
            return Err(failed(format!("Keiki returned {}", response.status())));
        }
        let value = response.json().await.map_err(failed)?;
        self.check(scope)?;
        Ok(value)
    }

    async fn page(
        &self,
        scope: &Owner,
        agent: &str,
        conversation: &str,
        limit: u32,
        before: Option<u32>,
    ) -> Result<Value, RpcError> {
        let client = keiki_api::Client::new(self.auth.api_url());
        let locator = keiki_api::ConversationLocator {
            identity: conversation.into(),
            agent_id: Some(agent.into()),
            api_key: None,
        };
        let start = before.map(|end| end.saturating_sub(limit)).unwrap_or(0);
        let mut detail = self
            .request(scope, |token| {
                client
                    .conversation_page_authenticated_request(token, &locator, limit, start)
                    .map_err(failed)
            })
            .await?;
        let total = detail["meta"]["messageCount"]
            .as_u64()
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;
        let start = if before.is_none() && total > limit {
            let start = total - limit;
            detail = self
                .request(scope, |token| {
                    client
                        .conversation_page_authenticated_request(token, &locator, limit, start)
                        .map_err(failed)
                })
                .await?;
            start
        } else {
            start
        };
        if detail["meta"]["agentId"]
            .as_str()
            .is_some_and(|id| id != agent)
        {
            return Err(failed("Conversation agent mismatch"));
        }
        detail["before"] = if start > 0 { json!(start) } else { Value::Null };
        if detail["phone"].as_str() != Some(conversation) {
            return Err(failed("Conversation identity mismatch"));
        }
        Ok(detail)
    }

    fn import(&self, scope: &Owner, binding: &Binding, detail: &Value) -> Result<(), RpcError> {
        self.check(scope)?;
        let messages = detail["messages"]
            .as_array()
            .ok_or_else(|| failed("Missing messages"))?;
        let complete = detail["meta"]["messageCount"].as_u64() == Some(messages.len() as u64)
            && detail["before"].is_null();
        self.store
            .cache(scope, &binding.chat_id, messages, complete)
            .map_err(failed)?;
        // A live turn owns the document until it settles. Polls may still update its cache.
        if self.active(&binding.chat_id) || self.docs.has_waiting_commands() {
            return Ok(());
        }
        self.replace_history(scope, binding)
    }

    fn replace_history(&self, scope: &Owner, binding: &Binding) -> Result<(), RpcError> {
        self.check(scope)?;
        let entries = self
            .store
            .history(scope, &binding.chat_id)
            .map_err(failed)?;
        let handle = self.docs.open(&binding.chat_id).map_err(failed)?;
        handle.import_transcript(|| -> Result<(), RpcError> {
            let list = handle.doc().doc().get_list("messages");
            list.delete(0, list.len()).map_err(failed)?;
            for message in entries {
                if !message["peerAskId"].is_null() {
                    continue;
                }
                let id = message["id"]
                    .as_str()
                    .ok_or_else(|| failed("Message has no id"))?;
                let mut content = message["content"].as_str().unwrap_or("").to_owned();
                if message["internal"] == true && !binding.conversation_id.starts_with("api:") {
                    content = format!("Internal turn\n\n{content}");
                }
                handle
                    .doc()
                    .push_message(&SessionMessageEntry {
                        id: format!("keiki:{id}"),
                        role: if message["direction"] == "inbound" {
                            MessageRole::User
                        } else {
                            MessageRole::Assistant
                        },
                        parts: vec![MessagePart::Text {
                            id: format!("keiki:{id}:text"),
                            text: content,
                        }],
                        created_at: message["createdAt"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map_or(0, |t| t.timestamp_millis()),
                        device_id: self.workspace.device_id().into(),
                        status: None,
                        continuation_of: None,
                        duration_ms: None,
                    })
                    .map_err(failed)?;
            }
            Ok(())
        })?;
        handle.publish_messages();
        Ok(())
    }

    fn active(&self, chat: &str) -> bool {
        self.sessions.session_status(chat).is_some_and(|s| {
            matches!(
                s.status,
                SessionStatus::Working | SessionStatus::AwaitingInput
            )
        })
    }
}

#[async_trait]
impl BoundConversationSource for Inner {
    async fn authorize(&self, session: &str) -> Result<BoundConversation, String> {
        let scope = owner(&self.auth)?;
        let token = self.auth.access_token().await.map_err(|e| e.to_string())?;
        self.check(&scope).map_err(|e| e.to_string())?;
        let binding = self.store.get(&scope, session)?;
        let cancelled = CancellationToken::new();
        let stop = cancelled.clone();
        let mut watch = self.auth.watch_scope();
        let auth = self.auth.clone();
        tokio::spawn(async move {
            loop {
                if owner(&auth).as_ref() != Ok(&scope) {
                    stop.cancel();
                    break;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    result = watch.changed() => {
                        if result.is_err() || owner(&auth).as_ref() != Ok(&scope) { stop.cancel(); break; }
                    }
                }
            }
        });
        Ok(BoundConversation {
            agent_id: binding.agent_id,
            conversation_id: binding.conversation_id,
            credentials: zeron_copilot::CopilotCredentials {
                base_url: self.auth.api_url().into(),
                access_token: token,
            },
            cancelled,
        })
    }

    async fn settled(&self, session: &str) -> Result<(), String> {
        let scope = owner(&self.auth)?;
        let binding = self.store.get(&scope, session)?;
        let detail = self
            .page(
                &scope,
                &binding.agent_id,
                &binding.conversation_id,
                100,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let messages = detail["messages"].as_array().ok_or("Missing messages")?;
        let complete = detail["meta"]["messageCount"].as_u64() == Some(messages.len() as u64)
            && detail["before"].is_null();
        self.store.cache(&scope, session, messages, complete)?;
        for _ in 0..100 {
            if !self.active(session) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.active(session) {
            return Ok(());
        }
        self.replace_history(&scope, &binding)
            .map_err(|e| e.to_string())
    }

    async fn unauthorized(&self, token: &str) {
        self.auth.reject_access_token(token);
    }

    async fn questions(&self, session: &str, questions: Value) -> Result<(), String> {
        let scope = owner(&self.auth)?;
        self.store
            .set_questions(&scope, session, &public_value(questions))
    }
}
