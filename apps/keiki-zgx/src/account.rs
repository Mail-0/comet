use anyhow::{Context, Result, ensure};
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

#[derive(Default)]
struct Account {
    token: Option<TokenSet>,
    user: Option<SessionUser>,
    agents: Vec<AgentSummary>,
    chat: Option<ConversationLocator>,
    reply: String,
}

impl Account {
    fn snapshot(&self, busy: bool, error: Option<String>) -> Value {
        json!({
            "signed_in": self.token.is_some(),
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
        let token = self.token.as_ref().context("Sign in to Keiki first")?;
        self.user = Some(client.session(token.access_token()).await?.user);
        state.send_replace(self.snapshot(true, None));
        self.agents = client.list_agents(token.access_token()).await?;
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
                ensure!(self.token.is_none(), "Already signed in");
                self.token = Some(sign_in(client).await?);
                self.load(client, state).await?;
            }
            Command::SignOut => {
                let token = std::mem::take(self).token;
                state.send_replace(self.snapshot(true, None));
                if let Some(token) = token {
                    client.revoke_token(token.access_token()).await?;
                }
            }
            Command::ChooseOrg(org_id) => {
                let token = self.token.as_ref().context("Sign in to Keiki first")?;
                let user = self.user.as_ref().context("No organization list loaded")?;
                ensure!(
                    user.orgs.iter().any(|org| org.id == org_id),
                    "Unknown organization"
                );
                self.agents.clear();
                self.chat = None;
                self.reply.clear();
                state.send_replace(self.snapshot(true, None));
                let result = client.switch_org(token.access_token(), &org_id).await;
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
                let token = self.token.as_ref().context("Sign in to Keiki first")?;
                let chat = self.chat.as_ref().context("Choose a Keiki agent first")?;
                let message = message.trim().to_owned();
                ensure!(!message.is_empty(), "Enter a message");
                let response = client
                    .steer_conversation_stream(token.access_token(), chat, message)
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
    while let Some(command) = commands.recv().await {
        state.send_replace(account.snapshot(true, None));
        let result = account.apply(command, &client, &state).await;
        if result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<keiki_api::Error>())
            .is_some_and(keiki_api::Error::is_authentication_failure)
        {
            account = Account::default();
            state.send_replace(account.snapshot(
                false,
                Some("Keiki rejected the session. Sign in again.".into()),
            ));
        } else {
            state
                .send_replace(account.snapshot(false, result.err().map(|error| error.to_string())));
        }
    }
}

async fn sign_in(client: &Client) -> Result<TokenSet> {
    const SCOPE: &str = "manage";
    let (listener, redirect_uri) = keiki_api::bind_loopback_listener().await?;
    client.discover_oauth(SCOPE).await?;
    let client_id = client.register_client(&redirect_uri).await?;
    let flow = AuthorizationFlow::new(client_id, redirect_uri.clone());
    let url = client.authorization_url(&flow, SCOPE)?;
    open::that_detached(url.as_str()).context("Could not open Keiki sign-in in the browser")?;
    let callback = keiki_api::wait_for_loopback_callback(listener, redirect_uri).await?;
    let code = flow.authorization_code(&callback)?;
    Ok(client.exchange_code(&flow, &code).await?)
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
            AgUiEvent::TextMessageChunk {
                message_id,
                delta: Some(delta),
            } => {
                let id = message_id
                    .or_else(|| self.current_id.clone())
                    .unwrap_or_default();
                self.append(id, &delta);
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

    #[tokio::test]
    async fn selected_keiki_agent_receives_an_authenticated_streaming_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(format!("http://{}", listener.local_addr().unwrap()));
        let server = async {
            for (expected, content_type, body) in [
                (
                    "POST /oauth/token HTTP/1.1",
                    "application/json",
                    r#"{"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600,"token_type":"bearer","scope":"manage"}"#,
                ),
                (
                    "GET /api/webapp/auth/session HTTP/1.1",
                    "application/json",
                    r#"{"user":{"orgs":[{"id":"org-1"}],"activeOrgId":"org-1"}}"#,
                ),
                (
                    "GET /api/webapp/agents HTTP/1.1",
                    "application/json",
                    r#"{"agents":[{"id":"agent-1","name":"Keiki","model":"auto","runtime":"cloud","active":true,"toolCount":0,"lastSeenAt":"","createdAt":""}]}"#,
                ),
                (
                    "/steer?agentId=agent-1 HTTP/1.1",
                    "text/event-stream",
                    concat!(
                        "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m1\",\"delta\":\"Keiki reply\"}\n\n",
                        "data: {\"type\":\"RUN_FINISHED\",\"runId\":\"r1\"}\n\n"
                    ),
                ),
            ] {
                let (conn, _) = listener.accept().await.unwrap();
                let mut io = BufReader::new(conn);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(io.read_line(&mut request).await.unwrap(), 0);
                }
                let request_line = request.lines().next().unwrap();
                assert!(request_line.ends_with(expected), "{request_line}");
                if expected.contains("steer?") {
                    assert!(
                        request_line.starts_with("POST /api/webapp/conversations/api:agent-1:")
                    );
                    let len = request
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    let mut body = vec![0; len];
                    io.read_exact(&mut body).await.unwrap();
                    assert_eq!(
                        serde_json::from_slice::<Value>(&body).unwrap(),
                        json!({"text": "hello", "stream": true})
                    );
                }
                if !expected.contains("/oauth/token") {
                    assert!(request.contains("\r\nauthorization: Bearer fixture-access\r\n"));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                io.write_all(response.as_bytes()).await.unwrap();
            }
        };
        let app = async {
            let flow = AuthorizationFlow::new("fixture-client".into(), "http://127.0.0.1:1".into());
            let mut account = Account {
                token: Some(client.exchange_code(&flow, "fixture-code").await.unwrap()),
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
