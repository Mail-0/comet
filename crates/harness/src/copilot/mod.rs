//! Dashboard Copilot harness over the authenticated AG-UI HTTP stream.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use zeron_copilot::{
    AgUiEvent, Client, CopilotCredentials, Error as CopilotError, Interrupt, ResumeEntry,
    ResumePayload, ResumeStatus, SseDecoder, TurnMapper,
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls, SteerMessage};

/// Credential access is deliberately abstracted at the harness boundary so the
/// engine can own the device-local holder without introducing a dependency cycle.
pub trait CopilotCredentialSource: Send + Sync {
    fn snapshot(&self) -> Option<CopilotCredentials>;
}

pub struct CopilotHarness {
    credentials: Arc<dyn CopilotCredentialSource>,
}

impl CopilotHarness {
    pub fn new(credentials: Arc<dyn CopilotCredentialSource>) -> Self {
        Self { credentials }
    }
}

#[async_trait]
impl Harness for CopilotHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Copilot
    }

    fn display_name(&self) -> &str {
        "Copilot"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    fn installed(&self) -> bool {
        self.credentials.snapshot().is_some()
    }

    fn deterministic_turn_end(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![Model {
            id: "copilot".into(),
            label: "Copilot".into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }])
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if !self.installed() {
            return Err(HarnessError::NotInstalled(
                "Copilot credentials are unavailable".into(),
            ));
        }

        let credentials = Arc::clone(&self.credentials);
        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(async move {
            run_copilot(credentials, request, controls, event_tx).await;
        });
        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatRequest {
    thread_id: String,
    run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume: Option<Vec<ResumeEntry>>,
    messages: Vec<Value>,
}

fn chat_request(
    thread_id: &str,
    run_id: &str,
    parent_run_id: Option<String>,
    resume: Option<ResumePayload>,
    messages: Vec<Value>,
) -> ChatRequest {
    ChatRequest {
        thread_id: thread_id.to_owned(),
        run_id: run_id.to_owned(),
        parent_run_id,
        resume: resume.map(|payload| payload.resume),
        messages,
    }
}

async fn chat_request_with_history(
    client: &Client,
    credentials: &Arc<dyn CopilotCredentialSource>,
    thread_id: &str,
    run_id: &str,
    parent_run_id: Option<String>,
    resume: Option<ResumePayload>,
    prompt: Option<String>,
) -> Result<ChatRequest, String> {
    let current = credentials
        .snapshot()
        .ok_or_else(|| "Copilot credentials are unavailable".to_owned())?;
    let mut messages = match client.get_thread(&current, thread_id).await {
        Ok(thread) => thread
            .messages
            .as_array()
            .cloned()
            .ok_or_else(|| "Copilot thread history was not an array".to_owned())?,
        Err(CopilotError::Api { status, .. }) if status == reqwest::StatusCode::NOT_FOUND => {
            Vec::new()
        }
        Err(CopilotError::Unauthorized) => {
            return Err("Copilot authorization expired".into());
        }
        Err(error) => {
            return Err(format!("Copilot thread history failed: {error}"));
        }
    };
    if let Some(prompt) = prompt {
        messages.push(json!({ "role": "user", "content": prompt }));
    } else if messages.is_empty() {
        return Err("Copilot cannot resume an empty thread".into());
    }
    Ok(chat_request(
        thread_id,
        run_id,
        parent_run_id,
        resume,
        messages,
    ))
}

async fn run_copilot(
    credentials: Arc<dyn CopilotCredentialSource>,
    request: RunRequest,
    controls: RunControls,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
) {
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input: Arc<
        dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
            + Send
            + Sync,
    > = Arc::from(request_input);
    let client = credentials
        .snapshot()
        .map(|current| Client::new(current.base_url));
    let Some(client) = client else {
        send_done_error(&event_tx, "Copilot credentials are unavailable").await;
        return;
    };
    let thread_id = request
        .resume
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut pending_steers = VecDeque::new();
    let mut steering_closed = false;
    let mut next_body = match chat_request_with_history(
        &client,
        &credentials,
        &thread_id,
        &Uuid::new_v4().to_string(),
        None,
        None,
        Some(request.prompt),
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            send_done_error(&event_tx, &error).await;
            return;
        }
    };

    'session: loop {
        let Some(current) = credentials.snapshot() else {
            send_done_error(&event_tx, "Copilot credentials are unavailable").await;
            return;
        };
        let response = match client.post_chat(&current, &next_body).await {
            Ok(response) => response,
            Err(CopilotError::Unauthorized) => {
                send_done_error(&event_tx, "Copilot authorization expired").await;
                return;
            }
            Err(error) => {
                send_done_error(&event_tx, &error.to_string()).await;
                return;
            }
        };

        let result = consume_response(
            &client,
            &credentials,
            response,
            &interrupt,
            &mut steering,
            &mut pending_steers,
            &event_tx,
        )
        .await;
        match result {
            ConsumeResult::Done => {
                let steer = if let Some(steer) = pending_steers.pop_front() {
                    Some(steer)
                } else {
                    tokio::select! {
                        _ = interrupt.cancelled() => {
                            send_done_interrupted(&event_tx).await;
                            break 'session;
                        }
                        steer = steering.recv(), if !steering_closed => {
                            match steer {
                                Some(steer) => Some(steer),
                                None => {
                                    steering_closed = true;
                                    None
                                }
                            }
                        }
                    }
                };
                let Some(steer) = steer else {
                    break 'session;
                };
                let next = Uuid::new_v4().to_string();
                if event_tx
                    .send(Ok(AgentEvent::Steered {
                        assistant_message_id: None,
                        next_assistant_message_id: Some(next.clone()),
                    }))
                    .await
                    .is_err()
                {
                    break 'session;
                }
                next_body = match chat_request_with_history(
                    &client,
                    &credentials,
                    &thread_id,
                    &next,
                    None,
                    None,
                    Some(steer.prompt),
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => {
                        send_done_error(&event_tx, &error).await;
                        break 'session;
                    }
                };
            }
            ConsumeResult::Interrupt { run_id, interrupts } => {
                let resume = await_interrupts(request_input.clone(), interrupts, &interrupt).await;
                let Some(resume) = resume else {
                    send_done_interrupted(&event_tx).await;
                    break 'session;
                };
                pending_steers.clear();
                next_body = match chat_request_with_history(
                    &client,
                    &credentials,
                    &thread_id,
                    &Uuid::new_v4().to_string(),
                    Some(run_id),
                    Some(resume),
                    None,
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => {
                        send_done_error(&event_tx, &error).await;
                        break 'session;
                    }
                };
            }
            ConsumeResult::Stopped => break 'session,
        }
    }
}

