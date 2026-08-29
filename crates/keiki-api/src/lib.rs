use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
pub use keiki_model::{
    AgentConfig, AgentInput, AgentSummary, AgentTemplateSummary, BlockConversationResponse,
    ClearConversationResponse, ConversationDetail, ConversationLocator, ConversationSearchHit,
    ConversationSummary, ConversationTakeover, CreateAgentFromTemplate, CreateAgentResponse,
    SendConversationMessageResponse, SteerConversationResponse, TakeoverResponse,
};
use keiki_model::{
    AgentEditResponse, AgentTemplatesResponse, AgentsResponse, ConversationTextInput,
    ConversationsResponse,
};
use rand::Rng as _;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use url::{Url, form_urlencoded};

pub const OAUTH_REDIRECT_URI: &str = "keiki://oauth/callback";
const LOOPBACK_CALLBACK_PATH: &str = "/oauth/callback";
const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONVERSATION_MESSAGE_PAGE_LIMIT: u32 = 500;

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Clone)]
pub struct AuthorizationFlow {
    client_id: String,
    redirect_uri: String,
    code_verifier: String,
    code_challenge: String,
    state: String,
}

#[derive(Clone)]
pub struct TokenSet {
    access_token: String,
    refresh_token: String,
    expires_at: Instant,
    scope: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub client_id: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationAction {
    Block,
    History,
    Takeover,
    Messages,
    Steer,
}

impl ConversationAction {
    fn path(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::History => "history",
            Self::Takeover => "takeover",
            Self::Messages => "messages",
            Self::Steer => "steer",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the OAuth callback was invalid")]
    InvalidCallback,
    #[error("authorization was rejected: {0}")]
    AuthorizationRejected(String),
    #[error("the server returned an invalid OAuth contract")]
    InvalidContract,
    #[error("Keiki OAuth contract mismatch at {endpoint}: {detail}")]
    OAuthContract {
        endpoint: &'static str,
        detail: String,
    },
    #[error("Keiki request task failed: {0}")]
    TaskFailed(String),
    #[error("Keiki local operation failed: {0}")]
    Local(String),
    #[error("invalid Keiki response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Keiki returned {status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl Error {
    pub fn is_invalid_refresh_token(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: StatusCode::BAD_REQUEST,
                ..
            }
        )
    }

    pub fn is_authentication_failure(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
                ..
            }
        )
    }
}

pub async fn bind_loopback_listener() -> Result<(TcpListener, String), Error> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| Error::Local(format!("bind Keiki OAuth loopback listener: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| Error::Local(format!("read Keiki OAuth loopback address: {error}")))?
        .port();
    Ok((listener, loopback_redirect_uri(port)))
}

pub async fn wait_for_loopback_callback(
    listener: TcpListener,
    redirect_uri: String,
) -> Result<String, Error> {
    tokio::time::timeout(LOOPBACK_TIMEOUT, async move {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|error| {
                Error::Local(format!("accept Keiki OAuth loopback connection: {error}"))
            })?;
            let request_line = {
                let mut reader = BufReader::new(&mut stream);
                let mut request_line = Vec::new();
                let bytes = reader.read_until(b'\n', &mut request_line).await.map_err(|error| {
                    Error::Local(format!("read Keiki OAuth loopback request: {error}"))
                })?;
                if bytes > 8 * 1024 {
                    return Err(Error::Local(
                        "Keiki OAuth loopback request line is too long".to_string(),
                    ));
                }
                String::from_utf8(request_line).map_err(|error| {
                    Error::Local(format!("decode Keiki OAuth loopback request: {error}"))
                })?
            };
            let Some(callback) = callback_url_from_request_line(&request_line, &redirect_uri)?
            else {
                continue;
            };
            let body = "You can close this tab and return to Keiki";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.map_err(|error| {
                Error::Local(format!("write Keiki OAuth loopback response: {error}"))
            })?;
            return Ok(callback);
        }
    })
    .await
    .map_err(|_| Error::Local("timed out waiting for the Keiki OAuth callback".to_string()))?
}

fn callback_url_from_request_line(
    request_line: &str,
    redirect_uri: &str,
) -> Result<Option<String>, Error> {
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or_else(|| {
        Error::Local("Keiki OAuth request line is missing its method".to_string())
    })?;
    let target = parts.next().ok_or_else(|| {
        Error::Local("Keiki OAuth request line is missing its target".to_string())
    })?;
    if method != "GET" {
        return Ok(None);
    }
    let target_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| Error::Local(format!("parse Keiki OAuth request target: {error}")))?;
    if target_url.path() != LOOPBACK_CALLBACK_PATH {
        return Ok(None);
    }
    let Some(query) = target_url.query() else {
        return Ok(None);
    };
    let has_callback_parameter =
        form_urlencoded::parse(query.as_bytes()).any(|(name, _)| name == "code" || name == "error");
    if !has_callback_parameter {
        return Ok(None);
    }
    Ok(Some(format!("{redirect_uri}?{query}")))
}

fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{LOOPBACK_CALLBACK_PATH}")
}

