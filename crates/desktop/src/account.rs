mod credentials;

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use credentials::{CredentialStore, SecureStore};
use keiki_api::{AuthorizationFlow, Client, TokenSet};
use keiki_model::{AgentSummary, ConversationLocator, SessionUser, SteerConversationResponse};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use zeron_copilot::{AgUiEvent, SseDecoder, SseFrame};

pub enum Command {
    SignIn,
    SignOut,
    ChooseOrg(String),
    ChooseAgent(String),
    Send(String),
}

impl Command {
    pub fn sign_in(_: String) -> Self {
        Self::SignIn
    }

    pub fn sign_out(_: String) -> Self {
        Self::SignOut
    }
}

struct Session {
    client_id: String,
    token: TokenSet,
    store: Option<Arc<dyn CredentialStore>>,
}

#[derive(Debug)]
struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keiki rejected the session. Sign in again.")
    }
}

impl std::error::Error for SessionExpired {}

impl Session {
    async fn authorized<T, F>(
        &mut self,
        client: &Client,
        request: impl Fn(String) -> F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T, keiki_api::Error>>,
    {
        let mut refreshed = self.token.should_refresh();
        if refreshed {
            self.refresh(client).await?;
        }
        loop {
            match request(self.token.access_token().to_owned()).await {
                Err(keiki_api::Error::Api {
                    status: reqwest::StatusCode::UNAUTHORIZED,
                    ..
                }) => {
                    if refreshed {
                        return Err(SessionExpired.into());
                    }
                }
                result => return Ok(result?),
            }
            self.refresh(client).await?;
            refreshed = true;
        }
    }

    async fn refresh(&mut self, client: &Client) -> Result<()> {
        let credentials = self.token.stored_credentials(self.client_id.clone());
        match client.refresh_token(&credentials).await {
            Ok(token) => {
                self.token = token;
                self.persist().await?;
            }
            Err(error) if error.is_invalid_refresh_token() || error.is_authentication_failure() => {
                return Err(SessionExpired.into());
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn persist(&self) -> Result<()> {
        if let Some(store) = &self.store {
            store
                .save(&self.token.stored_credentials(self.client_id.clone()))
                .await?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Account {
    session: Option<Session>,
    user: Option<SessionUser>,
    agents: Vec<AgentSummary>,
    chat: Option<ConversationLocator>,
    reply: String,
}

impl Account {
    async fn restore(
        &mut self,
        client: &Client,
        state: &watch::Sender<Value>,
        store: Arc<dyn CredentialStore>,
    ) -> Result<()> {
        let Some(credentials) = store.load().await? else {
            return Ok(());
        };
        let token = match client.refresh_token(&credentials).await {
            Ok(token) => token,
            Err(error) if error.is_invalid_refresh_token() || error.is_authentication_failure() => {
                store.delete().await?;
                return Err(SessionExpired.into());
            }
            Err(error) => return Err(error.into()),
        };
        self.session = Some(Session {
            client_id: credentials.client_id,
            token,
            store: Some(store),
        });
        self.session
            .as_ref()
            .context("Missing restored session")?
            .persist()
            .await?;
        self.load(client, state).await
    }

    fn snapshot(&self, busy: bool, error: Option<String>) -> Value {
        json!({
            "signed_in": self.session.is_some(),
            "orgs": self.user.as_ref().map(|user| &user.orgs),
            "active_org": self.user.as_ref().and_then(|user| user.active_org_id.as_ref()),
            "agents": self.agents.iter().map(|agent| json!({"id": agent.id, "name": agent.name})).collect::<Vec<_>>(),
            "agent_id": self.chat.as_ref().and_then(|chat| chat.agent_id.as_ref()),
            "reply": self.reply,
            "busy": busy,
            "error": error,
        })
    }

    async fn load(&mut self, client: &Client, state: &watch::Sender<Value>) -> Result<()> {
        let session = self.session.as_mut().context("Sign in to Keiki first")?;
        self.user = Some(
            session
                .authorized(client, |token| async move { client.session(&token).await })
                .await?
                .user,
        );
        state.send_replace(self.snapshot(true, None));
        self.agents = self
            .session
            .as_mut()
            .context("Sign in to Keiki first")?
            .authorized(
                client,
                |token| async move { client.list_agents(&token).await },
            )
            .await?;
        keiki_model::sort_agents(&mut self.agents);
        Ok(())
    }

    async fn apply(
        &mut self,
        command: Command,
        client: &Client,
        state: &watch::Sender<Value>,
    ) -> Result<()> {
        match command {
            Command::SignIn => {
                ensure!(self.session.is_none(), "Already signed in");
                self.session = Some(sign_in(client).await?);
                self.session
                    .as_ref()
                    .context("Missing session")?
                    .persist()
                    .await?;
                self.load(client, state).await?;
            }
            Command::SignOut => {
                let session = std::mem::take(self).session;
                state.send_replace(self.snapshot(true, None));
                if let Some(session) = session {
                    if let Some(store) = &session.store {
                        store.delete().await?;
                    }
                    client.revoke_token(session.token.access_token()).await?;
                } else {
                    SecureStore.delete().await?;
                }
            }
            Command::ChooseOrg(org_id) => {
                let user = self.user.as_ref().context("No organization list loaded")?;
                ensure!(
                    user.orgs.iter().any(|org| org.id == org_id),
                    "Unknown organization"
                );
                self.agents.clear();
                self.chat = None;
                self.reply.clear();
                state.send_replace(self.snapshot(true, None));
                let result = self
                    .session
                    .as_mut()
                    .context("Sign in to Keiki first")?
                    .authorized(client, |token| {
                        let org = &org_id;
                        async move { client.switch_org(&token, org).await }
                    })
                    .await;
                if result.as_ref().is_err_and(|err| err.is::<SessionExpired>()) {
                    return result.map(|_| ());
                }
                // Reload the actual session even if the remote switch's result was lost.
                self.user = None;
                self.load(client, state).await?;
                let switched = result?;
                ensure!(
                    switched.ok && switched.active_org_id == org_id,
                    "Keiki did not confirm the organization switch"
                );
            }
            Command::ChooseAgent(agent_id) => {
                ensure!(
                    self.agents.iter().any(|agent| agent.id == agent_id),
                    "Unknown Keiki agent"
                );
                if self.chat.as_ref().and_then(|chat| chat.agent_id.as_ref()) != Some(&agent_id) {
                    self.chat = Some(ConversationLocator {
                        identity: format!("api:{agent_id}:{}", uuid::Uuid::new_v4().simple()),
                        agent_id: Some(agent_id),
                        api_key: None,
                    });
                    self.reply.clear();
                }
            }
            Command::Send(message) => {
                let session = self.session.as_mut().context("Sign in to Keiki first")?;
                let chat = self.chat.as_ref().context("Choose a Keiki agent first")?;
                let message = message.trim().to_owned();
                ensure!(!message.is_empty(), "Enter a message");
                let response = session
                    .authorized(client, |token| {
                        let message = message.clone();
                        async move {
                            client
                                .steer_conversation_stream(&token, chat, message)
                                .await
                        }
                    })
                    .await?;
                self.reply.clear();
                stream_reply(response, |reply| {
                    self.reply = reply;
                    state.send_replace(self.snapshot(true, None));
                })
                .await?;
            }
        }
        Ok(())
    }
}

pub fn register(registration: &crate::bridge::Registration<'_>) {
    registration.watch("keiki.account", registration.account.clone());
    registration.account_action("account.sign_in", Command::sign_in);
    registration.account_action("account.sign_out", Command::sign_out);
    registration.account_action("account.choose_org", Command::ChooseOrg);
}

pub fn initial() -> Value {
    Account::default().snapshot(false, None)
}

pub async fn run(
    api_url: String,
    mut commands: mpsc::Receiver<Command>,
    state: watch::Sender<Value>,
) {
    let client = Client::new(api_url);
    let mut account = Account::default();
    state.send_replace(account.snapshot(true, None));
    let restored = account
        .restore(&client, &state, Arc::new(SecureStore))
        .await;
    if restored
        .as_ref()
        .is_err_and(|error| error.is::<SessionExpired>())
    {
        if let Some(store) = account
            .session
            .as_ref()
            .and_then(|session| session.store.as_ref())
            && let Err(error) = store.delete().await
        {
            tracing::warn!(%error, "Could not clear rejected Keiki credentials");
        }
        account = Account::default();
    }
    state.send_replace(account.snapshot(false, restored.err().map(|error| error.to_string())));
    while let Some(command) = commands.recv().await {
        state.send_replace(account.snapshot(true, None));
        let result = account.apply(command, &client, &state).await;
        if result
            .as_ref()
            .is_err_and(|error| error.is::<SessionExpired>())
        {
            if let Some(store) = account
                .session
                .as_ref()
                .and_then(|session| session.store.as_ref())
                && let Err(error) = store.delete().await
            {
                tracing::warn!(%error, "Could not clear rejected Keiki credentials");
            }
            account = Account::default();
        }
        state.send_replace(account.snapshot(false, result.err().map(|error| error.to_string())));
    }
}

async fn sign_in(client: &Client) -> Result<Session> {
    const SCOPE: &str = "manage";
    let (listener, redirect_uri) = keiki_api::bind_loopback_listener().await?;
    client.discover_oauth(SCOPE).await?;
    let client_id = client.register_client(&redirect_uri).await?;
    let flow = AuthorizationFlow::new(client_id.clone(), redirect_uri.clone());
    let url = client.authorization_url(&flow, SCOPE)?;
    open::that_detached(url.as_str()).context("Could not open Keiki sign-in in the browser")?;
    let callback = keiki_api::wait_for_loopback_callback(listener, redirect_uri).await?;
    let code = flow.authorization_code(&callback)?;
    let token = client.exchange_code(&flow, &code).await?;
    Ok(Session {
        client_id,
        token,
        store: Some(Arc::new(SecureStore)),
    })
}

async fn stream_reply(
    mut response: reqwest::Response,
    mut publish: impl FnMut(String),
) -> Result<()> {
    let streaming = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !streaming {
        publish(response.json::<SteerConversationResponse>().await?.reply);
        return Ok(());
    }
    let mut decoder = SseDecoder::new();
    let mut reply = Reply::default();
    while let Some(chunk) = response.chunk().await? {
        for frame in decoder.push(&chunk)? {
            reply.apply(frame, &mut publish)?;
        }
    }
    if let Some(frame) = decoder.finish()? {
        reply.apply(frame, &mut publish)?;
    }
    ensure!(reply.finished, "Keiki stream ended before the run finished");
    Ok(())
}

#[derive(Default)]
struct Reply {
    parts: Vec<(String, String)>,
    current_id: Option<String>,
    finished: bool,
}

impl Reply {
    fn apply(&mut self, frame: SseFrame, publish: &mut impl FnMut(String)) -> Result<()> {
        if frame.data.is_empty() {
            return Ok(());
        }
        match frame.ag_ui_event()? {
            AgUiEvent::TextMessageStart { message_id } => self.current_id = Some(message_id),
            AgUiEvent::TextMessageContent { message_id, delta } => self.append(message_id, &delta),
            AgUiEvent::TextMessageChunk { message_id, delta } => {
                if let Some(id) = message_id {
                    self.current_id = Some(id);
                }
                if let Some(delta) = delta {
                    self.append(self.current_id.clone().unwrap_or_default(), &delta);
                }
            }
            AgUiEvent::TextMessageDiscarded { message_id } => {
                self.parts.retain(|(id, _)| *id != message_id)
            }
            AgUiEvent::RunFinished { .. } => self.finished = true,
            AgUiEvent::RunError { message } => anyhow::bail!("{message}"),
            _ => return Ok(()),
        }
        publish(
            self.parts
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        Ok(())
    }

    fn append(&mut self, id: String, delta: &str) {
        if let Some((_, text)) = self.parts.iter_mut().find(|(key, _)| *key == id) {
            text.push_str(delta);
        } else {
            self.parts.push((id, delta.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        time::{Duration, timeout},
    };

    #[derive(Default)]
    struct MemoryStore {
        credentials: std::sync::Mutex<Option<keiki_api::StoredCredentials>>,
        fail_load: bool,
    }

    #[async_trait::async_trait]
    impl CredentialStore for MemoryStore {
        async fn load(&self) -> Result<Option<keiki_api::StoredCredentials>> {
            ensure!(!self.fail_load, "credential storage unavailable");
            Ok(self.credentials.lock().unwrap().clone())
        }

        async fn save(&self, credentials: &keiki_api::StoredCredentials) -> Result<()> {
            *self.credentials.lock().unwrap() = Some(credentials.clone());
            Ok(())
        }

        async fn delete(&self) -> Result<()> {
            *self.credentials.lock().unwrap() = None;
            Ok(())
        }
    }

    #[tokio::test]
    async fn missing_or_unavailable_credentials_do_not_create_a_session() {
        let mut account = Account::default();
        let client = Client::new("http://127.0.0.1:1");
        let (state, _) = watch::channel(initial());
        account
            .restore(&client, &state, Arc::new(MemoryStore::default()))
            .await
            .unwrap();
        assert!(account.session.is_none());
        let error = account
            .restore(
                &client,
                &state,
                Arc::new(MemoryStore {
                    fail_load: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("storage unavailable"));
        assert!(account.session.is_none());
    }

    #[tokio::test]
    async fn restored_credentials_rotate_and_sign_out_deletes_them() {
        restore_credentials(false).await;
    }

    #[tokio::test]
    async fn rejected_restored_credentials_are_deleted() {
        restore_credentials(true).await;
    }

    async fn restore_credentials(reject: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(format!("http://{}", listener.local_addr().unwrap()));
        let store = Arc::new(MemoryStore::default());
        store
            .save(&keiki_api::StoredCredentials {
                client_id: "fixture-client".into(),
                refresh_token: "fixture-refresh".into(),
            })
            .await
            .unwrap();
        let server = async {
            let responses = if reject {
                vec![(
                    "POST /oauth/token",
                    "400 Bad Request",
                    r#"{"error":"invalid_grant"}"#,
                )]
            } else {
                vec![
                    (
                        "POST /oauth/token",
                        "200 OK",
                        r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"bearer","scope":"manage"}"#,
                    ),
                    (
                        "GET /api/webapp/auth/session",
                        "200 OK",
                        r#"{"user":{"orgs":[{"id":"org-1"}],"activeOrgId":"org-1"}}"#,
                    ),
                    ("GET /api/webapp/agents", "200 OK", r#"{"agents":[]}"#),
                    ("POST /oauth/revoke", "200 OK", "{}"),
                ]
            };
            for (expected, status, body) in responses {
                let (connection, _) = listener.accept().await.unwrap();
                let mut io = BufReader::new(connection);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(io.read_line(&mut request).await.unwrap(), 0);
                }
                assert!(request.starts_with(expected), "{request}");
                let length: usize = request
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .parse()
                    .unwrap();
                let mut payload = vec![0; length];
                io.read_exact(&mut payload).await.unwrap();
                if expected == "POST /oauth/token" {
                    assert_eq!(payload, b"grant_type=refresh_token&refresh_token=fixture-refresh&client_id=fixture-client");
                }
                io.write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
        };
        let app = async {
            let mut account = Account::default();
            let (state, _) = watch::channel(initial());
            let result = account.restore(&client, &state, store.clone()).await;
            if reject {
                assert!(result.unwrap_err().is::<SessionExpired>());
                assert!(account.session.is_none());
            } else {
                result.unwrap();
                assert!(
                    account.snapshot(false, None)["signed_in"]
                        .as_bool()
                        .unwrap()
                );
                assert_eq!(
                    store.load().await.unwrap().unwrap().refresh_token,
                    "rotated-refresh"
                );
                account
                    .apply(Command::SignOut, &client, &state)
                    .await
                    .unwrap();
                assert!(account.session.is_none());
            }
            assert!(store.load().await.unwrap().is_none());
        };
        timeout(Duration::from_secs(5), async { tokio::join!(server, app) })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn streaming_follow_up_keeps_its_conversation_when_keiki_rejects_an_expired_token() {
        streaming_follow_up_after_refresh(false).await;
    }

    #[tokio::test]
    async fn rejected_refreshed_token_requires_signing_in_again() {
        streaming_follow_up_after_refresh(true).await;
    }

    async fn streaming_follow_up_after_refresh(reject_refresh: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(format!("http://{}", listener.local_addr().unwrap()));
        const STEER: &str = "/steer?agentId=agent-1 HTTP/1.1";
        const JSON: &str = "application/json";
        const STREAM: &str = "text/event-stream";
        const DELTAS: &str = concat!(
            "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m1\",\"delta\":\"Keiki reply\"}\n\n",
            "data: {\"type\":\"RUN_FINISHED\",\"runId\":\"r1\"}\n\n"
        );
        let server = async {
            let mut conversation = None;
            for (step, (expected, status, content_type, body)) in [
                (
                    "POST /oauth/token HTTP/1.1",
                    "200 OK",
                    JSON,
                    r#"{"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600,"token_type":"bearer","scope":"manage"}"#,
                ),
                (
                    "GET /api/webapp/auth/session HTTP/1.1",
                    "200 OK",
                    JSON,
                    r#"{"user":{"orgs":[{"id":"org-1"}],"activeOrgId":"org-1"}}"#,
                ),
                (
                    "GET /api/webapp/agents HTTP/1.1",
                    "200 OK",
                    JSON,
                    r#"{"agents":[{"id":"agent-1","name":"Keiki","model":"auto","runtime":"cloud","active":true,"toolCount":0,"lastSeenAt":"","createdAt":""}]}"#,
                ),
                (STEER, "200 OK", STREAM, DELTAS),
                (STEER, "401 Unauthorized", JSON, r#"{"error":"expired"}"#),
                (
                    "POST /oauth/token HTTP/1.1",
                    "200 OK",
                    JSON,
                    r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"bearer","scope":"manage"}"#,
                ),
                if reject_refresh {
                    (STEER, "401 Unauthorized", JSON, r#"{"error":"invalid_token"}"#)
                } else {
                    (STEER, "200 OK", STREAM, DELTAS)
                },
            ].into_iter().enumerate() {
                let (conn, _) = listener.accept().await.unwrap();
                let mut io = BufReader::new(conn);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(io.read_line(&mut request).await.unwrap(), 0);
                }
                let request_line = request.lines().next().unwrap();
                assert!(request_line.ends_with(expected), "{request_line}");
                if expected == STEER {
                    assert!(
                        request_line.starts_with("POST /api/webapp/conversations/api:agent-1:")
                    );
                    assert_eq!(conversation.get_or_insert_with(|| request_line.to_owned()), request_line);
                }
                let len = request
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .parse()
                    .unwrap();
                let mut payload = vec![0; len];
                io.read_exact(&mut payload).await.unwrap();
                if expected == STEER {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&payload).unwrap(),
                        json!({"text": if step == 3 { "hello" } else { "follow-up" }, "stream": true})
                    );
                }
                if step == 5 {
                    assert_eq!(payload, b"grant_type=refresh_token&refresh_token=fixture-refresh&client_id=fixture-client");
                }
                if !expected.contains("/oauth/token") {
                    let access = if step == 6 { "rotated-access" } else { "fixture-access" };
                    assert!(request.contains(&format!("\r\nauthorization: Bearer {access}\r\n")));
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                io.write_all(response.as_bytes()).await.unwrap();
            }
        };
        let app = async {
            let flow = AuthorizationFlow::new("fixture-client".into(), "http://127.0.0.1:1".into());
            let mut account = Account {
                session: Some(Session {
                    client_id: "fixture-client".into(),
                    token: client.exchange_code(&flow, "fixture-code").await.unwrap(),
                    store: None,
                }),
                ..Default::default()
            };
            let (state, _) = watch::channel(initial());
            account.load(&client, &state).await.unwrap();
            account
                .apply(Command::ChooseAgent("agent-1".into()), &client, &state)
                .await
                .unwrap();
            account
                .apply(Command::Send("hello".into()), &client, &state)
                .await
                .unwrap();
            assert_eq!(account.reply, "Keiki reply");
            let result = account
                .apply(Command::Send("follow-up".into()), &client, &state)
                .await;
            if reject_refresh {
                assert!(result.unwrap_err().is::<SessionExpired>());
            } else {
                result.unwrap();
            }
            let session = account.session.unwrap();
            assert_eq!(
                session
                    .token
                    .stored_credentials(session.client_id)
                    .refresh_token,
                "rotated-refresh"
            );
            assert_eq!(account.reply, "Keiki reply");
        };
        timeout(Duration::from_secs(5), async { tokio::join!(server, app) })
            .await
            .unwrap();
    }

    #[test]
    fn reply_streams_partial_text_and_removes_discarded_text() {
        let mut reply = Reply::default();
        let mut output = Vec::new();
        for data in [
            json!({"type": "TEXT_MESSAGE_START", "messageId": "1"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "draft"}),
            json!({"type": "TEXT_MESSAGE_DISCARDED", "messageId": "1"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "2", "delta": "Keiki "}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "2", "delta": "reply"}),
            json!({"type": "RUN_FINISHED", "runId": "run-1"}),
        ] {
            reply
                .apply(
                    SseFrame {
                        event: None,
                        id: None,
                        data: data.to_string(),
                    },
                    &mut |part| output.push(part),
                )
                .unwrap();
        }
        assert!(output.contains(&"draft".into()));
        assert!(output.contains(&"Keiki ".into()));
        assert_eq!(output.last().unwrap(), "Keiki reply");
        assert!(reply.finished);
    }

    #[test]
    fn chunk_only_messages_keep_their_identity_for_continuation_and_discard() {
        let mut reply = Reply::default();
        let mut text = String::new();
        for (data, expected) in [
            (
                json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "first", "delta": "Keep"}),
                "Keep",
            ),
            (
                json!({"type": "TEXT_MESSAGE_CHUNK", "delta": " this"}),
                "Keep this",
            ),
            (
                json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "second"}),
                "Keep this",
            ),
            (
                json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "Discard"}),
                "Keep this\n\nDiscard",
            ),
            (
                json!({"type": "TEXT_MESSAGE_DISCARDED", "messageId": "second"}),
                "Keep this",
            ),
        ] {
            reply
                .apply(
                    SseFrame {
                        event: None,
                        id: None,
                        data: data.to_string(),
                    },
                    &mut |value| text = value,
                )
                .unwrap();
            assert_eq!(text, expected);
        }
    }

    #[tokio::test]
    async fn only_an_agent_from_the_keiki_account_can_be_selected() {
        let mut account = Account::default();
        let (state, _) = watch::channel(initial());
        assert!(
            account
                .apply(
                    Command::ChooseAgent("unlisted".into()),
                    &Client::new("https://onkeiki.com"),
                    &state
                )
                .await
                .is_err()
        );
        assert!(account.chat.is_none());
    }
}