enum ConsumeResult {
    Done,
    Interrupt {
        run_id: String,
        interrupts: Vec<Interrupt>,
    },
    Stopped,
}

async fn consume_response(
    client: &Client,
    credentials: &Arc<dyn CopilotCredentialSource>,
    response: reqwest::Response,
    interrupt: &CancellationToken,
    steering: &mut mpsc::Receiver<SteerMessage>,
    pending_steers: &mut VecDeque<SteerMessage>,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> ConsumeResult {
    let mut decoder = SseDecoder::new();
    let mut mapper = TurnMapper::new();
    let mut run_id = String::new();
    let mut steering_closed = false;
    let mut body = response.bytes_stream();
    loop {
        tokio::select! {
            _ = interrupt.cancelled() => {
                if !run_id.is_empty() && let Some(current) = credentials.snapshot() {
                    let _ = client.cancel_run(&current, &run_id).await;
                }
                send_done_interrupted(event_tx).await;
                return ConsumeResult::Stopped;
            }
            steer = steering.recv(), if !steering_closed => {
                match steer {
                    Some(steer) => pending_steers.push_back(steer),
                    None => steering_closed = true,
                }
            }
            chunk = body.next() => {
                let Some(chunk) = chunk else {
                    if let Ok(Some(frame)) = decoder.finish()
                        && let Ok(event) = frame.ag_ui_event()
                        && let Some(result) =
                            handle_event(event, &mut mapper, &mut run_id, event_tx).await
                    {
                        return result;
                    }
                    send_done_error(event_tx, "Copilot stream ended without completion").await;
                    return ConsumeResult::Stopped;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        send_done_error(event_tx, &error.to_string()).await;
                        return ConsumeResult::Stopped;
                    }
                };
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        send_done_error(event_tx, &error.to_string()).await;
                        return ConsumeResult::Stopped;
                    }
                };
                for frame in frames {
                    let event = match frame.ag_ui_event() {
                        Ok(event) => event,
                        Err(error) => {
                            send_done_error(event_tx, &error.to_string()).await;
                            return ConsumeResult::Stopped;
                        }
                    };
                    if let Some(result) = handle_event(
                        event, &mut mapper, &mut run_id, event_tx
                    ).await {
                        return result;
                    }
                }
            }
        }
    }
}

