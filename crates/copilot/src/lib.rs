//! Client-side Copilot transport and AG-UI normalization.

use std::{collections::HashMap, fmt};

use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, ToolCall};

/// Credentials supplied by the caller for each request.
///
/// The access token intentionally does not live on [`Client`]. Keiki refresh
/// can replace it between calls without rebuilding the HTTP client.
#[derive(Clone)]
pub struct CopilotCredentials {
    pub base_url: String,
    pub access_token: String,
}

impl fmt::Debug for CopilotCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotCredentials")
            .field("base_url", &self.base_url)
            .field("access_token", &"[redacted]")
            .finish()
    }
}

/// Errors returned by the Copilot transport and wire decoder.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Copilot authorization expired")]
    Unauthorized,
    #[error("Copilot returned {status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error("invalid Copilot response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid Copilot SSE frame: {0}")]
    Sse(String),
    #[error("invalid Copilot URL: {0}")]
    Url(String),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

/// HTTP client for the dashboard Copilot routes.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Start a turn and return the untouched SSE response body.
    pub async fn post_chat<T: Serialize>(
        &self,
        credentials: &CopilotCredentials,
        body: &T,
    ) -> Result<Response, Error> {
        self.send_stream(self.authorized(
            credentials,
            self.http.post(self.endpoint("/chat")).json(body),
        ))
        .await
    }

    /// Fetch the durable thread transcript as an SSE response.
    pub async fn get_chat_thread(
        &self,
        credentials: &CopilotCredentials,
        thread_id: &str,
    ) -> Result<Response, Error> {
        let mut url = self.url("/chat")?;
        url.query_pairs_mut().append_pair("threadId", thread_id);
        self.send_stream(self.authorized(credentials, self.http.get(url)))
            .await
    }

    /// Replay a durable run from an offset as an SSE response.
    pub async fn get_chat_run(
        &self,
        credentials: &CopilotCredentials,
        run_id: &str,
        offset: &str,
    ) -> Result<Response, Error> {
        let mut url = self.url("/chat")?;
        url.query_pairs_mut()
            .append_pair("runId", run_id)
            .append_pair("offset", offset);
        self.send_stream(self.authorized(credentials, self.http.get(url)))
            .await
    }

    pub async fn cancel_run(
        &self,
        credentials: &CopilotCredentials,
        run_id: &str,
    ) -> Result<Value, Error> {
        self.send_json(
            self.authorized(
                credentials,
                self.http
                    .post(self.endpoint(&format!("/runs/{run_id}/cancel"))),
            ),
        )
        .await
    }

    pub async fn list_threads(
        &self,
        credentials: &CopilotCredentials,
    ) -> Result<ThreadList, Error> {
        self.send_json(self.authorized(credentials, self.http.get(self.endpoint("/threads"))))
            .await
    }

    pub async fn get_thread(
        &self,
        credentials: &CopilotCredentials,
        thread_id: &str,
    ) -> Result<Thread, Error> {
        self.send_json(
            self.authorized(
                credentials,
                self.http
                    .get(self.endpoint(&format!("/threads/{thread_id}"))),
            ),
        )
        .await
    }

    pub async fn update_thread<T: Serialize>(
        &self,
        credentials: &CopilotCredentials,
        thread_id: &str,
        body: &T,
    ) -> Result<Thread, Error> {
        self.send_json(
            self.authorized(
                credentials,
                self.http
                    .put(self.endpoint(&format!("/threads/{thread_id}")))
                    .json(body),
            ),
        )
        .await
    }

    pub async fn delete_thread(
        &self,
        credentials: &CopilotCredentials,
        thread_id: &str,
    ) -> Result<Value, Error> {
        self.send_json(
            self.authorized(
                credentials,
                self.http
                    .delete(self.endpoint(&format!("/threads/{thread_id}"))),
            ),
        )
        .await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/copilot{path}", self.base_url)
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, Error> {
        self.endpoint(path)
            .parse()
            .map_err(|error| Error::Url(format!("invalid Copilot base URL: {error}")))
    }

    fn authorized(
        &self,
        credentials: &CopilotCredentials,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        request.bearer_auth(&credentials.access_token)
    }

    async fn send_stream(&self, request: reqwest::RequestBuilder) -> Result<Response, Error> {
        let response = request.send().await?;
        ensure_success(response).await
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, Error> {
        let response = ensure_success(request.send().await?).await?;
        let body = response.bytes().await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

async fn ensure_success(response: Response) -> Result<Response, Error> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthorized);
    }
    if !status.is_success() {
        let message = response.text().await.unwrap_or_default();
        return Err(Error::Api { status, message });
    }
    Ok(response)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadList {
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub title: Option<String>,
    pub messages: Value,
    pub activity: Value,
    pub updated_at: Option<String>,
}

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: String,
}