impl Client {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn discover_oauth(&self) -> Result<(), Error> {
        const AUTHORIZATION_SERVER_METADATA: &str = "/.well-known/oauth-authorization-server";
        const PROTECTED_RESOURCE_METADATA: &str =
            "/.well-known/oauth-protected-resource/api/webapp";
        let metadata: AuthorizationServerMetadata = self
            .send_contract_json(
                self.http.get(self.endpoint(AUTHORIZATION_SERVER_METADATA)),
                AUTHORIZATION_SERVER_METADATA,
            )
            .await?;
        if metadata.issuer != self.base_url {
            return Err(Error::OAuthContract {
                endpoint: AUTHORIZATION_SERVER_METADATA,
                detail: format!(
                    "issuer was {:?}, expected {:?}",
                    metadata.issuer, self.base_url
                ),
            });
        }
        for (field, actual, expected) in [
            (
                "authorization_endpoint",
                metadata.authorization_endpoint.as_str(),
                self.endpoint("/oauth/authorize"),
            ),
            (
                "token_endpoint",
                metadata.token_endpoint.as_str(),
                self.endpoint("/oauth/token"),
            ),
            (
                "registration_endpoint",
                metadata.registration_endpoint.as_str(),
                self.endpoint("/oauth/register"),
            ),
            (
                "revocation_endpoint",
                metadata.revocation_endpoint.as_str(),
                self.endpoint("/oauth/revoke"),
            ),
        ] {
            if actual != expected {
                return Err(Error::OAuthContract {
                    endpoint: AUTHORIZATION_SERVER_METADATA,
                    detail: format!("{field} was {actual:?}, expected {expected:?}"),
                });
            }
        }
        if !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(Error::OAuthContract {
                endpoint: AUTHORIZATION_SERVER_METADATA,
                detail: format!(
                    "code_challenge_methods_supported was {:?}, expected S256",
                    metadata.code_challenge_methods_supported
                ),
            });
        }
        if !metadata
            .scopes_supported
            .iter()
            .any(|scope| scope == "manage")
        {
            return Err(Error::OAuthContract {
                endpoint: AUTHORIZATION_SERVER_METADATA,
                detail: format!(
                    "scopes_supported was {:?}, expected manage",
                    metadata.scopes_supported
                ),
            });
        }

        let resource: ProtectedResourceMetadata = self
            .send_contract_json(
                self.http.get(self.endpoint(PROTECTED_RESOURCE_METADATA)),
                PROTECTED_RESOURCE_METADATA,
            )
            .await?;
        let expected_resource = self.endpoint("/api/webapp");
        if resource.resource != expected_resource {
            return Err(Error::OAuthContract {
                endpoint: PROTECTED_RESOURCE_METADATA,
                detail: format!(
                    "resource was {:?}, expected {:?}",
                    resource.resource, expected_resource
                ),
            });
        }
        if !resource
            .authorization_servers
            .iter()
            .any(|server| server == &self.base_url)
        {
            return Err(Error::OAuthContract {
                endpoint: PROTECTED_RESOURCE_METADATA,
                detail: format!(
                    "authorization_servers was {:?}, expected {}",
                    resource.authorization_servers, self.base_url
                ),
            });
        }
        if !resource
            .scopes_supported
            .iter()
            .any(|scope| scope == "manage")
        {
            return Err(Error::OAuthContract {
                endpoint: PROTECTED_RESOURCE_METADATA,
                detail: format!(
                    "scopes_supported was {:?}, expected manage",
                    resource.scopes_supported
                ),
            });
        }
        if !resource
            .bearer_methods_supported
            .iter()
            .any(|method| method == "header")
        {
            return Err(Error::OAuthContract {
                endpoint: PROTECTED_RESOURCE_METADATA,
                detail: format!(
                    "bearer_methods_supported was {:?}, expected header",
                    resource.bearer_methods_supported
                ),
            });
        }

