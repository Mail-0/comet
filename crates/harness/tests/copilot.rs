use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use zeron_copilot::CopilotCredentials;
use zeron_harness::{
    CancellationToken, CopilotCredentialSource, CopilotHarness, Harness, RunControls, SteerMessage,
};
use zeron_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer};

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
    history: Option<Vec<Value>>,
    interrupt_first: bool,
    posts: Arc<Mutex<Vec<Value>>>,
}

impl FakeCopilot {
    async fn start(history: Option<Vec<Value>>, interrupt_first: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            history,
            interrupt_first,
            posts: Arc::new(Mutex::new(Vec::new())),
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

        let (status, content_type, response) =
            if method == "GET" && path.ends_with("/threads/thread") {
                match &self.history {
                    Some(messages) => (
                        "200 OK",
                        "application/json",
                        json!({
                            "id": "thread",
                            "title": null,
                            "messages": messages,
                            "activity": [],
                            "updatedAt": null
                        })
                        .to_string(),
                    ),
                    None => ("404 Not Found", "application/json", "{}".into()),
                }
            } else if method == "POST" && path.ends_with("/chat") {
                self.posts.lock().unwrap().push(body);
                let post_index = self.posts.lock().unwrap().len() - 1;
                let events = if self.interrupt_first && post_index == 0 {
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
                    let run_id = format!("run-{}", post_index + 1);
                    vec![
                        json!({"type":"RUN_STARTED","threadId":"thread","runId":run_id}),
                        json!({"type":"RUN_FINISHED","runId":run_id}),
                    ]
                };
                (
                    "200 OK",
                    "text/event-stream",
                    events
                        .into_iter()
                        .map(|event| format!("data: {}\n\n", event))
                        .collect::<String>(),
                )
            } else {
                ("404 Not Found", "application/json", "{}".into())
            };
        let headers = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n",
            response.len()
        );
        let _ = stream.write_all(headers.as_bytes()).await;
        let _ = stream.write_all(response.as_bytes()).await;
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
    let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(1);
    drop(steer_tx);
    let answer = answer.map(str::to_owned);
    RunControls {
        request_input: Box::new(move |questions| {
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Done { .. }))
    );
}

fn harness(fake: &FakeCopilot) -> CopilotHarness {
    CopilotHarness::new(Arc::new(TestCredentials(CopilotCredentials {
        base_url: fake.base.clone(),
        access_token: "test-token".into(),
    })))
}

#[tokio::test]
async fn subsequent_turns_preserve_stored_transcript() {
    let stored = vec![
        json!({"role":"user","content":"old question"}),
        json!({"role":"assistant","content":"old answer"}),
    ];
    let fake = FakeCopilot::start(Some(stored.clone()), false).await;
    let copilot = harness(&fake);

    run_to_end(&copilot, request("first", Some("thread")), controls(None)).await;
    run_to_end(&copilot, request("second", Some("thread")), controls(None)).await;

    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        posts[1]["messages"],
        json!([
            {"role":"user","content":"old question"},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"second"}
        ])
    );
}

#[tokio::test]
async fn missing_thread_history_is_treated_as_empty() {
    let fake = FakeCopilot::start(None, false).await;
    let copilot = harness(&fake);

    run_to_end(
        &copilot,
        request("new thread", Some("thread")),
        controls(None),
    )
    .await;

    let posts = fake.posts();
    assert_eq!(
        posts[0]["messages"],
        json!([{"role":"user","content":"new thread"}])
    );
}

#[tokio::test]
async fn interrupt_resume_preserves_transcript_and_approval_payload() {
    let stored = vec![
        json!({"role":"user","content":"question"}),
        json!({"role":"assistant","content":"approval needed"}),
    ];
    let fake = FakeCopilot::start(Some(stored.clone()), true).await;
    let copilot = harness(&fake);

    run_to_end(
        &copilot,
        request("question", Some("thread")),
        controls(Some("Approve")),
    )
    .await;

    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1]["messages"], json!(stored));
    assert_eq!(posts[1]["parentRunId"], "run-1");
    assert_eq!(
        posts[1]["resume"],
        json!([{
            "interruptId":"interrupt-1",
            "status":"resolved",
            "payload":{"approved":true}
        }])
    );
}

#[tokio::test]
async fn interrupt_decline_sends_a_negative_approval_payload() {
    let fake = FakeCopilot::start(
        Some(vec![
            json!({"role":"assistant","content":"approval needed"}),
        ]),
        true,
    )
    .await;
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
    assert_eq!(
        posts[1]["messages"],
        json!([{
            "role":"assistant",
            "content":"approval needed"
        }])
    );
}
