use super::*;
use futures::StreamExt;
use zeron_harness::{CancellationToken, CopilotHarness, Harness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest};
use zeron_rpc::{RpcReply, RpcService};

#[path = "keiki_actions_tests.rs"]
mod actions;

const DETAIL: &str = "/api/webapp/conversations/api:agent-a:c?agentId=agent-a&limit=50&offset=0";
const REFRESH: &str = "/api/webapp/conversations/api:agent-a:c?agentId=agent-a&limit=100&offset=0";
const STEER: &str = "/api/webapp/conversations/api:agent-a:c/steer?agentId=agent-a";

fn agents() -> Value {
    json!({"agents":[{"id":"agent-a","name":"Assistant"},{"id":"agent-b","name":"Other"}]})
}

fn detail(messages: Value) -> Value {
    json!({"phone":"api:agent-a:c","meta":{"agentId":"agent-a","messageCount":messages.as_array().unwrap().len()},
        "messages":messages,"agent":{"id":"agent-a"},"asks":[],"blocked":false,"takeover":null,
        "activeTurnStartedAt":null,"liveness":null})
}

fn message(id: &str, text: &str, direction: &str, time: u32) -> Value {
    json!({"id":id,"content":text,"direction":direction,"createdAt":chrono::DateTime::from_timestamp(1767225600+time as i64,0).unwrap().to_rfc3339(),"internal":false})
}

fn restored(_url: &str) -> Vec<Reply> {
    vec![
        Reply::ok(
            "POST",
            "/oauth/token",
            tokens("engine-token", "rotated-refresh"),
        ),
        Reply::ok(
            "GET",
            "/api/webapp/auth/session",
            account(
                Some("org-1"),
                json!([{"id":"org-1","name":"One"},{"id":"org-2","name":"Two"}]),
            ),
        ),
    ]
}

async fn runtime(server: &Server, dir: &std::path::Path) -> crate::EngineRuntime {
    let (auth, _, _) = auth(server, true);
    let config = crate::EngineConfig {
        data_dir: dir.to_owned(),
        ipc_port: 0,
        default_harness: crate::HarnessId::Copilot,
    };
    crate::Engine::assemble_runtime(&config, auth, crate::EngineProfile::local(dir).unwrap())
        .await
        .unwrap()
}

async fn value(rpc: &crate::EngineRpc, method: &str, params: Value) -> Value {
    match rpc.handle(method, params).await.unwrap() {
        RpcReply::Value(value) => value,
        _ => panic!("expected value"),
    }
}

async fn open(rpc: &crate::EngineRpc) -> String {
    value(
        rpc,
        "OpenKeikiConversation",
        json!({"agentId":"agent-a","conversationId":"api:agent-a:c"}),
    )
    .await["chatId"]
        .as_str()
        .unwrap()
        .into()
}

fn controls(steers: Vec<&str>, cancelled: bool) -> RunControls {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    for text in steers {
        tx.try_send(SteerMessage::text(text)).unwrap();
    }
    drop(tx);
    let interrupt = CancellationToken::new();
    if cancelled {
        interrupt.cancel();
    }
    RunControls {
        interrupt,
        steering: rx,
        request_input: Box::new(|_| panic!("bound questions must be read-only")),
        execution_lease: None,
        realtime: None,
        turn: Default::default(),
    }
}

async fn run(core: &crate::EngineCore, chat: &str, control: RunControls) -> Vec<AgentEvent> {
    let harness = CopilotHarness::new(Arc::new(core.copilot_credentials.clone()));
    let request: RunRequest = serde_json::from_value(json!({"prompt":"hello","resume":chat,"model":null,"reasoning":null,"cwd":"/","sandbox":"danger-full-access"})).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        harness
            .run(request, control)
            .await
            .unwrap()
            .map(|event| event.unwrap())
            .collect(),
    )
    .await
    .unwrap()
}

fn streamed(events: &[Value]) -> Reply {
    let mut reply = Reply::ok("POST", STEER, Value::Null);
    reply.stream = Some(
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect(),
    );
    reply
}