/// Incremental SSE frame decoder.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one arbitrary network chunk and return every complete frame.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, Error> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((end, separator_len)) = frame_end(&self.buffer) {
            let bytes = self.buffer.drain(..end + separator_len).collect::<Vec<_>>();
            frames.push(parse_frame(&bytes[..end])?);
        }
        Ok(frames)
    }

    /// Flush a final frame when a server closes without a trailing blank line.
    pub fn finish(&mut self) -> Result<Option<SseFrame>, Error> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.buffer);
        Ok(Some(parse_frame(&bytes)?))
    }
}

fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len().saturating_sub(1) {
        match (buffer[index], buffer[index + 1]) {
            (b'\n', b'\n') | (b'\r', b'\r') => return Some((index, 2)),
            (b'\r', b'\n') if buffer.get(index + 2..index + 4) == Some(&b"\r\n"[..]) => {
                return Some((index, 4));
            }
            _ => {}
        }
    }
    None
}

fn parse_frame(bytes: &[u8]) -> Result<SseFrame, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| Error::Sse(format!("frame was not UTF-8: {error}")))?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut event = None;
    let mut id = None;
    let mut data = Vec::new();

    for line in normalized.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line
            .split_once(':')
            .map(|(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)))
            .unwrap_or((line, ""));
        match field {
            "event" => event = Some(value.to_owned()),
            "id" => id = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }

    Ok(SseFrame {
        event,
        id,
        data: data.join("\n"),
    })
}