async fn handle_event(
    event: AgUiEvent,
    mapper: &mut TurnMapper,
    run_id: &mut String,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Option<ConsumeResult> {
    if let AgUiEvent::RunStarted {
        run_id: started_run_id,
        ..
    } = &event
    {
        *run_id = started_run_id.clone();
    }
    let mapped = mapper.handle(event);
    let is_done = mapped
        .iter()
        .any(|event| matches!(event, AgentEvent::Done { .. }));
    for event in mapped {
        if event_tx.send(Ok(event)).await.is_err() {
            return Some(ConsumeResult::Stopped);
        }
    }
    let interrupts = mapper.take_interrupts();
    if !interrupts.is_empty() {
        return Some(ConsumeResult::Interrupt {
            run_id: run_id.clone(),
            interrupts,
        });
    }
    if is_done {
        return Some(ConsumeResult::Done);
    }
    None
}

async fn await_interrupts(
    request_input: Arc<
        dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
            + Send
            + Sync,
    >,
    interrupts: Vec<Interrupt>,
    cancellation: &CancellationToken,
) -> Option<ResumePayload> {
    let questions = interrupts
        .iter()
        .map(|interrupt| UserInputQuestion {
            id: interrupt.id.clone(),
            header: if interrupt.reason.is_empty() {
                "Copilot approval".into()
            } else {
                interrupt.reason.clone()
            },
            question: interrupt
                .message
                .clone()
                .unwrap_or_else(|| interrupt.reason.clone()),
            options: vec!["Approve".into(), "Decline".into()],
            multi_select: false,
        })
        .collect::<Vec<_>>();
    let receiver = request_input(questions);
    let answers = tokio::select! {
        _ = cancellation.cancelled() => return None,
        answers = receiver => answers.unwrap_or_default(),
    };
    let resume = interrupts
        .into_iter()
        .map(|interrupt| {
            let answer = answers
                .iter()
                .find(|answer| answer.question_id == interrupt.id)
                .and_then(|answer| answer.labels.first());
            let (status, payload) = match answer.map(|label| label.to_ascii_lowercase()) {
                Some(label)
                    if matches!(label.as_str(), "approve" | "approved" | "yes" | "allow") =>
                {
                    (ResumeStatus::Resolved, Some(json!({ "approved": true })))
                }
                Some(label)
                    if matches!(
                        label.as_str(),
                        "decline" | "declined" | "reject" | "rejected" | "no"
                    ) =>
                {
                    (ResumeStatus::Resolved, Some(json!({ "approved": false })))
                }
                _ => (ResumeStatus::Cancelled, None),
            };
            ResumeEntry {
                interrupt_id: interrupt.id,
                status,
                payload,
            }
        })
        .collect();
    Some(ResumePayload { resume })
}

async fn send_done_error(event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, message: &str) {
    let _ = event_tx
        .send(Ok(AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(message.to_owned()),
            session_id: None,
        }))
        .await;
}

async fn send_done_interrupted(event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>) {
    let _ = event_tx
        .send(Ok(AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: None,
        }))
        .await;
}
