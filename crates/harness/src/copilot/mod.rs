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
    let prompts = interrupts.iter().map(interrupt_prompt).collect::<Vec<_>>();
    let questions = prompts
        .iter()
        .map(|prompt| prompt.question.clone())
        .collect::<Vec<_>>();
    let receiver = request_input(questions);
    let answers = tokio::select! {
        _ = cancellation.cancelled() => return None,
        answers = receiver => answers.unwrap_or_default(),
    };
    let resume = prompts
        .into_iter()
        .map(|prompt| {
            let answer = answers
                .iter()
                .find(|answer| answer.question_id == prompt.question.id)
                .and_then(|answer| answer.labels.first());
            let (status, payload) = match prompt.answer.resume(answer) {
                Some(payload) => (ResumeStatus::Resolved, Some(payload)),
                None => (ResumeStatus::Cancelled, None),
            };
            ResumeEntry {
                interrupt_id: prompt.question.id,
                status,
                payload,
            }
        })
        .collect();
    Some(ResumePayload { resume })
}

/// How an interrupt's answer becomes its TanStack resume payload.
#[derive(Debug, Clone, PartialEq)]
enum InterruptAnswer {
    /// `needsApproval` tool: `{ approved: bool }`.
    Approval,
    /// The copilot's `ask_choice` client tool: its output `{ chosen }` is the
    /// picked option's id (label → id), or the typed text verbatim.
    Choice { ids_by_label: Vec<(String, String)> },
}