fn stream_events() -> Vec<Value> {
    vec![
        json!({"type":"RUN_STARTED","threadId":"not-a-dashboard-resume","runId":"r"}),
        json!({"type":"TEXT_MESSAGE_START","messageId":"answer","role":"assistant"}),
        json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"answer","delta":"Hello"}),
        json!({"type":"TEXT_MESSAGE_END","messageId":"answer"}),
        json!({"type":"RUN_FINISHED","threadId":"not-a-dashboard-resume","runId":"r"}),
    ]
}

#[tokio::test]
async fn browse_whole_agents_groups_roster_paginated_conversations_empty_and_error() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", "/api/webapp/agent-groups", json!({"groups":[{"id":"group-a","name":"Team","members":[{"agentId":"agent-a","name":"Assistant"}]}]})),
            Reply::ok("GET", "/api/webapp/agent-groups/group%2Da/roster", json!({"members":[],"totals":{}})),
            Reply::ok("GET", "/api/webapp/agents/agent%2Da/conversations?limit=2&offset=0", json!({"conversations":[{"phone":"a","apiKey":"must-not-leak"},{"phone":"b","apiKey":"must-not-leak"}]})),
            Reply::ok("GET", "/api/webapp/agents/agent%2Da/conversations?limit=2&offset=2", json!({"conversations":[]})),
            Reply::error("GET", "/api/webapp/agents/agent%2Da/conversations?limit=2&offset=4", 503),
        ]);
        replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let rpc = runtime.core().rpc_service();
    assert_eq!(
        value(&rpc, "ListKeikiAgents", json!({})).await["agents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        value(&rpc, "ListKeikiAgentGroups", json!({})).await["groups"][0]["id"],
        "group-a"
    );
    assert_eq!(
        value(
            &rpc,
            "GetKeikiAgentGroupRoster",
            json!({"groupId":"group-a"})
        )
        .await["members"],
        json!([])
    );
    let first = value(
        &rpc,
        "ListKeikiConversations",
        json!({"agentId":"agent-a","limit":2}),
    )
    .await;
    assert_eq!(first["nextOffset"], 2);
    assert!(!first.to_string().contains("must-not-leak"));
    assert!(
        value(
            &rpc,
            "ListKeikiConversations",
            json!({"agentId":"agent-a","limit":2,"offset":2})
        )
        .await["nextOffset"]
            .is_null()
    );
    assert!(
        rpc.handle(
            "ListKeikiConversations",
            json!({"agentId":"agent-a","limit":2,"offset":4})
        )
        .await
        .is_err()
    );
    assert!(
        rpc.handle(
            "ListKeikiConversations",
            json!({"agentId":"agent-a","limit":0})
        )
        .await
        .is_err()
    );
    assert!(runtime.core().workspace.read_chats().unwrap().is_empty());
    runtime.shutdown().await;
    let requests = server.done.await.unwrap();
    assert!(requests.iter().skip(2).all(|(headers, _)| {
        headers.get("authorization").map(String::as_str) == Some("Bearer engine-token")
    }));
}

