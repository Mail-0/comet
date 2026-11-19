use super::*;

pub const BOUND_PREFIX: &str = "keiki-bound-";

pub struct BoundConversation {
    pub agent_id: String,
    pub conversation_id: String,
    pub credentials: CopilotCredentials,
    pub cancelled: CancellationToken,
}

impl Drop for BoundConversation {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

#[async_trait]
pub trait BoundConversationSource: Send + Sync {
    async fn authorize(&self, session: &str) -> Result<BoundConversation, String>;
    async fn settled(&self, session: &str) -> Result<(), String>;
    async fn unauthorized(&self, token: &str);
    async fn questions(&self, session: &str, questions: Value) -> Result<(), String>;
}

pub(super) async fn run(
    credentials: Arc<dyn CopilotCredentialSource>,
    request: RunRequest,
    mut controls: RunControls,
    tx: mpsc::Sender<AgentEventResult>,
) {
    if !request.attachments.is_empty() {
        send_done_error(&tx, "Keiki conversations currently accept text only").await;
        return;
    }
    let Some(source) = credentials.bound_source() else {
        send_done_error(&tx, "Keiki conversation bindings are unavailable").await;
        return;
    };
    let session = request.resume.unwrap();
    let mut prompt = request.prompt;
    let mut steer = false;
    let mut steering_closed = false;
    loop {
        let bound = match source.authorize(&session).await {
            Ok(bound) => bound,
            Err(error) => {
                send_done_error(&tx, &error).await;
                return;
            }
        };
        let client = keiki_api::Client::new(bound.credentials.base_url.clone());
        let locator = keiki_api::ConversationLocator {
            agent_id: Some(bound.agent_id.clone()),
            identity: bound.conversation_id.clone(),
            api_key: None,
        };
        let operation = async {
            if bound.conversation_id.starts_with("api:") {
                let response = match client
                    .steer_conversation_stream(
                        &bound.credentials.access_token,
                        &locator,
                        prompt.clone(),
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        return Err(api_error(
                            source.as_ref(),
                            &bound.credentials.access_token,
                            error,
                        )
                        .await);
                    }
                };
                Ok(Some(response))
            } else {
                let result = if steer {
                    client
                        .steer_conversation(
                            &bound.credentials.access_token,
                            &locator,
                            prompt.clone(),
                        )
                        .await
                        .map(|_| ())
                } else {
                    client
                        .send_conversation_message(
                            &bound.credentials.access_token,
                            &locator,
                            prompt.clone(),
                        )
                        .await
                        .map(|_| ())
                };
                if let Err(error) = result {
                    return Err(
                        api_error(source.as_ref(), &bound.credentials.access_token, error).await,
                    );
                }
                Ok(None)
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = controls.interrupt.cancelled() => None,
            _ = bound.cancelled.cancelled() => None,
            result = operation => Some(result),
        };
        // The request is dropped before reconciliation, including on Stop.
        match outcome {
            None => {
                send_done_interrupted(&tx).await;
                let _ = source.settled(&session).await;
                return;
            }
            Some(Err(error)) => {
                send_done_error(&tx, &error).await;
                let _ = source.settled(&session).await;
                return;
            }
            Some(Ok(response)) => {
                if let Some(response) = response {
                    enum Outcome {
                        Finished(Result<Value, String>),
                        Steer(SteerMessage),
                        Cancelled,
                    }
                    let outcome = {
                        let stream = consume(response, &session, &tx);
                        tokio::pin!(stream);
                        loop {
                            tokio::select! {
                                biased;
                                _ = controls.interrupt.cancelled() => break Outcome::Cancelled,
                                _ = bound.cancelled.cancelled() => break Outcome::Cancelled,
                                result = &mut stream => break Outcome::Finished(result),
                                next = controls.steering.recv(), if !steering_closed => match next {
                                    Some(next) => break Outcome::Steer(next),
                                    None => steering_closed = true,
                                }
                            }
                        }
                    };
                    match outcome {
                        Outcome::Cancelled => {
                            send_done_interrupted(&tx).await;
                            let _ = source.settled(&session).await;
                            return;
                        }
                        Outcome::Steer(next) => {
                            let _ = tx
                                .send(Ok(AgentEvent::Steered {
                                    assistant_message_id: None,
                                    next_assistant_message_id: Some(Uuid::new_v4().to_string()),
                                }))
                                .await;
                            prompt = next.prompt;
                            steer = true;
                            continue;
                        }
                        Outcome::Finished(Err(error)) => {
                            send_done_error(&tx, &error).await;
                            let _ = source.settled(&session).await;
                            return;
                        }
                        Outcome::Finished(Ok(questions)) => {
                            if let Err(error) = source.questions(&session, questions).await {
                                send_done_error(&tx, &error).await;
                                return;
                            }
                        }
                    }
                }
                let _ = tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Completed,
                        session_id: Some(session.clone()),
                        error: None,
                        result: None,
                    }))
                    .await;
                if let Err(error) = source.settled(&session).await {
                    let _ = tx.send(Ok(AgentEvent::Error { message: error })).await;
                }
            }
        }
        if steering_closed {
            return;
        }
        let next = tokio::select! {
            biased;
            _ = controls.interrupt.cancelled() => return,
            _ = bound.cancelled.cancelled() => return,
            next = controls.steering.recv() => next,
        };
        let Some(next) = next else { return };
        let _ = tx
            .send(Ok(AgentEvent::Steered {
                assistant_message_id: None,
                next_assistant_message_id: Some(Uuid::new_v4().to_string()),
            }))
            .await;
        prompt = next.prompt;
        steer = true;
    }
}