        Ok(())
    }

    pub async fn register_client(&self, redirect_uri: &str) -> Result<String, Error> {
        const REGISTRATION_ENDPOINT: &str = "/oauth/register";
        let response: RegistrationResponse = self
            .send_contract_json(
                self.http
                    .post(self.endpoint(REGISTRATION_ENDPOINT))
                    .json(&RegistrationRequest {
                        client_name: "Keiki Desktop",
                        redirect_uris: [redirect_uri],
                    }),
                REGISTRATION_ENDPOINT,
            )
            .await?;
        if response.client_id.is_empty() {
            return Err(Error::OAuthContract {
                endpoint: REGISTRATION_ENDPOINT,
                detail: "client_id was empty".into(),
            });
        }
        if response.token_endpoint_auth_method != "none" {
            return Err(Error::OAuthContract {
                endpoint: REGISTRATION_ENDPOINT,
                detail: format!(
                    "token_endpoint_auth_method was {:?}, expected none",
                    response.token_endpoint_auth_method
                ),
            });
        }
        if !response
            .redirect_uris
            .iter()
            .any(|registered| registered == redirect_uri)
        {
            return Err(Error::OAuthContract {
                endpoint: REGISTRATION_ENDPOINT,
                detail: format!(
                    "redirect_uris was {:?}, expected {:?}",
                    response.redirect_uris, redirect_uri
                ),
            });
        }
        Ok(response.client_id)
    }

    pub fn authorization_url(&self, flow: &AuthorizationFlow) -> Result<Url, Error> {
        let mut url = Url::parse(&self.endpoint("/oauth/authorize"))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &flow.client_id)
            .append_pair("redirect_uri", &flow.redirect_uri)
            .append_pair("scope", "manage")
            .append_pair("resource", &self.endpoint("/api/webapp"))
            .append_pair("code_challenge", &flow.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &flow.state);
        Ok(url)
    }

    pub async fn exchange_code(
        &self,
        flow: &AuthorizationFlow,
        code: &str,
    ) -> Result<TokenSet, Error> {
        let response: TokenResponse = self
            .send_json(self.exchange_code_request(flow, code))
            .await?;
        TokenSet::try_from(response)
    }

    pub async fn refresh_token(&self, credentials: &StoredCredentials) -> Result<TokenSet, Error> {
        let response: TokenResponse = self
            .send_json(
                self.refresh_token_request(&credentials.client_id, &credentials.refresh_token),
            )
            .await?;
        TokenSet::try_from(response)
    }

    pub async fn revoke_token(&self, token: &str) -> Result<(), Error> {
        let response = self
            .revoke_token_request(token)
            .send()
            .await
            .map_err(Error::Request)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response).await)
        }
    }

    pub fn list_agents_request(&self) -> reqwest::RequestBuilder {
        self.http.get(self.endpoint("/api/webapp/agents"))
    }

    pub fn list_agents_authenticated_request(&self, access_token: &str) -> reqwest::RequestBuilder {
        self.list_agents_request().bearer_auth(access_token)
    }

    pub async fn list_agents(&self, access_token: &str) -> Result<Vec<AgentSummary>, Error> {
        let response: AgentsResponse = self
            .send_json(self.list_agents_authenticated_request(access_token))
            .await?;
        Ok(response.agents)
    }

    pub fn list_agent_templates_authenticated_request(
        &self,
        access_token: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .get(self.endpoint("/api/webapp/agent-templates"))
            .bearer_auth(access_token)
    }

    pub async fn list_agent_templates(
        &self,
        access_token: &str,
    ) -> Result<Vec<AgentTemplateSummary>, Error> {
        let response: AgentTemplatesResponse = self
            .send_json(self.list_agent_templates_authenticated_request(access_token))
            .await?;
        Ok(response.templates)
    }

    pub fn create_agent_from_template_request(
        &self,
        access_token: &str,
        input: &CreateAgentFromTemplate,
    ) -> reqwest::RequestBuilder {
        self.http
            .post(self.endpoint("/api/webapp/agents"))
            .bearer_auth(access_token)
            .json(input)
    }

    pub async fn create_agent_from_template(
        &self,
        access_token: &str,
        input: &CreateAgentFromTemplate,
    ) -> Result<CreateAgentResponse, Error> {
        self.send_json(self.create_agent_from_template_request(access_token, input))
            .await
    }

    pub fn create_agent_request(
        &self,
        access_token: &str,
        input: &AgentInput,
    ) -> reqwest::RequestBuilder {
        self.http
            .post(self.endpoint("/api/webapp/agents"))
            .bearer_auth(access_token)
            .json(input)
    }

    pub async fn create_agent(
        &self,
        access_token: &str,
        input: &AgentInput,
    ) -> Result<CreateAgentResponse, Error> {
        self.send_json(self.create_agent_request(access_token, input))
            .await
    }

    pub fn agent_config_authenticated_request(
        &self,
        access_token: &str,
        agent_id: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut endpoint = Url::parse(&self.endpoint("/api/webapp/agents"))?;
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| Error::InvalidContract)?;
        segments.push(agent_id).push("edit");
        drop(segments);
        Ok(self.http.get(endpoint).bearer_auth(access_token))
    }

    pub async fn agent_config(
        &self,
        access_token: &str,
        agent_id: &str,
    ) -> Result<AgentConfig, Error> {
        let response: AgentEditResponse = self
            .send_json(self.agent_config_authenticated_request(access_token, agent_id)?)
            .await?;
        Ok(response.agent)
    }

    pub fn list_conversations_authenticated_request(
        &self,
        access_token: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .get(self.endpoint("/api/webapp/conversations"))
            .bearer_auth(access_token)
    }

    pub async fn list_conversations(
        &self,
        access_token: &str,
    ) -> Result<Vec<ConversationSummary>, Error> {
        let response: ConversationsResponse = self
            .send_json(self.list_conversations_authenticated_request(access_token))
            .await?;
        Ok(response.conversations)
    }

    pub fn search_conversations_authenticated_request(
        &self,
        access_token: &str,
        query: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut endpoint = Url::parse(&self.endpoint("/api/webapp/conversations"))?;
        endpoint.query_pairs_mut().append_pair("q", query);
        Ok(self.http.get(endpoint).bearer_auth(access_token))
    }

    pub async fn search_conversations(
        &self,
        access_token: &str,
        query: &str,
    ) -> Result<Vec<ConversationSearchHit>, Error> {
        #[derive(Deserialize)]
        struct SearchResponse {
            conversations: Vec<ConversationSearchHit>,
        }

        let response: SearchResponse = self
            .send_json(self.search_conversations_authenticated_request(access_token, query)?)
            .await?;
        Ok(response.conversations)
    }

    pub fn conversation_authenticated_request(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
    ) -> Result<reqwest::RequestBuilder, Error> {
        self.conversation_page_authenticated_request(
            access_token,
            locator,
            CONVERSATION_MESSAGE_PAGE_LIMIT,
            0,
        )
    }

    pub fn conversation_page_authenticated_request(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        limit: u32,
        offset: u32,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut endpoint = self.conversation_endpoint(locator, None)?;
        endpoint
            .query_pairs_mut()
            .append_pair("limit", &limit.to_string())
            .append_pair("offset", &offset.to_string());
        Ok(self.http.get(endpoint).bearer_auth(access_token))
    }

    pub async fn conversation(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
    ) -> Result<ConversationDetail, Error> {
        let mut detail: ConversationDetail = self
            .send_json(self.conversation_authenticated_request(access_token, locator)?)
            .await?;
        let initial_offset = detail
            .meta
            .message_count
            .saturating_sub(CONVERSATION_MESSAGE_PAGE_LIMIT);
        let mut messages = Vec::new();
        let mut offset = initial_offset;

        if initial_offset == 0 {
            let page_len = detail.messages.len();
            messages.append(&mut detail.messages);
            if page_len < CONVERSATION_MESSAGE_PAGE_LIMIT as usize {
                return Ok(detail);
            }
            offset = CONVERSATION_MESSAGE_PAGE_LIMIT;
        }

        loop {
            let page: ConversationDetail = self
                .send_json(self.conversation_page_authenticated_request(
                    access_token,
                    locator,
                    CONVERSATION_MESSAGE_PAGE_LIMIT,
                    offset,
                )?)
                .await?;
            let page_len = page.messages.len();
            messages.extend(page.messages);
            if page_len < CONVERSATION_MESSAGE_PAGE_LIMIT as usize {
                break;
            }
            offset = offset.saturating_add(CONVERSATION_MESSAGE_PAGE_LIMIT);
        }

        detail.messages = newest_messages(messages);
        Ok(detail)
    }

    pub fn conversation_action_authenticated_request(
        &self,
        method: reqwest::Method,
        access_token: &str,
        locator: &ConversationLocator,
        action: ConversationAction,
    ) -> Result<reqwest::RequestBuilder, Error> {
        Ok(self
            .http
            .request(method, self.conversation_endpoint(locator, Some(action))?)
            .bearer_auth(access_token))
    }

    pub async fn set_conversation_blocked(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        blocked: bool,
    ) -> Result<BlockConversationResponse, Error> {
        let method = if blocked {
            reqwest::Method::POST
        } else {
            reqwest::Method::DELETE
        };
        self.send_json(self.conversation_action_authenticated_request(
            method,
            access_token,
            locator,
            ConversationAction::Block,
        )?)
        .await
    }

    pub async fn start_conversation_takeover(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
    ) -> Result<ConversationTakeover, Error> {
        let response: TakeoverResponse = self
            .send_json(self.conversation_action_authenticated_request(
                reqwest::Method::POST,
                access_token,
                locator,
                ConversationAction::Takeover,
            )?)
            .await?;
        response.takeover.ok_or(Error::InvalidContract)
    }

    pub async fn end_conversation_takeover(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
    ) -> Result<(), Error> {
        let response: TakeoverResponse = self
            .send_json(self.conversation_action_authenticated_request(
                reqwest::Method::DELETE,
                access_token,
                locator,
                ConversationAction::Takeover,
            )?)
            .await?;
        if response.takeover.is_some() {
            return Err(Error::InvalidContract);
        }
        Ok(())
    }

    pub async fn clear_conversation_history(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
    ) -> Result<ClearConversationResponse, Error> {
        self.send_json(self.conversation_action_authenticated_request(
            reqwest::Method::DELETE,
            access_token,
            locator,
            ConversationAction::History,
        )?)
        .await
    }

    pub async fn send_conversation_message(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        text: String,
    ) -> Result<SendConversationMessageResponse, Error> {
        self.send_json(self.send_conversation_message_authenticated_request(
            access_token,
            locator,
            text,
        )?)
        .await
    }

    pub fn send_conversation_message_authenticated_request(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        text: String,
    ) -> Result<reqwest::RequestBuilder, Error> {
        Ok(self
            .conversation_action_authenticated_request(
                reqwest::Method::POST,
                access_token,
                locator,
                ConversationAction::Messages,
            )?
            .json(&ConversationTextInput { text }))
    }

    pub async fn steer_conversation(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        text: String,
    ) -> Result<SteerConversationResponse, Error> {
        self.send_json(self.steer_conversation_authenticated_request(
            access_token,
            locator,
            text,
        )?)
        .await
    }

    pub fn steer_conversation_authenticated_request(
        &self,
        access_token: &str,
        locator: &ConversationLocator,
        text: String,
    ) -> Result<reqwest::RequestBuilder, Error> {
        Ok(self
            .conversation_action_authenticated_request(
                reqwest::Method::POST,
                access_token,
                locator,
                ConversationAction::Steer,
            )?
            .json(&ConversationTextInput { text }))
    }

    fn conversation_endpoint(
        &self,
        locator: &ConversationLocator,
        action: Option<ConversationAction>,
    ) -> Result<Url, Error> {
        let mut endpoint = Url::parse(&self.endpoint("/api/webapp/conversations"))?;
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| Error::InvalidContract)?;
        segments.push(&locator.identity);
        if let Some(action) = action {
            segments.push(action.path());
        }
        drop(segments);
        if let Some(agent_id) = locator.agent_id.as_deref() {
            endpoint.query_pairs_mut().append_pair("agentId", agent_id);
        } else if let Some(api_key) = locator.api_key.as_deref() {
            endpoint.query_pairs_mut().append_pair("apiKey", api_key);
        }
        Ok(endpoint)
    }

    fn exchange_code_request(
        &self,
        flow: &AuthorizationFlow,
        code: &str,
    ) -> reqwest::RequestBuilder {
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("code_verifier", &flow.code_verifier)
            .append_pair("client_id", &flow.client_id)
            .append_pair("redirect_uri", &flow.redirect_uri)
            .finish();
        self.form_request("/oauth/token", body)
    }

    fn refresh_token_request(
        &self,
        client_id: &str,
        refresh_token: &str,
    ) -> reqwest::RequestBuilder {
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", client_id)
            .finish();
        self.form_request("/oauth/token", body)
    }

    fn revoke_token_request(&self, token: &str) -> reqwest::RequestBuilder {
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .finish();
        self.form_request("/oauth/revoke", body)
    }

    fn form_request(&self, path: &str, body: String) -> reqwest::RequestBuilder {
        self.http
            .post(self.endpoint(path))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .body(body)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, Error> {
        let response = request.send().await.map_err(Error::Request)?;
        if response.status().is_success() {
            Ok(response.json().await.map_err(Error::Request)?)
        } else {
            Err(response_error(response).await)
        }
    }

    async fn send_contract_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        endpoint: &'static str,
    ) -> Result<T, Error> {
        let response = request.send().await.map_err(Error::Request)?;
        let status = response.status();
        let body = response.text().await.map_err(Error::Request)?;
        tracing::debug!(endpoint, %status, body, "Keiki OAuth response");
        if !status.is_success() {
            return Err(response_error_body(status, &body));
        }
        serde_json::from_str(&body).map_err(Error::Json)
    }
}