#[tokio::test]
async fn duplicate_opens_import_sorted_stable_history_and_reconcile_changes_and_clear() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        for _ in 0..2 {
            replies.push(Reply::ok("GET", "/api/webapp/agents", agents()));
            replies.push(Reply::ok(
                "GET",
                DETAIL,
                detail(json!([
                    message("b", "second", "outbound", 2),
                    message("a", "first", "inbound", 1)
                ])),
            ));
        }
        replies.push(Reply::ok(
            "GET",
            REFRESH,
            detail(json!([
                message("a", "first", "inbound", 1),
                message("b", "updated", "outbound", 2)
            ])),
        ));
        replies.push(Reply::ok("GET", REFRESH, detail(json!([]))));
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    assert_eq!(open(&rpc).await, chat);
    assert_eq!(core.workspace.read_chats().unwrap().len(), 1);
    let handle = core.doc_host.open(&chat).unwrap();
    let entries = handle.doc().read_entries().unwrap();
    assert_eq!(
        entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec!["keiki:a", "keiki:b"]
    );
    assert_eq!(entries[0].created_at, 1767225601000);
    assert_eq!(entries[0].role, zeron_doc::MessageRole::User);
    assert!(core.sessions.session_status(&chat).is_none());
    value(&rpc, "LoadKeikiHistory", json!({"chatId":chat,"limit":100})).await;
    let entries = handle.doc().read_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(serde_json::to_string(&entries).unwrap().contains("updated"));
    value(&rpc, "LoadKeikiHistory", json!({"chatId":chat,"limit":100})).await;
    assert!(handle.doc().read_entries().unwrap().is_empty());
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn normal_bound_send_and_steer_map_streams_without_dashboard_thread_or_token_results() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
        ]);
        for _ in 0..2 {
            replies.push(streamed(&stream_events()));
            replies.push(Reply::ok(
                "GET",
                REFRESH,
                detail(json!([message("answer", "Hello", "outbound", 2)])),
            ));
        }
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let chat = open(&core.rpc_service()).await;
    let events = run(core, &chat, controls(vec!["change course"], false)).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e,AgentEvent::TextDelta{text} if text=="Hello"))
    );
    assert!(
        events
            .iter()
            .filter_map(
                |e| if let AgentEvent::SessionStarted { session_id, .. } = e {
                    Some(session_id)
                } else {
                    None
                }
            )
            .all(|id| id == &chat)
    );
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("engine-token")
    );
    assert_eq!(
        core.doc_host
            .open(&chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        1
    );
    runtime.shutdown().await;
    let requests = server.done.await.unwrap();
    let posts: Vec<Value> = requests
        .into_iter()
        .filter_map(|(_, body)| serde_json::from_str::<Value>(&body).ok())
        .filter(|v| v.get("text").is_some())
        .collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], json!({"text":"hello","stream":true}));
    assert_eq!(posts[1], json!({"text":"change course","stream":true}));
}

#[tokio::test]
async fn pending_questions_are_read_only_and_composer_reply_uses_steer_not_resume() {
    let server = Server::start(|url| {
        let mut replies=restored(url);
        replies.extend([Reply::ok("GET","/api/webapp/agents",agents()),Reply::ok("GET",DETAIL,detail(json!([])))]);
        replies.push(streamed(&[json!({"type":"RUN_FINISHED","runId":"r","outcome":{"type":"interrupt","interrupts":[{"id":"question-1","reason":"Choose","message":"Where?"}]}})]));
        replies.push(Reply::ok("GET",REFRESH,detail(json!([]))));
        replies.push(Reply::ok("GET",REFRESH,detail(json!([]))));
        replies.push(streamed(&stream_events()));
        replies.push(Reply::ok("GET",REFRESH,detail(json!([]))));
        replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    let events = run(core, &chat, controls(vec![], false)).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::InputRequested { .. }))
    );
    let detail = value(&rpc, "LoadKeikiHistory", json!({"chatId":chat,"limit":100})).await;
    assert_eq!(detail["pendingQuestions"][0]["readOnly"], true);
    assert_eq!(detail["pendingQuestions"][0]["replyInComposer"], true);
    run(core, &chat, controls(vec![], false)).await;
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn dropped_stream_and_cancelled_send_do_not_claim_completion() {
    for cancel in [false, true] {
        let server = Server::start(|url| {
            let mut replies = restored(url);
            replies.extend([
                Reply::ok("GET", "/api/webapp/agents", agents()),
                Reply::ok("GET", DETAIL, detail(json!([]))),
            ]);
            if !cancel {
                replies.push(streamed(&[
                    json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"x","delta":"partial"}),
                ]));
            }
            replies.push(Reply::ok("GET", REFRESH, detail(json!([]))));
            replies
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, dir.path()).await;
        let core = runtime.core();
        let chat = open(&core.rpc_service()).await;
        let events = run(core, &chat, controls(vec![], cancel)).await;
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )));
        let expected = if cancel {
            DoneStatus::Interrupted
        } else {
            DoneStatus::Errored
        };
        assert!(
            events
                .iter()
                .any(|e| matches!(e,AgentEvent::Done{status,..} if *status==expected))
        );
        assert!(
            core.doc_host
                .open(&chat)
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .is_empty()
        );
        runtime.shutdown().await;
        server.done.await.unwrap();
    }
}