type AgentEventResult = Result<AgentEvent, HarnessError>;

async fn api_error(
    source: &dyn BoundConversationSource,
    token: &str,
    error: keiki_api::Error,
) -> String {
    if matches!(&error, keiki_api::Error::Api { status, .. } if *status == reqwest::StatusCode::UNAUTHORIZED)
    {
        source.unauthorized(token).await;
        "Keiki authorization expired".into()
    } else {
        "Keiki conversation request failed".into()
    }
}

async fn consume(
    response: reqwest::Response,
    session: &str,
    tx: &mpsc::Sender<AgentEventResult>,
) -> Result<Value, String> {
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
    {
        let body: keiki_api::SteerConversationResponse =
            response.json().await.map_err(|e| e.to_string())?;
        tx.send(Ok(AgentEvent::TextDelta { text: body.reply }))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(json!([]));
    }
    let mut decoder = SseDecoder::new();
    let mut mapper = TurnMapper::new();
    let mut bytes = response.bytes_stream();
    let mut finished = false;
    let mut pending = Vec::new();
    loop {
        let (frames, eof) = match bytes.next().await {
            Some(chunk) => (
                decoder
                    .push(&chunk.map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
                false,
            ),
            None => (
                decoder
                    .finish()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .collect(),
                true,
            ),
        };
        for frame in frames {
            if frame.data.is_empty() || frame.data.trim() == "[DONE]" {
                continue;
            }
            let event = frame.ag_ui_event().map_err(|e| e.to_string())?;
            for mut event in mapper.handle(event) {
                match &mut event {
                    AgentEvent::Done { status, error, .. } => {
                        finished = true;
                        if *status != DoneStatus::Completed {
                            return Err(error
                                .clone()
                                .unwrap_or_else(|| "Keiki turn did not complete".into()));
                        }
                        continue;
                    }
                    AgentEvent::SessionStarted { session_id, .. } => *session_id = session.into(),
                    _ => {}
                }
                tx.send(Ok(event)).await.map_err(|e| e.to_string())?;
            }
            for question in mapper.take_interrupts() {
                let payload = keiki_api::redact_conversation_credentials(
                    json!({"id":question.id,"reason":question.reason,"message":question.message,"metadata":question.metadata}),
                );
                pending.push(json!({"id":question.id,"question":payload,"readOnly":true,"replyInComposer":true}));
                let id = format!("keiki-question:{}", question.id);
                tx.send(Ok(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: zeron_proto::ToolCall::Unknown {
                        name: "Pending question — reply in composer".into(),
                        input: Some(json!({"pending": true, "question": payload})),
                    },
                }))
                .await
                .map_err(|e| e.to_string())?;
                tx.send(Ok(AgentEvent::ToolResult {
                    id,
                    is_error: false,
                    output: Some("Reply in the composer to steer this conversation.".into()),
                    diff: None,
                    preview_paths: Vec::new(),
                    image_payloads: Vec::new(),
                    exit_code: None,
                }))
                .await
                .map_err(|e| e.to_string())?;
                finished = true;
            }
            if finished {
                return Ok(json!(pending));
            }
        }
        if eof {
            break;
        }
    }
    if finished {
        Ok(json!(pending))
    } else {
        Err("Keiki stream closed before the turn finished".into())
    }
}