fn newest_messages(
    mut messages: Vec<keiki_model::ConversationMessage>,
) -> Vec<keiki_model::ConversationMessage> {
    let first_message = messages
        .len()
        .saturating_sub(CONVERSATION_MESSAGE_PAGE_LIMIT as usize);
    messages.drain(..first_message);
    messages
}

impl AuthorizationFlow {
    pub fn new(client_id: String, redirect_uri: String) -> Self {
        let code_verifier = random_urlsafe();
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        Self {
            client_id,
            redirect_uri,
            code_verifier,
            code_challenge,
            state: random_urlsafe(),
        }
    }

    pub fn authorization_code(&self, callback: &str) -> Result<String, Error> {
        let callback = Url::parse(callback)?;
        let redirect = Url::parse(&self.redirect_uri)?;
        if callback.scheme() != redirect.scheme()
            || callback.host_str() != redirect.host_str()
            || callback.port() != redirect.port()
            || callback.path() != redirect.path()
            || callback.fragment().is_some()
        {
            return Err(Error::InvalidCallback);
        }

        let query = callback.query_pairs().collect::<Vec<_>>();
        if unique_query_value(&query, "state") != Some(self.state.as_str()) {
            return Err(Error::InvalidCallback);
        }
        if let Some(error) = unique_query_value(&query, "error") {
            return Err(Error::AuthorizationRejected(error.to_string()));
        }
        unique_query_value(&query, "code")
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .ok_or(Error::InvalidCallback)
    }

