use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use zeron_copilot::CopilotCredentials;
use zeron_harness::{
    CancellationToken, CopilotCredentialSource, CopilotHarness, Harness, RunControls, SteerMessage,
};
use zeron_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion};

#[derive(Clone)]
struct TestCredentials(CopilotCredentials);

impl CopilotCredentialSource for TestCredentials {
    fn snapshot(&self) -> Option<CopilotCredentials> {
        Some(self.0.clone())
    }
}

#[derive(Clone)]
struct FakeCopilot {
    base: String,
    interrupt_first: Arc<AtomicBool>,
    pending_before_first: Arc<AtomicBool>,
    pending_after_first: Arc<AtomicBool>,
    hold_until_cancel: Arc<AtomicBool>,
    posts: Arc<Mutex<Vec<Value>>>,
    gets: Arc<Mutex<Vec<String>>>,
    cancellations: Arc<Mutex<Vec<String>>>,
}

impl FakeCopilot {
    async fn start(_history: Option<Vec<Value>>, interrupt_first: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            interrupt_first: Arc::new(AtomicBool::new(interrupt_first)),
            pending_before_first: Arc::new(AtomicBool::new(false)),
            pending_after_first: Arc::new(AtomicBool::new(false)),
            hold_until_cancel: Arc::new(AtomicBool::new(false)),
            posts: Arc::new(Mutex::new(Vec::new())),
            gets: Arc::new(Mutex::new(Vec::new())),
            cancellations: Arc::new(Mutex::new(Vec::new())),
        };
        let accept = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let fake = accept.clone();
                tokio::spawn(async move {
                    fake.serve(stream).await;
                });
            }
        });
        fake
    }

    fn posts(&self) -> Vec<Value> {
        self.posts.lock().unwrap().clone()
    }

    fn gets(&self) -> Vec<String> {
        self.gets.lock().unwrap().clone()
    }

    fn cancellations(&self) -> Vec<String> {
        self.cancellations.lock().unwrap().clone()
    }

    fn with_pending_after_first(self) -> Self {
        self.pending_before_first.store(false, Ordering::Relaxed);
        self.pending_after_first.store(true, Ordering::Relaxed);
        self
    }

    fn with_pending_before_first(self) -> Self {
        self.pending_before_first.store(true, Ordering::Relaxed);
        self
    }

    fn with_hold_until_cancel(self) -> Self {
        self.hold_until_cancel.store(true, Ordering::Relaxed);
        self
    }

    async fn serve(&self, mut stream: tokio::net::TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let request_line = head.lines().next().unwrap_or_default();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .unwrap_or(Value::Null);
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default();
        let target = request_parts.next().unwrap_or_default();
        let path = target.split('?').next().unwrap_or_default();

        let (status, content_type, response) = if method == "GET" && path.ends_with("/chat") {
            self.gets.lock().unwrap().push(target.to_owned());
            let pending = (self.pending_before_first.load(Ordering::Relaxed)
                && self.posts().is_empty())
                || (self.pending_after_first.load(Ordering::Relaxed) && self.posts().len() == 1);
            (
                "200 OK",
                "application/json",
                json!({
                    "messages": [],
                    "activeRun": null,
                    "interrupts": pending.then(|| json!({
                        "runId": "durable-parent",
                        "pending": [{
                            "id": "interrupt-1",
                            "reason": "Approve the action",
                            "message": "May Copilot continue?"
                        }]
                    }))
                })
                .to_string(),
            )
        } else if method == "POST" && path.ends_with("/chat") {
            self.posts.lock().unwrap().push(body);
            let post_index = self.posts.lock().unwrap().len() - 1;
            let events = if self.interrupt_first.load(Ordering::Relaxed) && post_index == 0 {
                vec![
                    json!({"type":"RUN_STARTED","threadId":"thread","runId":"run-1"}),
                    json!({
                        "type":"RUN_FINISHED",
                        "runId":"run-1",
                        "outcome": {
                            "type":"interrupt",
                            "interrupts": [{
                                "id":"interrupt-1",
                                "reason":"Approve the action",
                                "message":"May Copilot continue?"
                            }]
                        }
                    }),
                ]
            } else {
                let run_id = if self.hold_until_cancel.load(Ordering::Relaxed) {
                    "provider-run".to_owned()
                } else {
                    format!("run-{}", post_index + 1)
                };
                vec![
                    json!({"type":"RUN_STARTED","threadId":"thread","runId":run_id}),
                    json!({"type":"RUN_FINISHED","runId":run_id}),
                ]
            };
            let response = events
                .into_iter()
                .map(|event| format!("data: {}\n\n", event))
                .collect::<String>();
            ("200 OK", "text/event-stream", response)
        } else if method == "POST" && path.contains("/runs/") && path.ends_with("/cancel") {
            self.cancellations.lock().unwrap().push(path.to_owned());
            ("200 OK", "application/json", "{}".into())
        } else {
            ("404 Not Found", "application/json", "{}".into())
        };
        let headers = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n",
            response.len()
        );
        let _ = stream.write_all(headers.as_bytes()).await;
        if method == "POST"
            && path.ends_with("/chat")
            && self.hold_until_cancel.load(Ordering::Relaxed)
        {
            let first_event = response
                .split_once("data: ")
                .and_then(|(_, rest)| rest.split_once("\n\ndata: "))
                .map(|(_, _)| {
                    let end = response.find("\n\ndata: ").unwrap_or(response.len());
                    &response[..end + 2]
                })
                .unwrap_or(&response);
            let _ = stream.write_all(first_event.as_bytes()).await;
            for _ in 0..100 {
                if !self.cancellations.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } else {
            let _ = stream.write_all(response.as_bytes()).await;
        }
    }
}