#[tokio::test]
async fn rejected_bound_token_clears_engine_account_and_holder() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::error("POST", STEER, 401),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let chat = open(&core.rpc_service()).await;
    let events = run(core, &chat, controls(vec![], false)).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Errored,
            ..
        }
    )));
    assert_eq!(core.auth().state(), AuthState::SignedOut);
    assert!(core.copilot_credentials.snapshot().is_none());
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[test]
fn persistent_bindings_are_account_org_agent_scoped_and_history_preserves_remote_ids() {
    use crate::keiki_store::{Owner, Store};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let owner = Owner {
        account: "one@example.test".into(),
        org: "org-1".into(),
        revision: 0,
    };
    let first = store.bind(&owner, "a", "same", false).unwrap();
    let mut refreshed = owner.clone();
    refreshed.revision += 1;
    assert_eq!(
        store.bind(&refreshed, "a", "same", false).unwrap().chat_id,
        first.chat_id
    );
    assert_eq!(
        store.bind(&owner, "a", "same", false).unwrap().chat_id,
        first.chat_id
    );
    store
        .cache(
            &owner,
            &first.chat_id,
            &[message("remote", "persisted", "inbound", 1)],
            true,
        )
        .unwrap();
    for other in [
        Owner {
            account: owner.account.clone(),
            org: "org-2".into(),
            revision: 0,
        },
        Owner {
            account: "two@example.test".into(),
            org: owner.org.clone(),
            revision: 0,
        },
    ] {
        assert!(store.get(&other, &first.chat_id).is_err());
        assert!(store.history(&other, &first.chat_id).is_err());
        assert_ne!(
            store.bind(&other, "a", "same", false).unwrap().chat_id,
            first.chat_id
        );
    }
    assert_ne!(
        store.bind(&owner, "b", "same", false).unwrap().chat_id,
        first.chat_id
    );
    drop(store);
    let reopened = Store::new(dir.path());
    assert_eq!(
        reopened.bind(&owner, "a", "same", false).unwrap().chat_id,
        first.chat_id
    );
    assert_eq!(
        reopened.history(&owner, &first.chat_id).unwrap()[0]["id"],
        "remote"
    );
}

#[tokio::test]
async fn opening_latest_history_then_loading_older_pages_reconciles_stable_ids() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let page = |start: u32, end: u32| {
            let mut result = detail(
                (start..end)
                    .map(|i| message(&format!("m{i:03}"), &format!("text {i}"), "outbound", i))
                    .collect(),
            );
            result["meta"]["messageCount"] = json!(53);
            result
        };
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, page(0, 50)),
            Reply::ok(
                "GET",
                "/api/webapp/conversations/api:agent-a:c?agentId=agent-a&limit=50&offset=3",
                page(3, 53),
            ),
            Reply::ok("GET", DETAIL, page(0, 3)),
            Reply::ok("GET", DETAIL, page(0, 3)),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    let handle = core.doc_host.open(&chat).unwrap();
    assert_eq!(handle.doc().read_entries().unwrap().len(), 50);
    let older = value(&rpc, "LoadKeikiHistory", json!({"chatId":chat,"before":3})).await;
    assert!(older["before"].is_null());
    value(&rpc, "LoadKeikiHistory", json!({"chatId":chat,"before":3})).await;
    let entries = handle.doc().read_entries().unwrap();
    assert_eq!(entries.len(), 53);
    assert_eq!(entries[0].id, "keiki:m000");
    assert_eq!(entries[52].id, "keiki:m052");
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn token_refresh_is_engine_owned_and_invalid_refresh_prevents_bound_posts() {
    for valid in [true, false] {
        let server = Server::start(|url| {
            let mut replies = restored(url);
            replies.extend([
                Reply::ok("GET", "/api/webapp/agents", agents()),
                Reply::ok("GET", DETAIL, detail(json!([]))),
            ]);
            if valid {
                replies.extend([
                    Reply::ok(
                        "POST",
                        "/oauth/token",
                        tokens("refreshed-engine-token", "new-refresh"),
                    ),
                    Reply::ok(
                        "GET",
                        "/api/webapp/auth/session",
                        account(Some("org-1"), json!([{"id":"org-1","name":"One"}])),
                    ),
                    streamed(&stream_events()),
                    Reply::ok("GET", REFRESH, detail(json!([]))),
                ]);
            } else {
                replies.push(Reply::error("POST", "/oauth/token", 400));
            }
            replies
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, dir.path()).await;
        let core = runtime.core();
        let chat = open(&core.rpc_service()).await;
        let auth = core.auth();
        lock(&auth.inner.session).access.as_mut().unwrap().received = SystemTime::UNIX_EPOCH;
        let events = run(core, &chat, controls(vec![], false)).await;
        assert_eq!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            )),
            valid
        );
        assert_eq!(core.auth().state().is_signed_in(), valid);
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("refreshed-engine-token")
        );
        runtime.shutdown().await;
        let requests = server.done.await.unwrap();
        if valid {
            assert!(requests.iter().rev().take(2).all(|(headers, _)| {
                headers.get("authorization").map(String::as_str)
                    == Some("Bearer refreshed-engine-token")
            }));
        }
    }
}

