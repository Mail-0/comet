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
    AgUiEvent, Client, CopilotCredentials, Error as CopilotError, Interrupt, PendingInterrupts,
    ResumeEntry, ResumePayload, ResumeStatus, SseDecoder, TurnMapper,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    append_to_transcript: Option<bool>,
}

fn chat_request(
    thread_id: &str,
    run_id: &str,
    parent_run_id: Option<String>,
    resume: Option<ResumePayload>,
    messages: Vec<Value>,
    append_to_transcript: bool,
) -> ChatRequest {
    ChatRequest {
        thread_id: thread_id.to_owned(),
        run_id: run_id.to_owned(),
        parent_run_id,
        resume: resume.map(|payload| payload.resume),
        messages,
        append_to_transcript: append_to_transcript.then_some(true),
    }
}

async fn pending_interrupts(
    client: &Client,
    credentials: &Arc<dyn CopilotCredentialSource>,
    thread_id: &str,
) -> Result<Option<PendingInterrupts>, String> {
    let current = credentials
        .snapshot()
        .ok_or_else(|| "Copilot credentials are unavailable".to_owned())?;
    match client.get_chat_thread(&current, thread_id).await {
        Ok(state) => Ok(state
            .interrupts
            .filter(|interrupts| !interrupts.pending.is_empty())),
        Err(CopilotError::Unauthorized) => Err("Copilot authorization expired".into()),
        Err(error) => Err(format!("Copilot thread hydration failed: {error}")),
    }
}

fn normal_chat_request(thread_id: &str, run_id: &str, prompt: String) -> ChatRequest {
    chat_request(
        thread_id,
        run_id,
        None,
        None,
        vec![json!({ "role": "user", "content": prompt })],
        true,
    )
}

fn resume_chat_request(
    thread_id: &str,
    run_id: &str,
    parent_run_id: String,
    resume: ResumePayload,
) -> ChatRequest {
    chat_request(
        thread_id,
        run_id,
        Some(parent_run_id),
        Some(resume),
        Vec::new(),
        false,
    )
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
    let mut prompt_after_resume = None;
    let mut next_body;
    match pending_interrupts(&client, &credentials, &thread_id).await {
        Ok(Some(interrupts)) => {
            let resume =
                await_interrupts(request_input.clone(), interrupts.pending, &interrupt).await;
            let Some(resume) = resume else {
                send_done_interrupted(&event_tx).await;
                return;
            };
            prompt_after_resume = Some(request.prompt);
            next_body = resume_chat_request(
                &thread_id,
                &Uuid::new_v4().to_string(),
                interrupts.run_id,
                resume,
            );
        }
        Ok(None) => {
            next_body =
                normal_chat_request(&thread_id, &Uuid::new_v4().to_string(), request.prompt);
        }
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

        let request_run_id = next_body.run_id.clone();
        let result = consume_response(
            &client,
            &credentials,
            response,
            &request_run_id,
            &interrupt,
            &mut steering,
            &mut pending_steers,
            &event_tx,
        )
        .await;
        match result {
            ConsumeResult::Done(done) => {
                let pending = match pending_interrupts(&client, &credentials, &thread_id).await {
                    Ok(pending) => pending,
                    Err(error) => {
                        send_done_error(&event_tx, &error).await;
                        break 'session;
                    }
                };
                if let Some(interrupts) = pending {
                    let resume =
                        await_interrupts(request_input.clone(), interrupts.pending, &interrupt)
                            .await;
                    let Some(resume) = resume else {
                        send_done_interrupted(&event_tx).await;
                        break 'session;
                    };
                    pending_steers.clear();
                    next_body = resume_chat_request(
                        &thread_id,
                        &Uuid::new_v4().to_string(),
                        interrupts.run_id,
                        resume,
                    );
                    continue;
                }
                if let Some(prompt) = prompt_after_resume.take() {
                    next_body =
                        normal_chat_request(&thread_id, &Uuid::new_v4().to_string(), prompt);
                    continue;
                }
                if event_tx.send(Ok(done)).await.is_err() {
                    break 'session;
                }
                let steer = if let Some(steer) = pending_steers.pop_front() {
                    Some(steer)
                } else if steering_closed {
                    None
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
                next_body = normal_chat_request(&thread_id, &next, steer.prompt);
            }
            ConsumeResult::Interrupt { interrupts } => {
                let resume = await_interrupts(request_input.clone(), interrupts, &interrupt).await;
                let Some(resume) = resume else {
                    send_done_interrupted(&event_tx).await;
                    break 'session;
                };
                pending_steers.clear();
                next_body = resume_chat_request(
                    &thread_id,
                    &Uuid::new_v4().to_string(),
                    request_run_id,
                    resume,
                );
            }
            ConsumeResult::Stopped => break 'session,
        }
    }
}

enum ConsumeResult {
    Done(AgentEvent),
    Interrupt { interrupts: Vec<Interrupt> },
    Stopped,
}

async fn consume_response(
    client: &Client,
    credentials: &Arc<dyn CopilotCredentialSource>,
    response: reqwest::Response,
    request_run_id: &str,
    interrupt: &CancellationToken,
    steering: &mut mpsc::Receiver<SteerMessage>,
    pending_steers: &mut VecDeque<SteerMessage>,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> ConsumeResult {
    let mut decoder = SseDecoder::new();
    let mut mapper = TurnMapper::new();
    let mut steering_closed = false;
    let mut body = response.bytes_stream();
    loop {
        tokio::select! {
            _ = interrupt.cancelled() => {
                if let Some(current) = credentials.snapshot() {
                    let _ = client.cancel_run(&current, request_run_id).await;
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
                            handle_event(event, &mut mapper, event_tx).await
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
                        event, &mut mapper, event_tx
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
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Option<ConsumeResult> {
    let mapped = mapper.handle(event);
    let done = mapped.iter().find_map(|event| match event {
        AgentEvent::Done { .. } => Some(event.clone()),
        _ => None,
    });
    for event in mapped {
        if matches!(event, AgentEvent::Done { .. }) {
            continue;
        }
        if event_tx.send(Ok(event)).await.is_err() {
            return Some(ConsumeResult::Stopped);
        }
    }
    let interrupts = mapper.take_interrupts();
    if !interrupts.is_empty() {
        return Some(ConsumeResult::Interrupt { interrupts });
    }
    if let Some(done) = done {
        return Some(ConsumeResult::Done(done));
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