fn request(prompt: &str, resume: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("copilot".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: resume.map(str::to_owned),
    }
}

fn controls(answer: Option<&str>) -> RunControls {
    controls_with_observer(answer, None)
}

fn controls_with_observer(
    answer: Option<&str>,
    observed: Option<Arc<Mutex<Vec<Vec<UserInputQuestion>>>>>,
) -> RunControls {
    let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(1);
    drop(steer_tx);
    let answer = answer.map(str::to_owned);
    RunControls {
        request_input: Box::new(move |questions| {
            if let Some(observed) = &observed {
                observed.lock().unwrap().push(questions.clone());
            }
            let (tx, rx) = oneshot::channel();
            let answers = answer.as_ref().map(|label| {
                questions
                    .iter()
                    .map(|question| UserInputAnswer {
                        question_id: question.id.clone(),
                        labels: vec![label.clone()],
                    })
                    .collect()
            });
            let _ = tx.send(answers.unwrap_or_default());
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    }
}

async fn run_to_end(harness: &CopilotHarness, request: RunRequest, controls: RunControls) {
    let stream = harness.run(request, controls).await.unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        stream.map(|event| event.unwrap()).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::Done { .. })));
}

fn harness(fake: &FakeCopilot) -> CopilotHarness {
    CopilotHarness::new(Arc::new(TestCredentials(CopilotCredentials {
        base_url: fake.base.clone(),
        access_token: "test-token".into(),
    })))
}

#[tokio::test]
async fn normal_turn_uses_server_append_without_history_fetch() {
    let fake = FakeCopilot::start(None, false).await;
    let copilot = harness(&fake);

    run_to_end(&copilot, request("first", Some("thread")), controls(None)).await;

    let posts = fake.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["messages"],
        json!([{"role":"user","content":"first"}])
    );
    assert_eq!(posts[0]["appendToTranscript"], true);
    assert!(fake
        .gets()
        .iter()
        .all(|target| !target.contains("/threads/")));
}

#[tokio::test]
async fn a_second_turn_posts_only_the_new_message_for_server_append() {
    let fake = FakeCopilot::start(None, false).await;
    let copilot = harness(&fake);

    run_to_end(&copilot, request("first", Some("thread")), controls(None)).await;
    run_to_end(&copilot, request("second", Some("thread")), controls(None)).await;

    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        posts[1]["messages"],
        json!([{"role":"user","content":"second"}])
    );
    assert_eq!(posts[1]["appendToTranscript"], true);
}

#[tokio::test]
async fn a_thread_with_pending_interrupts_resumes_before_new_input() {
    let fake = FakeCopilot::start(None, false)
        .await
        .with_pending_before_first();
    let copilot = harness(&fake);

    run_to_end(
        &copilot,
        request("new question", Some("thread")),
        controls(Some("Approve")),
    )
    .await;

    let posts = fake.posts();
    assert_eq!(posts[0]["messages"], json!([]));
    assert_eq!(posts[0]["parentRunId"], "durable-parent");
    assert_eq!(posts[0]["resume"][0]["payload"], json!({"approved": true}));
    assert_eq!(
        posts[1]["messages"],
        json!([{"role":"user","content":"new question"}])
    );
}