impl SseFrame {
    pub fn ag_ui_event(&self) -> Result<AgUiEvent, Error> {
        AgUiEvent::from_json(&self.data)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgUiEvent {
    RunStarted {
        thread_id: String,
        run_id: String,
    },
    TextMessageStart {
        message_id: String,
    },
    TextMessageContent {
        message_id: String,
        delta: String,
    },
    TextMessageChunk {
        message_id: Option<String>,
        delta: Option<String>,
    },
    TextMessageEnd {
        message_id: String,
    },
    ReasoningStart {
        message_id: String,
    },
    ReasoningMessageStart {
        message_id: String,
    },
    ReasoningMessageContent {
        message_id: String,
        delta: String,
    },
    ReasoningMessageChunk {
        message_id: Option<String>,
        delta: Option<String>,
    },
    ReasoningMessageEnd {
        message_id: String,
    },
    ReasoningEnd {
        message_id: String,
    },
    ThinkingTextMessageContent {
        delta: String,
    },
    ToolCallStart {
        tool_call_id: String,
        tool_call_name: String,
    },
    ToolCallArgs {
        tool_call_id: String,
        delta: String,
    },
    ToolCallChunk {
        tool_call_id: Option<String>,
        tool_call_name: Option<String>,
        delta: Option<String>,
    },
    ToolCallEnd {
        tool_call_id: String,
        input: Option<Value>,
    },
    ToolCallResult {
        tool_call_id: String,
        content: String,
    },
    RunFinished {
        run_id: String,
        outcome: Option<RunOutcome>,
        result: Option<Value>,
    },
    RunError {
        message: String,
    },
    Unknown {
        event_type: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Success,
    Interrupt { interrupts: Vec<Interrupt> },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interrupt {
    pub id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl AgUiEvent {
    pub fn from_json(json: &str) -> Result<Self, Error> {
        let value: Value = serde_json::from_str(json)?;
        Self::from_value(value)
    }

    fn from_value(value: Value) -> Result<Self, Error> {
        let Some(object) = value.as_object() else {
            return Err(Error::Sse("AG-UI event was not an object".into()));
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match kind {
            "RUN_STARTED" => {
                let wire: RunStartedWire = serde_json::from_value(value)?;
                Ok(Self::RunStarted {
                    thread_id: wire.thread_id,
                    run_id: wire.run_id,
                })
            }
            "TEXT_MESSAGE_START" => {
                let wire: TextMessageStartWire = serde_json::from_value(value)?;
                Ok(Self::TextMessageStart {
                    message_id: wire.message_id,
                })
            }
            "TEXT_MESSAGE_CONTENT" => {
                let wire: TextMessageContentWire = serde_json::from_value(value)?;
                Ok(Self::TextMessageContent {
                    message_id: wire.message_id,
                    delta: wire.delta,
                })
            }
            "TEXT_MESSAGE_CHUNK" => {
                let wire: TextMessageChunkWire = serde_json::from_value(value)?;
                Ok(Self::TextMessageChunk {
                    message_id: wire.message_id,
                    delta: wire.delta,
                })
            }
            "TEXT_MESSAGE_END" => {
                let wire: TextMessageEndWire = serde_json::from_value(value)?;
                Ok(Self::TextMessageEnd {
                    message_id: wire.message_id,
                })
            }
            "REASONING_START" => {
                let wire: ReasoningStartWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningStart {
                    message_id: wire.message_id,
                })
            }
            "REASONING_MESSAGE_START" => {
                let wire: ReasoningStartWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningMessageStart {
                    message_id: wire.message_id,
                })
            }
            "REASONING_MESSAGE_CONTENT" => {
                let wire: ReasoningContentWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningMessageContent {
                    message_id: wire.message_id,
                    delta: wire.delta,
                })
            }
            "REASONING_MESSAGE_CHUNK" => {
                let wire: ReasoningChunkWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningMessageChunk {
                    message_id: wire.message_id,
                    delta: wire.delta,
                })
            }
            "REASONING_MESSAGE_END" => {
                let wire: ReasoningStartWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningMessageEnd {
                    message_id: wire.message_id,
                })
            }
            "REASONING_END" => {
                let wire: ReasoningStartWire = serde_json::from_value(value)?;
                Ok(Self::ReasoningEnd {
                    message_id: wire.message_id,
                })
            }
            "THINKING_TEXT_MESSAGE_CONTENT" => {
                let wire: ThinkingContentWire = serde_json::from_value(value)?;
                Ok(Self::ThinkingTextMessageContent { delta: wire.delta })
            }
            "TOOL_CALL_START" => {
                let wire: ToolCallStartWire = serde_json::from_value(value)?;
                Ok(Self::ToolCallStart {
                    tool_call_id: wire.tool_call_id,
                    tool_call_name: wire.tool_call_name,
                })
            }
            "TOOL_CALL_ARGS" => {
                let wire: ToolCallArgsWire = serde_json::from_value(value)?;
                Ok(Self::ToolCallArgs {
                    tool_call_id: wire.tool_call_id,
                    delta: wire.delta,
                })
            }
            "TOOL_CALL_CHUNK" => {
                let wire: ToolCallChunkWire = serde_json::from_value(value)?;
                Ok(Self::ToolCallChunk {
                    tool_call_id: wire.tool_call_id,
                    tool_call_name: wire.tool_call_name,
                    delta: wire.delta,
                })
            }
            "TOOL_CALL_END" => {
                let wire: ToolCallEndWire = serde_json::from_value(value)?;
                Ok(Self::ToolCallEnd {
                    tool_call_id: wire.tool_call_id,
                    input: wire.input,
                })
            }
            "TOOL_CALL_RESULT" => {
                let wire: ToolCallResultWire = serde_json::from_value(value)?;
                Ok(Self::ToolCallResult {
                    tool_call_id: wire.tool_call_id,
                    content: wire.content,
                })
            }
            "RUN_FINISHED" => parse_run_finished(value),
            "RUN_ERROR" => {
                let wire: RunErrorWire = serde_json::from_value(value)?;
                Ok(Self::RunError {
                    message: wire.message,
                })
            }
            _ => Ok(Self::Unknown {
                event_type: kind.to_owned(),
            }),
        }
    }
}

impl<'de> Deserialize<'de> for AgUiEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunStartedWire {
    thread_id: String,
    run_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMessageStartWire {
    message_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMessageContentWire {
    message_id: String,
    delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMessageEndWire {
    message_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMessageChunkWire {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    delta: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningStartWire {
    message_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningContentWire {
    message_id: String,
    delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningChunkWire {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    delta: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThinkingContentWire {
    delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallStartWire {
    tool_call_id: String,
    tool_call_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallArgsWire {
    tool_call_id: String,
    delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallChunkWire {
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_call_name: Option<String>,
    #[serde(default)]
    delta: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallEndWire {
    tool_call_id: String,
    #[serde(default)]
    input: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResultWire {
    tool_call_id: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunErrorWire {
    message: String,
}

fn parse_run_finished(value: Value) -> Result<AgUiEvent, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Sse("RUN_FINISHED was not an object".into()))?;
    let run_id = object
        .get("runId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let result = object.get("result").cloned();
    let outcome = object.get("outcome").and_then(|raw| {
        let object = raw.as_object()?;
        match object.get("type").and_then(Value::as_str) {
            Some("success") => Some(RunOutcome::Success),
            Some("interrupt") => {
                let interrupts = object
                    .get("interrupts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|interrupt| serde_json::from_value(interrupt).ok())
                    .collect();
                Some(RunOutcome::Interrupt { interrupts })
            }
            Some(_) => Some(RunOutcome::Unknown),
            None => None,
        }
    });
    Ok(AgUiEvent::RunFinished {
        run_id,
        outcome,
        result,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResumePayload {
    pub resume: Vec<ResumeEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeEntry {
    pub interrupt_id: String,
    pub status: ResumeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeStatus {
    Resolved,
    Cancelled,
}

impl ResumePayload {
    pub fn resolved(interrupt_id: impl Into<String>) -> Self {
        Self::one(interrupt_id, ResumeStatus::Resolved)
    }

    pub fn cancelled(interrupt_id: impl Into<String>) -> Self {
        Self::one(interrupt_id, ResumeStatus::Cancelled)
    }

    fn one(interrupt_id: impl Into<String>, status: ResumeStatus) -> Self {
        Self {
            resume: vec![ResumeEntry {
                interrupt_id: interrupt_id.into(),
                status,
            }],
        }
    }
}

/// AG-UI events reduced to Comet's existing transcript event contract.
#[derive(Debug, Default)]
pub struct TurnMapper {
    assistant_message_id: Option<String>,
    tool_calls: HashMap<String, PendingToolCall>,
    last_tool_call_id: Option<String>,
    interrupts: Vec<Interrupt>,
}

#[derive(Debug)]
struct PendingToolCall {
    name: String,
    args: String,
}

impl TurnMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, event: AgUiEvent) -> Vec<AgentEvent> {
        match event {
            AgUiEvent::RunStarted { run_id, .. } => {
                vec![AgentEvent::SessionStarted {
                    harness: HarnessId::Copilot,
                    model: "copilot".into(),
                    tools: Vec::new(),
                    cwd: String::new(),
                    session_id: run_id,
                    assistant_message_id: String::new(),
                }]
            }
            AgUiEvent::TextMessageStart { message_id } => {
                self.assistant_message_id = Some(message_id);
                Vec::new()
            }
            AgUiEvent::TextMessageContent { delta, .. }
            | AgUiEvent::TextMessageChunk {
                delta: Some(delta), ..
            } => vec![AgentEvent::TextDelta { text: delta }],
            AgUiEvent::TextMessageChunk { delta: None, .. } => Vec::new(),
            AgUiEvent::TextMessageEnd { message_id } => {
                vec![AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: message_id,
                }]
            }
            AgUiEvent::ReasoningMessageContent { delta, .. }
            | AgUiEvent::ReasoningMessageChunk {
                delta: Some(delta), ..
            }
            | AgUiEvent::ThinkingTextMessageContent { delta } => {
                vec![AgentEvent::ReasoningDelta { text: delta }]
            }
            AgUiEvent::ReasoningMessageChunk { delta: None, .. }
            | AgUiEvent::ReasoningStart { .. }
            | AgUiEvent::ReasoningMessageStart { .. }
            | AgUiEvent::ReasoningMessageEnd { .. }
            | AgUiEvent::ReasoningEnd { .. } => Vec::new(),
            AgUiEvent::ToolCallStart {
                tool_call_id,
                tool_call_name,
            } => {
                self.last_tool_call_id = Some(tool_call_id.clone());
                self.tool_calls.insert(
                    tool_call_id,
                    PendingToolCall {
                        name: tool_call_name,
                        args: String::new(),
                    },
                );
                Vec::new()
            }
            AgUiEvent::ToolCallArgs {
                tool_call_id,
                delta,
            } => {
                self.last_tool_call_id = Some(tool_call_id.clone());
                self.tool_calls
                    .entry(tool_call_id)
                    .or_insert_with(|| PendingToolCall {
                        name: "unknown".into(),
                        args: String::new(),
                    })
                    .args
                    .push_str(&delta);
                Vec::new()
            }
            AgUiEvent::ToolCallChunk {
                tool_call_id,
                tool_call_name,
                delta,
            } => {
                let id = tool_call_id
                    .or_else(|| self.last_tool_call_id.clone())
                    .or_else(|| tool_call_name.clone())
                    .unwrap_or_else(|| "unknown-tool-call".into());
                self.last_tool_call_id = Some(id.clone());
                let pending = self
                    .tool_calls
                    .entry(id)
                    .or_insert_with(|| PendingToolCall {
                        name: "unknown".into(),
                        args: String::new(),
                    });
                if let Some(name) = tool_call_name {
                    pending.name = name;
                }
                if let Some(delta) = delta {
                    pending.args.push_str(&delta);
                }
                Vec::new()
            }
            AgUiEvent::ToolCallEnd {
                tool_call_id,
                input,
            } => {
                self.last_tool_call_id = Some(tool_call_id.clone());
                let Some(pending) = self.tool_calls.remove(&tool_call_id) else {
                    return Vec::new();
                };
                let input = input.or_else(|| {
                    if pending.args.is_empty() {
                        None
                    } else {
                        serde_json::from_str(&pending.args).ok()
                    }
                });
                vec![AgentEvent::ToolCall {
                    id: tool_call_id,
                    call: ToolCall::Unknown {
                        name: pending.name,
                        input,
                    },
                }]
            }
            AgUiEvent::ToolCallResult {
                tool_call_id,
                content,
            } => vec![AgentEvent::ToolResult {
                id: tool_call_id,
                is_error: false,
                output: Some(content),
                diff: None,
            }],
            AgUiEvent::RunFinished {
                outcome: Some(RunOutcome::Interrupt { interrupts }),
                ..
            } => {
                let events = self.flush_tool_calls();
                self.interrupts.extend(interrupts);
                events
            }
            AgUiEvent::RunFinished {
                run_id,
                outcome: Some(RunOutcome::Success),
                result,
            } => {
                let mut events = self.flush_tool_calls();
                events.push(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: result.map(compact_json),
                    error: None,
                    session_id: Some(run_id),
                });
                events
            }
            AgUiEvent::RunFinished { run_id, result, .. } => {
                let mut events = self.flush_tool_calls();
                events.push(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: result.map(compact_json),
                    error: None,
                    session_id: Some(run_id),
                });
                events
            }
            AgUiEvent::RunError { message } => {
                let mut events = self.flush_tool_calls();
                events.push(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                });
                events
            }
            AgUiEvent::Unknown { .. } => Vec::new(),
        }
    }

    fn flush_tool_calls(&mut self) -> Vec<AgentEvent> {
        self.tool_calls
            .drain()
            .map(|(id, pending)| AgentEvent::ToolCall {
                id,
                call: ToolCall::Unknown {
                    name: pending.name,
                    input: (!pending.args.is_empty())
                        .then(|| serde_json::from_str(&pending.args).ok())
                        .flatten(),
                },
            })
            .collect()
    }

    pub fn take_interrupts(&mut self) -> Vec<Interrupt> {
        std::mem::take(&mut self.interrupts)
    }

    pub fn take_interrupt_ids(&mut self) -> Vec<String> {
        self.take_interrupts()
            .into_iter()
            .map(|interrupt| interrupt.id)
            .collect()
    }
}

fn compact_json(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events_from_chunks(chunks: &[&[u8]]) -> Vec<AgUiEvent> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(
                decoder
                    .push(chunk)
                    .unwrap()
                    .into_iter()
                    .map(|frame| frame.ag_ui_event().unwrap()),
            );
        }
        if let Some(frame) = decoder.finish().unwrap() {
            events.push(frame.ag_ui_event().unwrap());
        }
        events
    }

    #[test]
    fn decodes_text_across_arbitrary_network_chunks() {
        let fixture = b": keep-alive\r\n\
event: message\r\n\
id: 1\r\n\
data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t\",\"runId\":\"r\"}\r\n\
\r\n\
data: {\"type\":\"TEXT_MESSAGE_START\",\"messageId\":\"m\"}\r\n\
\r\n\
data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m\",\"delta\":\"Hel\"}\r\n\
\r\n\
data: {\"type\":\"TEXT_MESSAGE_CHUNK\",\"messageId\":\"m\",\"delta\":\"lo\"}\r\n\
\r\n\
data: {\"type\":\"TEXT_MESSAGE_END\",\"messageId\":\"m\"}\r\n\
\r\n\
data: {\"type\":\"RUN_FINISHED\",\"runId\":\"r\",\"outcome\":{\"type\":\"success\"}}\r\n\
\r\n";
        let chunks = fixture.chunks(7).collect::<Vec<_>>();
        let events = events_from_chunks(&chunks);
        let mut mapper = TurnMapper::new();
        let mapped = events
            .into_iter()
            .flat_map(|event| mapper.handle(event))
            .collect::<Vec<_>>();
        assert!(matches!(mapped[0], AgentEvent::SessionStarted { .. }));
        assert_eq!(
            mapped
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "Hello"
        );
        assert!(mapped.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantMessageCompleted { assistant_message_id } if assistant_message_id == "m"
        )));
        assert!(mapped.iter().any(|event| matches!(
            event,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )));
    }

    #[test]
    fn reassembles_fragmented_tool_arguments_before_emitting() {
        let events = events_from_chunks(&[
            b"data: {\"type\":\"TOOL_CALL_START\",\"toolCallId\":\"call\",\"toolCallName\":\"search\"}\n\n",
            b"data: {\"type\":\"TOOL_CALL_ARGS\",\"toolCallId\":\"call\",\"delta\":\"{\\\"query\\\":\\\"hel\"}\n\n",
            b"data: {\"type\":\"TOOL_CALL_ARGS\",\"toolCallId\":\"call\",\"delta\":\"lo\\\"}\"}\n\n",
            b"data: {\"type\":\"TOOL_CALL_END\",\"toolCallId\":\"call\"}\n\n",
        ]);
        let mut mapper = TurnMapper::new();
        let mapped = events
            .into_iter()
            .flat_map(|event| mapper.handle(event))
            .collect::<Vec<_>>();
        assert_eq!(
            mapped,
            vec![AgentEvent::ToolCall {
                id: "call".into(),
                call: ToolCall::Unknown {
                    name: "search".into(),
                    input: Some(serde_json::json!({"query": "hello"})),
                },
            }]
        );
    }

    #[test]
    fn interrupt_finish_is_exposed_without_done() {
        let event = AgUiEvent::from_json(
            r#"{"type":"RUN_FINISHED","runId":"r","outcome":{"type":"interrupt","interrupts":[{"id":"i1","reason":"approval"}]}}"#,
        )
        .unwrap();
        let mut mapper = TurnMapper::new();
        assert!(mapper.handle(event).is_empty());
        assert_eq!(mapper.take_interrupt_ids(), vec!["i1"]);
    }

    #[test]
    fn missing_finish_outcome_completes_the_run() {
        let event = AgUiEvent::from_json(r#"{"type":"RUN_FINISHED","runId":"r"}"#).unwrap();
        let mut mapper = TurnMapper::new();
        assert_eq!(
            mapper.handle(event),
            vec![AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("r".into()),
            }]
        );
    }

    #[test]
    fn chunked_tool_calls_inherit_ids_and_flush_at_completion() {
        let events = events_from_chunks(&[
            b"data: {\"type\":\"TOOL_CALL_CHUNK\",\"toolCallName\":\"search\",\"delta\":\"{\\\"query\\\":\\\"hel\"}\n\n",
            b"data: {\"type\":\"TOOL_CALL_CHUNK\",\"delta\":\"lo\\\"}\"}\n\n",
            b"data: {\"type\":\"RUN_FINISHED\",\"runId\":\"r\"}\n\n",
        ]);
        let mut mapper = TurnMapper::new();
        let mapped = events
            .into_iter()
            .flat_map(|event| mapper.handle(event))
            .collect::<Vec<_>>();
        assert_eq!(
            mapped,
            vec![
                AgentEvent::ToolCall {
                    id: "search".into(),
                    call: ToolCall::Unknown {
                        name: "search".into(),
                        input: Some(serde_json::json!({"query": "hello"})),
                    },
                },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some("r".into()),
                },
            ]
        );
    }

    #[test]
    fn incomplete_tool_calls_flush_before_errors() {
        let events = events_from_chunks(&[
            b"data: {\"type\":\"TOOL_CALL_CHUNK\",\"toolCallId\":\"call\",\"toolCallName\":\"search\",\"delta\":\"{\\\"query\\\":\\\"hel\"}\n\n",
            b"data: {\"type\":\"RUN_ERROR\",\"message\":\"failed\"}\n\n",
        ]);
        let mut mapper = TurnMapper::new();
        let mapped = events
            .into_iter()
            .flat_map(|event| mapper.handle(event))
            .collect::<Vec<_>>();
        assert!(matches!(mapped.first(), Some(AgentEvent::ToolCall { id, .. }) if id == "call"));
        assert!(matches!(
            mapped.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Errored,
                error: Some(message),
                ..
            }) if message == "failed"
        ));
    }

    #[test]
    fn run_error_emits_failed_done() {
        let event =
            AgUiEvent::from_json(r#"{"type":"RUN_ERROR","message":"model failed"}"#).unwrap();
        let mut mapper = TurnMapper::new();
        assert_eq!(
            mapper.handle(event),
            vec![AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("model failed".into()),
                session_id: None,
            }]
        );
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let event = AgUiEvent::from_json(r#"{"type":"FUTURE_EVENT","newField":true}"#).unwrap();
        let mut mapper = TurnMapper::new();
        assert!(mapper.handle(event).is_empty());
    }

    #[test]
    fn sse_data_lines_are_joined_and_unknown_fields_are_ignored() {
        let mut decoder = SseDecoder::new();
        let frames = decoder
            .push(
                b": comment\n\
event: message\n\
id: 42\n\
data: {\"type\":\"RUN_ERROR\",\n\
data: \"message\":\"failed\",\"future\":true}\n\n",
            )
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("message"));
        assert_eq!(frames[0].id.as_deref(), Some("42"));
        assert_eq!(
            frames[0].data,
            "{\"type\":\"RUN_ERROR\",\n\"message\":\"failed\",\"future\":true}"
        );
        assert_eq!(
            frames[0].ag_ui_event().unwrap(),
            AgUiEvent::RunError {
                message: "failed".into()
            }
        );
    }

    #[test]
    fn credentials_debug_redacts_the_token() {
        let credentials = CopilotCredentials {
            base_url: "https://example.test".into(),
            access_token: "secret-token".into(),
        };
        let debug = format!("{credentials:?}");
        assert!(debug.contains("https://example.test"));
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("secret-token"));
    }

    #[test]
    fn resume_payloads_use_the_ag_ui_shape() {
        assert_eq!(
            serde_json::to_value(ResumePayload::resolved("i1")).unwrap(),
            serde_json::json!({"resume":[{"interruptId":"i1","status":"resolved"}]})
        );
        assert_eq!(
            serde_json::to_value(ResumePayload::cancelled("i1")).unwrap(),
            serde_json::json!({"resume":[{"interruptId":"i1","status":"cancelled"}]})
        );
    }
}