    pub fn stored_credentials(&self, tokens: &TokenSet) -> StoredCredentials {
        StoredCredentials {
            client_id: self.client_id.clone(),
            refresh_token: tokens.refresh_token.clone(),
        }
    }

    #[cfg(test)]
    fn from_parts(
        client_id: String,
        redirect_uri: String,
        code_verifier: String,
        code_challenge: String,
        state: String,
    ) -> Self {
        Self {
            client_id,
            redirect_uri,
            code_verifier,
            code_challenge,
            state,
        }
    }
}

impl TokenSet {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn stored_credentials(&self, client_id: String) -> StoredCredentials {
        StoredCredentials {
            client_id,
            refresh_token: self.refresh_token.clone(),
        }
    }

    pub fn should_refresh(&self) -> bool {
        Instant::now() + Duration::from_secs(60) >= self.expires_at
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl std::fmt::Debug for AuthorizationFlow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationFlow")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("code_verifier", &"[redacted]")
            .field("code_challenge", &self.code_challenge)
            .field("state", &"[redacted]")
            .finish()
    }
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredCredentials")
            .field("client_id", &self.client_id)
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}

impl TryFrom<TokenResponse> for TokenSet {
    type Error = Error;

    fn try_from(response: TokenResponse) -> Result<Self, Self::Error> {
        if response.access_token.is_empty()
            || response.refresh_token.is_empty()
            || !response.token_type.eq_ignore_ascii_case("bearer")
            || !response
                .scope
                .split_ascii_whitespace()
                .any(|scope| scope == "manage")
        {
            return Err(Error::InvalidContract);
        }
        Ok(Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: Instant::now() + Duration::from_secs(response.expires_in),
            scope: response.scope,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    revocation_endpoint: String,
    code_challenge_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    bearer_methods_supported: Vec<String>,
}

#[derive(Serialize)]
struct RegistrationRequest<'a> {
    client_name: &'a str,
    redirect_uris: [&'a str; 1],
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

fn random_urlsafe() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unique_query_value<'a>(
    query: &'a [(std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)],
    key: &str,
) -> Option<&'a str> {
    let mut matches = query
        .iter()
        .filter_map(|(name, value)| (name == key).then_some(value.as_ref()));
    let value = matches.next()?;
    matches.next().is_none().then_some(value)
}

async fn response_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    response_error_body(status, &body)
}