#[tokio::test]
async fn completed_stream_hydrates_pending_interrupts_before_done() {
    let fake = FakeCopilot::start(None, false)
        .await
        .with_pending_after_first();
    let copilot = harness(&fake);
    let questions = Arc::new(Mutex::new(Vec::new()));

    run_to_end(
        &copilot,
        request("question", Some("thread")),
        controls_with_observer(Some("Approve"), Some(questions.clone())),
    )
    .await;

    let questions = questions.lock().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0][0].id, "interrupt-1");
    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1]["messages"], json!([]));
    assert_eq!(posts[1]["parentRunId"], "durable-parent");
    assert_eq!(
        posts[1]["resume"],
        json!([{
            "interruptId":"interrupt-1",
            "status":"resolved",
            "payload":{"approved":true}
        }])
    );
    assert!(posts[1].get("appendToTranscript").is_none());
}

#[tokio::test]
async fn interrupt_decline_sends_a_negative_approval_payload() {
    let fake = FakeCopilot::start(None, true).await;
    let copilot = harness(&fake);

    run_to_end(
        &copilot,
        request("question", Some("thread")),
        controls(Some("Decline")),
    )
    .await;

    let posts = fake.posts();
    assert_eq!(posts[1]["resume"][0]["status"], "resolved");
    assert_eq!(posts[1]["resume"][0]["payload"], json!({"approved": false}));
    assert_eq!(posts[1]["messages"], json!([]));
    assert!(posts[1].get("appendToTranscript").is_none());
}

#[tokio::test]
async fn cancellation_uses_the_minted_request_run_id() {
    let fake = FakeCopilot::start(None, false)
        .await
        .with_hold_until_cancel();
    let copilot = harness(&fake);
    let token = CancellationToken::new();
    let cancel = token.clone();
    let controls = {
        let (_steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(1);
        RunControls {
            request_input: Box::new(|_| {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Vec::new());
                rx
            }),
            steering: steer_rx,
            interrupt: token,
        }
    };
    let stream = copilot
        .run(request("stop", Some("thread")), controls)
        .await
        .unwrap();
    let join = tokio::spawn(stream.map(|event| event.unwrap()).collect::<Vec<_>>());
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .unwrap();
    let cancellations = fake.cancellations();
    assert_eq!(cancellations.len(), 1);
    let posts = fake.posts();
    let minted_run_id = posts[0]["runId"].as_str().unwrap();
    assert_eq!(
        cancellations[0],
        format!("/api/copilot/runs/{minted_run_id}/cancel")
    );
    assert!(!cancellations[0].contains("provider-run"));
}

#[tokio::test]
async fn text_before_tool_call_is_kept_as_assistant_text() {
    let mapper_events = [
        zeron_copilot::AgUiEvent::RunStarted {
            thread_id: "thread".into(),
            run_id: "provider".into(),
        },
        zeron_copilot::AgUiEvent::TextMessageStart {
            message_id: "assistant".into(),
        },
        zeron_copilot::AgUiEvent::TextMessageContent {
            message_id: "assistant".into(),
            delta: "Before approval".into(),
        },
        zeron_copilot::AgUiEvent::ToolCallStart {
            tool_call_id: "call".into(),
            tool_call_name: "delete".into(),
            parent_message_id: Some("assistant".into()),
        },
        zeron_copilot::AgUiEvent::ToolCallEnd {
            tool_call_id: "call".into(),
            input: Some(json!({})),
        },
        zeron_copilot::AgUiEvent::RunFinished {
            run_id: "provider".into(),
            outcome: None,
            result: None,
        },
    ];
    let mut mapper = zeron_copilot::TurnMapper::new();
    let mapped = mapper_events
        .into_iter()
        .flat_map(|event| mapper.handle(event))
        .collect::<Vec<_>>();
    assert!(mapped.iter().any(|event| matches!(
        event,
        AgentEvent::TextDelta { text } if text == "Before approval"
    )));
    assert!(mapped.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCall { id, .. } if id == "call"
    )));
}