#[tokio::test]
async fn org_switch_invalidates_inflight_reads_before_the_new_session_is_published() {
    let read = Arc::new(tokio::sync::Notify::new());
    let release_read = Arc::new(tokio::sync::Notify::new());
    let switching = Arc::new(tokio::sync::Notify::new());
    let release_switch = Arc::new(tokio::sync::Notify::new());
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut agents_reply = Reply::ok("GET", "/api/webapp/agents", agents());
        agents_reply.received = Some(read.clone());
        agents_reply.release = Some(release_read.clone());
        let mut switch = Reply::ok(
            "POST",
            "/api/webapp/orgs/switch",
            json!({"ok":true,"activeOrgId":"org-2"}),
        );
        switch.received = Some(switching.clone());
        switch.release = Some(release_switch.clone());
        replies.extend([
            agents_reply,
            switch,
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-2"), json!([{"id":"org-2","name":"Two"}])),
            ),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let fetching = tokio::spawn(async move { rpc.handle("ListKeikiAgents", json!({})).await });
    tokio::time::timeout(Duration::from_secs(2), read.notified())
        .await
        .unwrap();
    let auth = core.auth();
    let switch = tokio::spawn(async move { auth.select_org("org-2").await });
    tokio::time::timeout(Duration::from_secs(2), switching.notified())
        .await
        .unwrap();
    assert_eq!(core.auth().state().org_id(), Some("org-1"));
    release_read.notify_one();
    assert!(fetching.await.unwrap().is_err());
    release_switch.notify_one();
    switch.await.unwrap().unwrap();
    assert_eq!(core.auth().state().org_id(), Some("org-2"));
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn cancelling_an_active_stream_drops_the_connection_and_reconciles_authoritative_history() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut live = streamed(&stream_events()[..3]);
        live.hold_open = true;
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            live,
            Reply::ok("GET", REFRESH, detail(json!([]))),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let chat = open(&core.rpc_service()).await;
    let harness = CopilotHarness::new(Arc::new(core.copilot_credentials.clone()));
    let request = serde_json::from_value(
        json!({"prompt":"hello","resume":chat,"cwd":"/","sandbox":"danger-full-access"}),
    )
    .unwrap();
    let control = controls(vec![], false);
    let interrupt = control.interrupt.clone();
    let mut events = harness.run(request, control).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            events.next().await.unwrap().unwrap(),
            AgentEvent::TextDelta { .. }
        ) {}
    })
    .await
    .unwrap();
    interrupt.cancel();
    let tail: Vec<_> = tokio::time::timeout(Duration::from_secs(2), events.collect())
        .await
        .unwrap();
    assert!(tail.iter().any(|e| matches!(
        e,
        Ok(AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        })
    )));
    assert!(
        core.doc_host
            .open(&chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .is_empty()
    );
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn final_sse_frame_without_newline_completes_and_discarded_text_is_not_imported() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut events = stream_events();
        events.insert(
            3,
            json!({"type":"TEXT_MESSAGE_DISCARDED","messageId":"answer"}),
        );
        let mut stream = streamed(&events);
        stream.stream = stream.stream.map(|s| s.trim_end().to_owned());
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            stream,
            Reply::ok("GET", REFRESH, detail(json!([]))),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let chat = open(&core.rpc_service()).await;
    let events = run(core, &chat, controls(vec![], false)).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
    assert!(
        core.doc_host
            .open(&chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .is_empty()
    );
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn drafts_have_empty_local_history_and_watches_without_remote_conversations() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.push(Reply::ok("GET", "/api/webapp/agents", agents()));
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let opened = value(&rpc, "NewKeikiConversation", json!({"agentId":"agent-a"})).await;
    let chat = opened["chatId"].as_str().unwrap();
    let history = value(&rpc, "LoadKeikiHistory", json!({"chatId":chat})).await;
    assert_eq!(history["messages"], json!([]));
    assert_eq!(history["draft"], true);
    let RpcReply::Stream(mut watch) = rpc
        .handle("WatchKeikiConversation", json!({"chatId":chat}))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(watch.next().await.unwrap()["draft"], true);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), watch.next())
            .await
            .is_err()
    );
    drop(watch);
    let request = serde_json::from_value(
        json!({"prompt":"hello","resume":chat,"cwd":"/","sandbox":"danger-full-access"}),
    )
    .unwrap();
    assert!(
        core.sessions
            .dispatch(
                "ordinary-local",
                zeron_proto::HarnessId::Copilot,
                request,
                None
            )
            .await
            .is_err()
    );
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn contact_conversations_use_message_send_then_json_steer_and_reconcile() {
    const CONTACT: &str = "/api/webapp/conversations/contact-c?agentId=agent-a&limit=50&offset=0";
    const CONTACT_REFRESH: &str =
        "/api/webapp/conversations/contact-c?agentId=agent-a&limit=100&offset=0";
    let contact_detail = |messages| {
        let mut detail = detail(messages);
        detail["phone"] = json!("contact-c");
        detail
    };
    let server=Server::start(|url| {
        let mut replies=restored(url);
        replies.extend([
            Reply::ok("GET","/api/webapp/agents",agents()),
            Reply::ok("GET",CONTACT,contact_detail(json!([]))),
            Reply::ok("POST","/api/webapp/conversations/contact-c/messages?agentId=agent-a",json!({"ok":true,"takeover":{"startedAt":"2026-01-01T00:00:00Z","expiresAt":"2026-01-01T00:15:00Z"}})),
            Reply::ok("GET",CONTACT_REFRESH,contact_detail(json!([message("m1","hello","inbound",1)]))),
            Reply::ok("POST","/api/webapp/conversations/contact-c/steer?agentId=agent-a",json!({"reply":"steer ack","traceId":"trace"})),
            Reply::ok("GET",CONTACT_REFRESH,contact_detail(json!([message("m1","hello","inbound",1),message("m2","changed","outbound",2)]))),
        ]);replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let opened = value(
        &rpc,
        "OpenKeikiConversation",
        json!({"agentId":"agent-a","conversationId":"contact-c"}),
    )
    .await;
    let chat = opened["chatId"].as_str().unwrap();
    let events = run(core, chat, controls(vec!["change course"], false)).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        core.doc_host
            .open(chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        2
    );
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn organization_switch_hides_bound_chats_and_closes_existing_transcript_streams() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::ok(
                "POST",
                "/api/webapp/orgs/switch",
                json!({"ok":true,"activeOrgId":"org-2"}),
            ),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(
                    Some("org-2"),
                    json!([{"id":"org-1","name":"One"},{"id":"org-2","name":"Two"}]),
                ),
            ),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    let RpcReply::Stream(mut chats) = rpc
        .handle(zeron_rpc::methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(
        core.keiki
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .authorize_chat(&chat)
            .is_ok()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while chats.next().await.unwrap().as_array().unwrap().is_empty() {}
    })
    .await
    .unwrap();
    let RpcReply::Stream(mut transcript) = rpc
        .handle(
            zeron_rpc::methods::WATCH_DOC_MESSAGES,
            json!({"chatId":chat}),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    transcript.next().await.unwrap();
    core.auth().select_org("org-2").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), transcript.next())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), chats.next())
            .await
            .unwrap()
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        rpc.handle("LoadKeikiHistory", json!({"chatId":chat}))
            .await
            .is_err()
    );
    runtime.shutdown().await;
    server.done.await.unwrap();
}