fn response_error_body(status: StatusCode, body: &str) -> Error {
    let body = serde_json::from_str::<ErrorResponse>(body).ok();
    let message = body
        .and_then(|body| body.error_description.or(body.error))
        .unwrap_or_else(|| "request failed".into());
    Error::Api { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_ID: &str = "client-123";
    const REDIRECT_URI: &str = "keiki://oauth/callback";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn loopback_redirect_uri_uses_callback_path() {
        let redirect_uri = loopback_redirect_uri(8976);

        assert_eq!(redirect_uri, "http://127.0.0.1:8976/oauth/callback");
    }

    #[test]
    fn loopback_request_line_extracts_callback_query() {
        let callback = callback_url_from_request_line(
            "GET /oauth/callback?code=code-123&state=state-123 HTTP/1.1\r\n",
            "http://127.0.0.1:8976/oauth/callback",
        )
        .unwrap();

        assert_eq!(
            callback.as_deref(),
            Some("http://127.0.0.1:8976/oauth/callback?code=code-123&state=state-123")
        );
    }

    #[test]
    fn loopback_request_line_ignores_unrelated_paths() {
        let callback = callback_url_from_request_line(
            "GET /favicon.ico HTTP/1.1\r\n",
            "http://127.0.0.1:8976/oauth/callback",
        )
        .unwrap();

        assert_eq!(callback, None);
    }

    #[test]
    fn loopback_error_callback_is_rejected_as_authorization_error() {
        let callback = callback_url_from_request_line(
            "GET /oauth/callback?error=access_denied&state=state-123 HTTP/1.1\r\n",
            "http://127.0.0.1:8976/oauth/callback",
        )
        .unwrap()
        .unwrap();
        let flow = AuthorizationFlow::from_parts(
            CLIENT_ID.into(),
            "http://127.0.0.1:8976/oauth/callback".into(),
            VERIFIER.into(),
            CHALLENGE.into(),
            "state-123".into(),
        );

        assert!(matches!(
            flow.authorization_code(&callback),
            Err(Error::AuthorizationRejected(error)) if error == "access_denied"
        ));
    }

    #[test]
    fn endpoint_joining_normalizes_the_base_url() {
        let client = Client::new("https://keiki.example/");

        let request = client.list_agents_request().build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://keiki.example/api/webapp/agents"
        );
    }

    #[test]
    fn authorization_url_uses_manage_scope_and_pkce() {
        let client = Client::new("https://keiki.example/");
        let flow = AuthorizationFlow::from_parts(
            CLIENT_ID.into(),
            REDIRECT_URI.into(),
            VERIFIER.into(),
            CHALLENGE.into(),
            "state-123".into(),
        );

        let url = client.authorization_url(&flow).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://keiki.example/oauth/authorize")
        );
        assert_eq!(
            query.get("response_type").map(|value| value.as_ref()),
            Some("code")
        );
        assert_eq!(
            query.get("client_id").map(|value| value.as_ref()),
            Some(CLIENT_ID)
        );
        assert_eq!(
            query.get("redirect_uri").map(|value| value.as_ref()),
            Some(REDIRECT_URI)
        );
        assert_eq!(
            query.get("scope").map(|value| value.as_ref()),
            Some("manage")
        );
        assert_eq!(
            query.get("resource").map(|value| value.as_ref()),
            Some("https://keiki.example/api/webapp")
        );
        assert_eq!(
            query.get("code_challenge").map(|value| value.as_ref()),
            Some(CHALLENGE)
        );
        assert_eq!(
            query
                .get("code_challenge_method")
                .map(|value| value.as_ref()),
            Some("S256")
        );
        assert_eq!(
            query.get("state").map(|value| value.as_ref()),
            Some("state-123")
        );
    }

    #[test]
    fn generated_authorization_flows_use_fresh_pkce_and_state_values() {
        let first = AuthorizationFlow::new(CLIENT_ID.into(), REDIRECT_URI.into());
        let second = AuthorizationFlow::new(CLIENT_ID.into(), REDIRECT_URI.into());

        assert_eq!(
            first.code_challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(first.code_verifier.as_bytes()))
        );
        assert_eq!(first.code_verifier.len(), 43);
        assert_eq!(first.state.len(), 43);
        assert_ne!(first.code_verifier, second.code_verifier);
        assert_ne!(first.state, second.state);
    }

    #[test]
    fn callback_requires_the_exact_redirect_and_state() {
        let flow = AuthorizationFlow::from_parts(
            CLIENT_ID.into(),
            REDIRECT_URI.into(),
            VERIFIER.into(),
            CHALLENGE.into(),
            "state-123".into(),
        );

        assert_eq!(
            flow.authorization_code("keiki://oauth/callback?code=code-123&state=state-123")
                .unwrap(),
            "code-123"
        );
        assert!(matches!(
            flow.authorization_code("keiki://oauth/callback?code=code-123&state=wrong"),
            Err(Error::InvalidCallback)
        ));
        assert!(matches!(
            flow.authorization_code("keiki://other/callback?code=code-123&state=state-123"),
            Err(Error::InvalidCallback)
        ));
        assert!(matches!(
            flow.authorization_code(
                "keiki://oauth/callback?code=code-123&state=state-123&state=state-123"
            ),
            Err(Error::InvalidCallback)
        ));
    }

    #[test]
    fn token_requests_are_form_encoded_without_client_authentication() {
        let client = Client::new("https://keiki.example");
        let flow = AuthorizationFlow::from_parts(
            CLIENT_ID.into(),
            REDIRECT_URI.into(),
            VERIFIER.into(),
            CHALLENGE.into(),
            "state-123".into(),
        );

        let exchange = client
            .exchange_code_request(&flow, "code-123")
            .build()
            .unwrap();
        assert_eq!(exchange.url().as_str(), "https://keiki.example/oauth/token");
        assert!(
            exchange
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
        assert_eq!(
            std::str::from_utf8(exchange.body().unwrap().as_bytes().unwrap()).unwrap(),
            "grant_type=authorization_code&code=code-123&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk&client_id=client-123&redirect_uri=keiki%3A%2F%2Foauth%2Fcallback"
        );

        let refresh = client
            .refresh_token_request(CLIENT_ID, "refresh token")
            .build()
            .unwrap();
        assert!(
            refresh
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
        assert_eq!(
            std::str::from_utf8(refresh.body().unwrap().as_bytes().unwrap()).unwrap(),
            "grant_type=refresh_token&refresh_token=refresh+token&client_id=client-123"
        );

        let revoke = client
            .revoke_token_request("refresh token")
            .build()
            .unwrap();
        assert_eq!(revoke.url().as_str(), "https://keiki.example/oauth/revoke");
        assert!(
            revoke
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
        assert_eq!(
            std::str::from_utf8(revoke.body().unwrap().as_bytes().unwrap()).unwrap(),
            "token=refresh+token"
        );
    }

    #[test]
    fn token_responses_preserve_rotated_refresh_credentials_without_debugging_secrets() {
        let tokens = TokenSet::try_from(TokenResponse {
            access_token: "access-secret".into(),
            refresh_token: "rotated-secret".into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: "manage".into(),
        })
        .unwrap();
        let stored = tokens.stored_credentials(CLIENT_ID.into());

        assert_eq!(tokens.access_token(), "access-secret");
        assert_eq!(stored.refresh_token, "rotated-secret");
        assert!(!format!("{tokens:?}").contains("access-secret"));
        assert!(!format!("{tokens:?}").contains("rotated-secret"));
        assert!(!format!("{stored:?}").contains("rotated-secret"));
    }

    #[test]
    fn authenticated_requests_use_bearer_tokens() {
        let client = Client::new("https://keiki.example");

        let request = client
            .list_agents_authenticated_request("access-token")
            .build()
            .unwrap();

        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer access-token"
        );
    }

    #[test]
    fn agent_requests_use_the_webapp_contract() {
        let client = Client::new("https://keiki.example");
        let templates = client
            .list_agent_templates_authenticated_request("access-token")
            .build()
            .unwrap();
        assert_eq!(
            templates.url().as_str(),
            "https://keiki.example/api/webapp/agent-templates"
        );
        assert_eq!(
            templates.headers()[reqwest::header::AUTHORIZATION],
            "Bearer access-token"
        );

        let create = client
            .create_agent_from_template_request(
                "access-token",
                &CreateAgentFromTemplate {
                    template: "orchid".into(),
                    name: Some("My Orchid".into()),
                    line_number: Some("+15551234567".into()),
                },
            )
            .build()
            .unwrap();
        assert_eq!(
            create.url().as_str(),
            "https://keiki.example/api/webapp/agents"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(create.body().unwrap().as_bytes().unwrap())
                .unwrap(),
            serde_json::json!({
                "template": "orchid",
                "name": "My Orchid",
                "lineNumber": "+15551234567"
            })
        );

        let builder = client
            .create_agent_request(
                "access-token",
                &AgentInput {
                    name: "Custom".into(),
                    model: "google/gemini-3.5-flash".into(),
                    system_prompt: "Be helpful".into(),
                    max_steps: 25,
                    history_limit: 50,
                    reasoning_effort: keiki_model::ReasoningEffort::Medium,
                    line_number: None,
                    harness: keiki_model::AgentHarness::Flue,
                    features: keiki_model::AgentFeatures {
                        memory: true,
                        steering: true,
                        media: false,
                        browser: true,
                        scrape: true,
                        sandbox: true,
                        mcp: true,
                        escalation: false,
                        loops: true,
                        guards: true,
                        wallet: false,
                    },
                    escalation_routes: keiki_model::EscalationRoutes::default(),
                    skill_ids: vec!["skill-1".into()],
                    sandbox_script_ids: vec!["script-1".into()],
                    sandbox_env_secrets: vec!["DATABASE_URL".into()],
                    storage_mode: Some(keiki_model::StorageMode::Managed),
                },
            )
            .build()
            .unwrap();
        let builder_body = serde_json::from_slice::<serde_json::Value>(
            builder.body().unwrap().as_bytes().unwrap(),
        )
        .unwrap();
        assert_eq!(builder_body["systemPrompt"], "Be helpful");
        assert_eq!(builder_body["reasoningEffort"], "medium");
        assert_eq!(builder_body["storageMode"], "managed");
        assert_eq!(builder_body["skillIds"], serde_json::json!(["skill-1"]));
        assert_eq!(
            builder_body["sandboxEnvSecrets"],
            serde_json::json!(["DATABASE_URL"])
        );

        let edit = client
            .agent_config_authenticated_request("access-token", "agent/with space")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            edit.url().as_str(),
            "https://keiki.example/api/webapp/agents/agent%2Fwith%20space/edit"
        );
    }

    #[test]
    fn conversation_requests_encode_identity_and_pin_the_owner() {
        let client = Client::new("https://keiki.example");
        let locator = ConversationLocator {
            identity: "tg:user/name@example.com".into(),
            agent_id: Some("agent/id".into()),
            api_key: Some("ignored-key".into()),
        };

        let detail = client
            .conversation_authenticated_request("access-token", &locator)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            detail.url().as_str(),
            "https://keiki.example/api/webapp/conversations/tg:user%2Fname@example.com?agentId=agent%2Fid&limit=500&offset=0"
        );

        let newest_page = client
            .conversation_page_authenticated_request("access-token", &locator, 500, 272)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            newest_page.url().as_str(),
            "https://keiki.example/api/webapp/conversations/tg:user%2Fname@example.com?agentId=agent%2Fid&limit=500&offset=272"
        );

        let takeover = client
            .conversation_action_authenticated_request(
                reqwest::Method::POST,
                "access-token",
                &locator,
                ConversationAction::Takeover,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            takeover.url().as_str(),
            "https://keiki.example/api/webapp/conversations/tg:user%2Fname@example.com/takeover?agentId=agent%2Fid"
        );

        let agentless = ConversationLocator {
            identity: "foo@example.com".into(),
            agent_id: None,
            api_key: Some("key/with space".into()),
        };
        let agentless_detail = client
            .conversation_authenticated_request("access-token", &agentless)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            agentless_detail.url().as_str(),
            "https://keiki.example/api/webapp/conversations/foo@example.com?apiKey=key%2Fwith+space&limit=500&offset=0"
        );

        let search = client
            .search_conversations_authenticated_request("access-token", "refund / duplicate")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            search.url().as_str(),
            "https://keiki.example/api/webapp/conversations?q=refund+%2F+duplicate"
        );

        let message = client
            .send_conversation_message_authenticated_request(
                "access-token",
                &locator,
                "Operator reply".into(),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(message.method(), reqwest::Method::POST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                message.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            serde_json::json!({ "text": "Operator reply" })
        );

        let steer = client
            .steer_conversation_authenticated_request(
                "access-token",
                &locator,
                "Draft a response".into(),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(steer.body().unwrap().as_bytes().unwrap())
                .unwrap(),
            serde_json::json!({ "text": "Draft a response" })
        );
    }

    #[test]
    fn newest_message_window_keeps_the_tail_of_multiple_pages() {
        let messages = (0..503)
            .map(|index| keiki_model::ConversationMessage {
                id: index.to_string(),
                direction: keiki_model::MessageDirection::Inbound,
                content: format!("Message {index}"),
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
            })
            .collect();
        let newest = newest_messages(messages);

        assert_eq!(newest.len(), 500);
        assert_eq!(newest.first().map(|message| message.id.as_str()), Some("3"));
        assert_eq!(
            newest.last().map(|message| message.id.as_str()),
            Some("502")
        );
    }
}