impl InterruptAnswer {
    fn resume(&self, label: Option<&String>) -> Option<Value> {
        match self {
            Self::Approval => match label.map(|label| label.to_ascii_lowercase()) {
                Some(label)
                    if matches!(label.as_str(), "approve" | "approved" | "yes" | "allow") =>
                {
                    Some(json!({ "approved": true }))
                }
                Some(label)
                    if matches!(
                        label.as_str(),
                        "decline" | "declined" | "reject" | "rejected" | "no"
                    ) =>
                {
                    Some(json!({ "approved": false }))
                }
                _ => None,
            },
            Self::Choice { ids_by_label } => {
                let label = label.filter(|label| !label.trim().is_empty())?;
                let chosen = ids_by_label
                    .iter()
                    .find(|(candidate, _)| candidate == label)
                    .map(|(_, id)| id.clone())
                    .unwrap_or_else(|| label.clone());
                Some(json!({ "chosen": chosen }))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct InterruptPrompt {
    question: UserInputQuestion,
    answer: InterruptAnswer,
}

/// Longest tool input shown in an approval card before it is cut.
const MAX_APPROVAL_INPUT_CHARS: usize = 4_000;

/// The question the user sees for an interrupt. TanStack's `message` is a
/// display hint ("Client tool ask_choice is ready to run"); the tool call in
/// `metadata` carries what the user is actually being asked.
fn interrupt_prompt(interrupt: &Interrupt) -> InterruptPrompt {
    let metadata = interrupt.metadata.as_ref().and_then(Value::as_object);
    let kind = metadata
        .and_then(|m| m.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = metadata
        .and_then(|m| m.get("toolName"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = metadata.and_then(|m| m.get("input"));
    let message = interrupt
        .message
        .clone()
        .unwrap_or_else(|| interrupt.reason.clone());

    if kind == "client_tool"
        && let Some(prompt) = choice_prompt(interrupt, input)
    {
        return prompt;
    }

    let (header, question) = if kind == "approval" && !tool_name.is_empty() {
        let body = input
            .and_then(approval_input_text)
            .unwrap_or_else(|| message.clone());
        (format!("Approve {tool_name}"), body)
    } else if interrupt.reason.is_empty() {
        ("Copilot approval".to_owned(), message)
    } else {
        (interrupt.reason.clone(), message)
    };
    InterruptPrompt {
        question: UserInputQuestion {
            id: interrupt.id.clone(),
            header,
            question,
            options: vec!["Approve".into(), "Decline".into()],
            multi_select: false,
        },
        answer: InterruptAnswer::Approval,
    }
}

/// `ask_choice` input: `{ question, options: [{ id, label, detail? }] }`.
fn choice_prompt(interrupt: &Interrupt, input: Option<&Value>) -> Option<InterruptPrompt> {
    let input = input?.as_object()?;
    let question = input.get("question")?.as_str()?.trim();
    if question.is_empty() {
        return None;
    }
    let ids_by_label = input
        .get("options")?
        .as_array()?
        .iter()
        .map(|option| {
            let option = option.as_object()?;
            let label = option.get("label")?.as_str()?.trim();
            if label.is_empty() {
                return None;
            }
            let id = option
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or(label);
            let detail = option
                .get("detail")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|detail| !detail.is_empty());
            let shown = match detail {
                Some(detail) => format!("{label} — {detail}"),
                None => label.to_owned(),
            };
            Some((shown, id.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    if ids_by_label.is_empty() {
        return None;
    }
    Some(InterruptPrompt {
        question: UserInputQuestion {
            id: interrupt.id.clone(),
            header: "Question".into(),
            question: question.to_owned(),
            options: ids_by_label
                .iter()
                .map(|(label, _)| label.clone())
                .collect(),
            multi_select: false,
        },
        answer: InterruptAnswer::Choice { ids_by_label },
    })
}

/// The reviewable text of an approval-gated call: a single string argument
/// (e.g. `execute_typescript_with_approval`'s program) verbatim, anything
/// else as pretty JSON.
fn approval_input_text(input: &Value) -> Option<String> {
    let text = match input {
        Value::String(text) => text.clone(),
        Value::Object(fields) => {
            let mut strings = fields.values().filter_map(Value::as_str);
            match (strings.next(), strings.next(), fields.len()) {
                (Some(only), None, 1) => only.to_owned(),
                _ => serde_json::to_string_pretty(input).ok()?,
            }
        }
        Value::Null => return None,
        other => serde_json::to_string_pretty(other).ok()?,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut cut = text
        .chars()
        .take(MAX_APPROVAL_INPUT_CHARS)
        .collect::<String>();
    if cut.len() < text.len() {
        cut.push('…');
    }
    Some(cut)
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

#[cfg(test)]
mod interrupt_prompt_tests {
    use super::*;

    fn interrupt(reason: &str, message: &str, metadata: Value) -> Interrupt {
        Interrupt {
            id: "i1".into(),
            reason: reason.into(),
            message: Some(message.into()),
            metadata: Some(metadata),
        }
    }

    #[test]
    fn ask_choice_shows_the_question_and_resumes_with_the_option_id() {
        let prompt = interrupt_prompt(&interrupt(
            "tanstack:client_tool_execution",
            "Client tool ask_choice is ready to run",
            json!({
                "kind": "client_tool",
                "toolName": "ask_choice",
                "input": {
                    "question": "Which agent should get the new line?",
                    "options": [
                        {"id": "support", "label": "Support agent", "detail": "Routes to the support inbox"},
                        {"id": "sales", "label": "Sales agent"}
                    ]
                }
            }),
        ));
        assert_eq!(prompt.question.header, "Question");
        assert_eq!(
            prompt.question.question,
            "Which agent should get the new line?"
        );
        assert_eq!(
            prompt.question.options,
            vec!["Support agent — Routes to the support inbox", "Sales agent"]
        );
        assert_eq!(
            prompt.answer.resume(Some(&"Sales agent".to_owned())),
            Some(json!({ "chosen": "sales" }))
        );
        assert_eq!(
            prompt
                .answer
                .resume(Some(&"neither, use the default".to_owned())),
            Some(json!({ "chosen": "neither, use the default" }))
        );
        assert_eq!(prompt.answer.resume(None), None);
    }

    #[test]
    fn approval_shows_the_program_under_the_tool_name() {
        let prompt = interrupt_prompt(&interrupt(
            "tool_call",
            "Approval required to run execute_typescript_with_approval",
            json!({
                "kind": "approval",
                "toolName": "execute_typescript_with_approval",
                "input": {"typescriptCode": "await deleteAgent('a1')"}
            }),
        ));
        assert_eq!(
            prompt.question.header,
            "Approve execute_typescript_with_approval"
        );
        assert_eq!(prompt.question.question, "await deleteAgent('a1')");
        assert_eq!(prompt.question.options, vec!["Approve", "Decline"]);
        assert_eq!(
            prompt.answer.resume(Some(&"Approve".to_owned())),
            Some(json!({ "approved": true }))
        );
        assert_eq!(
            prompt.answer.resume(Some(&"Decline".to_owned())),
            Some(json!({ "approved": false }))
        );
    }

    #[test]
    fn multi_field_approval_input_is_pretty_json() {
        let prompt = interrupt_prompt(&interrupt(
            "tool_call",
            "Approval required to run publish_tool",
            json!({
                "kind": "approval",
                "toolName": "publish_tool",
                "input": {"name": "lookup", "version": 2}
            }),
        ));
        assert_eq!(prompt.question.header, "Approve publish_tool");
        assert!(prompt.question.question.contains("\"name\": \"lookup\""));
        assert!(prompt.question.question.contains("\"version\": 2"));
    }

    #[test]
    fn interrupts_without_metadata_keep_the_message() {
        let prompt = interrupt_prompt(&Interrupt {
            id: "i1".into(),
            reason: "Approve the action".into(),
            message: Some("May Copilot continue?".into()),
            metadata: None,
        });
        assert_eq!(prompt.question.header, "Approve the action");
        assert_eq!(prompt.question.question, "May Copilot continue?");
        assert_eq!(prompt.answer, InterruptAnswer::Approval);
    }
}
