use super::*;

fn preset(dir: &std::path::Path, chat: &str) {
    let store = crate::keiki_store::Store::new(dir);
    let owner = crate::keiki_store::Owner {
        account: "user@example.test".into(),
        org: "org-1".into(),
        revision: 0,
    };
    store.set_questions(&owner, chat, &json!([{ "id":"auth", "question":{"metadata":{"mcpConnect":{"agentId":"agent-a","presetId":"docs","name":"Docs"}}} }])).unwrap();
}

#[tokio::test]
async fn phase4_remote_contact_actions_and_task_membership_use_bound_locators() {
    let contact = "/api/webapp/conversations/contact-c?agentId=agent-a&limit=50&offset=0";
    let latest = "/api/webapp/conversations/contact-c?agentId=agent-a&limit=1&offset=0";
    let mut detail = detail(json!([]));
    detail["phone"] = json!("contact-c");
    detail["blocked"] = json!(true);
    detail["takeover"] =
        json!({"startedAt":"2026-01-01T00:00:00Z","expiresAt":"2099-01-01T00:00:00Z"});
    detail["tasks"] =
        json!([{"id":"task-a","status":"running","subagent":"Fern","request":"Review"}]);
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", contact, detail.clone()),
            Reply::ok("GET", latest, detail.clone()),
            Reply::ok(
                "DELETE",
                "/api/webapp/conversations/contact-c/block?agentId=agent-a",
                json!({"blocked":false}),
            ),
            Reply::ok(
                "POST",
                "/api/webapp/conversations/contact-c/block?agentId=agent-a",
                json!({"blocked":true}),
            ),
            Reply::ok(
                "POST",
                "/api/webapp/conversations/contact-c/takeover?agentId=agent-a",
                json!({"takeover":detail["takeover"]}),
            ),
            Reply::ok(
                "DELETE",
                "/api/webapp/conversations/contact-c/takeover?agentId=agent-a",
                json!({"takeover":null}),
            ),
            Reply::ok("GET", latest, detail.clone()),
            Reply::ok("GET", latest, detail),
            Reply::ok(
                "POST",
                "/api/webapp/subagent-tasks/task%2Da/cancel",
                json!({"cancelled":true}),
            ),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let rpc = runtime.core().rpc_service();
    let chat = value(
        &rpc,
        "OpenKeikiConversation",
        json!({"agentId":"agent-a","conversationId":"contact-c"}),
    )
    .await["chatId"]
        .as_str()
        .unwrap()
        .to_owned();
    let info = value(
        &rpc,
        "KeikiConversationInfo",
        json!({"chatId":chat,"includeRemote":true}),
    )
    .await;
    assert_eq!(info["blocked"], true);
    assert!(info["takeover"].is_object());
    assert_eq!(
        value(
            &rpc,
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"unblock"})
        )
        .await["blocked"],
        false
    );
    assert_eq!(
        value(
            &rpc,
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"block"})
        )
        .await["blocked"],
        true
    );
    value(
        &rpc,
        "SetKeikiTakeover",
        json!({"chatId":chat,"enabled":true}),
    )
    .await;
    value(
        &rpc,
        "SetKeikiTakeover",
        json!({"chatId":chat,"enabled":false}),
    )
    .await;
    assert!(
        rpc.handle(
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"end"})
        )
        .await
        .is_err()
    );
    assert!(
        rpc.handle(
            "CancelKeikiTask",
            json!({"chatId":chat,"taskId":"foreign-task"})
        )
        .await
        .is_err()
    );
    assert_eq!(
        value(
            &rpc,
            "CancelKeikiTask",
            json!({"chatId":chat,"taskId":"task-a"})
        )
        .await["cancelled"],
        true
    );
    runtime.shutdown().await;
    let requests = server.done.await.unwrap();
    for (headers, _) in requests.iter().skip(2) {
        assert_eq!(headers["authorization"], "Bearer engine-token");
    }
}

#[tokio::test]
async fn phase4_terminal_admission_bounds_inflight_opens_and_releases_on_scope_change() {
    let received: Vec<_> = (0..32)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let release: Vec<_> = (0..32)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
        ]);
        for i in 0..32 {
            let mut reply = Reply::ok(
                "POST",
                "/api/webapp/conversations/api:agent-a:c/terminal?agentId=agent-a",
                json!({"terminalId":"remote-shell"}),
            );
            reply.received = Some(received[i].clone());
            reply.release = Some(release[i].clone());
            replies.push(reply);
        }
        for _ in 0..32 {
            replies.push(Reply::ok(
                "DELETE",
                "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell?agentId=agent-a",
                json!({}),
            ));
        }
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    let mut opens = Vec::new();
    for _ in 0..32 {
        let rpc = core.rpc_service();
        let chat = chat.clone();
        opens.push(tokio::spawn(async move {
            value(&rpc, "OpenTerminal", json!({"chatId":chat})).await
        }));
    }
    for signal in received {
        tokio::time::timeout(Duration::from_secs(5), signal.notified())
            .await
            .unwrap();
    }
    assert!(
        rpc.handle("OpenTerminal", json!({"chatId":chat}))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Close a sandbox terminal")
    );
    for signal in release {
        signal.notify_one();
    }
    let mut ids = std::collections::HashSet::new();
    for open in opens {
        let answer = open.await.unwrap();
        assert!(ids.insert(answer["id"].as_str().unwrap().to_owned()));
    }
    let transition = ScopeTransition::new(core.auth().inner.scope_changes.clone());
    tokio::time::timeout(Duration::from_secs(5), server.done)
        .await
        .unwrap()
        .unwrap();
    drop(transition);
    for id in ids {
        assert!(
            rpc.handle("TerminalStatus", json!({"terminalId":id}))
                .await
                .is_err()
        );
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn phase4_mcp_opens_only_in_engine_and_redacts_capabilities_and_errors() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()), Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::ok("POST", "/api/webapp/agents/agent-a/mcp-presets/docs/connect", json!({"preset":{"id":"docs","name":"Docs","status":"needs_auth","authorizationUrl":"https://provider.example/consent?token=secret-capability","lastError":"secret-error"}})),
            Reply::ok("POST", "/api/webapp/agents/agent-a/mcp-presets/docs/status", json!({"preset":{"id":"docs","status":"connected","authorizationUrl":"https://provider.example/?token=do-not-return"}})),
            Reply::ok("POST", "/api/webapp/agents/agent-a/mcp-presets/docs/connect", json!({"preset":{"id":"docs","status":"needs_auth","authorizationUrl":"javascript:secret"}})),
        ]);
        replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    assert!(
        rpc.handle(
            "KeikiPresetConnection",
            json!({"chatId":chat,"presetId":"docs"})
        )
        .await
        .is_err()
    );
    preset(dir.path(), &chat);
    assert!(
        rpc.handle(
            "KeikiPresetConnection",
            json!({"chatId":chat,"presetId":"another"})
        )
        .await
        .is_err()
    );
    let opened = Arc::new(Mutex::new(Vec::new()));
    let captured = opened.clone();
    core.keiki
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set_opener(Box::new(move |url| {
            captured.lock().unwrap().push(url.to_owned());
            Ok(())
        }));
    let answer = value(
        &rpc,
        "KeikiPresetConnection",
        json!({"chatId":chat,"presetId":"docs"}),
    )
    .await;
    assert_eq!(answer["opened"], true);
    assert!(!answer.to_string().contains("secret"));
    assert!(!answer.to_string().contains("url"));
    assert!(opened.lock().unwrap()[0].contains("secret-capability"));
    let status = value(
        &rpc,
        "KeikiPresetConnection",
        json!({"chatId":chat,"presetId":"docs","refresh":true}),
    )
    .await;
    assert_eq!(status["status"], "connected");
    assert!(!status.to_string().contains("token"));
    let error = rpc
        .handle(
            "KeikiPresetConnection",
            json!({"chatId":chat,"presetId":"docs"}),
        )
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains("javascript"));
    assert!(!error.contains("secret"));
    assert_eq!(opened.lock().unwrap().len(), 1);
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn phase4_org_switch_discards_inflight_authorization_before_opening_browser() {
    let received = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut pending = Reply::ok("POST", "/api/webapp/agents/agent-a/mcp-presets/docs/connect", json!({"preset":{"id":"docs","status":"needs_auth","authorizationUrl":"https://provider.example/?token=stale"}}));
        pending.received = Some(received.clone()); pending.release = Some(release.clone());
        replies.extend([Reply::ok("GET", "/api/webapp/agents", agents()), Reply::ok("GET", DETAIL, detail(json!([]))), pending,
            Reply::ok("POST", "/api/webapp/orgs/switch", json!({"ok":true,"activeOrgId":"org-2"})),
            Reply::ok("GET", "/api/webapp/auth/session", account(Some("org-2"), json!([{"id":"org-2","name":"Two"}])))]);
        replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    preset(dir.path(), &chat);
    let opened = Arc::new(Mutex::new(Vec::new()));
    let captured = opened.clone();
    core.keiki
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set_opener(Box::new(move |url| {
            captured.lock().unwrap().push(url.to_owned());
            Ok(())
        }));
    let pending = tokio::spawn(async move {
        rpc.handle(
            "KeikiPresetConnection",
            json!({"chatId":chat,"presetId":"docs"}),
        )
        .await
    });
    received.notified().await;
    core.auth().select_org("org-2").await.unwrap();
    release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(opened.lock().unwrap().is_empty());
    runtime.shutdown().await;
    server.done.await.unwrap();
}

#[tokio::test]
async fn phase4_avatar_http_cache_and_scope_validation() {
    use base64::Engine as _;
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut avatar = Reply::ok(
            "GET",
            "/api/public/agent-avatars/agent%2Da.gif?size=64&state=idle&theme=dark",
            Value::Null,
        );
        avatar.raw = Some(
            base64::engine::general_purpose::STANDARD
                .decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")
                .unwrap(),
        );
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            avatar,
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::error(
                "GET",
                "/api/public/agent-avatars/agent%2Da.gif?size=64&state=error&theme=dark",
                503,
            ),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let rpc = runtime.core().rpc_service();
    let first = value(&rpc, "GetKeikiAvatar", json!({"agentId":"agent-a"})).await;
    assert!(
        first["src"]
            .as_str()
            .unwrap()
            .starts_with("data:image/gif;base64,")
    );
    assert_eq!(
        first,
        value(&rpc, "GetKeikiAvatar", json!({"agentId":"agent-a"})).await
    );
    assert!(
        rpc.handle("GetKeikiAvatar", json!({"agentId":"foreign"}))
            .await
            .is_err()
    );
    assert!(
        rpc.handle(
            "GetKeikiAvatar",
            json!({"agentId":"agent-a","state":"error"})
        )
        .await
        .is_err()
    );
    runtime.shutdown().await;
    let requests = server.done.await.unwrap();
    assert!(!requests[3].0.contains_key("authorization"));
}

#[tokio::test]
async fn phase4_terminal_scope_transition_ends_stream_and_closes_remote_shell() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut stream = Reply::ok(
            "GET",
            "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell/stream?agentId=agent-a",
            Value::Null,
        );
        stream.stream =
            Some("event: data\ndata: {\"type\":\"data\",\"seq\":1,\"data\":\"SGk=\"}\n\n".into());
        stream.hold_open = true;
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::ok(
                "POST",
                "/api/webapp/conversations/api:agent-a:c/terminal?agentId=agent-a",
                json!({"terminalId":"remote-shell"}),
            ),
            stream,
            Reply::ok(
                "DELETE",
                "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell?agentId=agent-a",
                json!({}),
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
    let id = value(&rpc, "OpenTerminal", json!({"chatId":chat})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let RpcReply::Stream(mut stream) = rpc
        .handle("SubscribeTerminal", json!({"terminalId":id}))
        .await
        .unwrap()
    else {
        panic!("stream")
    };
    assert_eq!(stream.next().await.unwrap()["seq"], 1);
    let transition = ScopeTransition::new(core.auth().inner.scope_changes.clone());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rpc.handle("WriteTerminal", json!({"terminalId":id,"data":"aGk="}))
            .await
            .is_err()
    );
    drop(transition);
    server.done.await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
async fn phase4_rejected_refresh_clears_terminal_stream_and_remote_session() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut stream = Reply::ok(
            "GET",
            "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell/stream?agentId=agent-a",
            Value::Null,
        );
        stream.stream =
            Some("event: data\ndata: {\"type\":\"data\",\"seq\":1,\"data\":\"SGk=\"}\n\n".into());
        stream.hold_open = true;
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::ok(
                "POST",
                "/api/webapp/conversations/api:agent-a:c/terminal?agentId=agent-a",
                json!({"terminalId":"remote-shell"}),
            ),
            stream,
            Reply::error("POST", "/oauth/token", 400),
            Reply::ok(
                "DELETE",
                "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell?agentId=agent-a",
                json!({}),
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
    let id = value(&rpc, "OpenTerminal", json!({"chatId":chat})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let RpcReply::Stream(mut stream) = rpc
        .handle("SubscribeTerminal", json!({"terminalId":id}))
        .await
        .unwrap()
    else {
        panic!("stream")
    };
    assert_eq!(stream.next().await.unwrap()["seq"], 1);
    lock(&core.auth().inner.session)
        .access
        .as_mut()
        .unwrap()
        .received = SystemTime::UNIX_EPOCH;
    let error = rpc
        .handle("WriteTerminal", json!({"terminalId":id,"data":"aGk="}))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains("engine-token"));
    assert!(matches!(core.auth().state(), AuthState::SignedOut));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    server.done.await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
async fn phase4_sandbox_terminal_uses_opaque_ids_and_existing_terminal_protocol() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        let mut stream = Reply::ok("GET", "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell/stream?agentId=agent-a", Value::Null);
        stream.stream = Some("event: data\ndata: {\"type\":\"data\",\"seq\":1,\"data\":\"SGVsbG8=\"}\n\nevent: exit\ndata: {\"type\":\"exit\",\"seq\":2,\"exitCode\":0}\n\n".into());
        replies.extend([
            Reply::ok("GET", "/api/webapp/agents", agents()),
            Reply::ok("GET", DETAIL, detail(json!([]))),
            Reply::ok("POST", "/api/webapp/conversations/api:agent-a:c/terminal?agentId=agent-a", json!({"terminalId":"remote-shell"})),
            stream,
            Reply::ok("POST", "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell/input?agentId=agent-a", json!({})),
            Reply::ok("POST", "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell/resize?agentId=agent-a", json!({})),
            Reply::ok("DELETE", "/api/webapp/conversations/api:agent-a:c/terminal/remote-shell?agentId=agent-a", json!({})),
        ]);
        replies
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let rpc = runtime.core().rpc_service();
    let chat = open(&rpc).await;
    assert!(
        rpc.handle("OpenTerminal", json!({"chatId":chat,"cols":0}))
            .await
            .is_err()
    );
    let session = value(
        &rpc,
        "OpenTerminal",
        json!({"chatId":chat,"cols":100,"rows":30}),
    )
    .await;
    let id = session["id"].as_str().unwrap();
    assert!(id.starts_with("keiki-pty-"));
    assert!(!session.to_string().contains("remote-shell"));
    assert!(!session.to_string().contains("engine-token"));
    let RpcReply::Stream(mut events) = rpc
        .handle("SubscribeTerminal", json!({"terminalId":id}))
        .await
        .unwrap()
    else {
        panic!("expected terminal stream")
    };
    assert_eq!(
        events.next().await.unwrap(),
        json!({"type":"data","seq":1,"data":"SGVsbG8="})
    );
    assert_eq!(events.next().await.unwrap()["exitCode"], 0);
    assert!(events.next().await.is_none());
    assert!(
        rpc.handle("WriteTerminal", json!({"terminalId":id,"data":"%%%"}))
            .await
            .is_err()
    );
    value(
        &rpc,
        "WriteTerminal",
        json!({"terminalId":id,"data":"bHMK"}),
    )
    .await;
    value(
        &rpc,
        "ResizeTerminal",
        json!({"terminalId":id,"cols":120,"rows":40}),
    )
    .await;
    value(&rpc, "CloseTerminal", json!({"terminalId":id})).await;
    let requests = server.done.await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&requests[4].1).unwrap(),
        json!({"cols":100,"rows":30})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&requests[6].1).unwrap(),
        json!({"data":"bHMK"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&requests[7].1).unwrap(),
        json!({"cols":120,"rows":40})
    );
    assert!(requests.iter().skip(2).all(|(headers, _)| {
        headers.get("authorization").map(String::as_str) == Some("Bearer engine-token")
    }));
    runtime.shutdown().await;
}

#[tokio::test]
async fn phase4_local_actions_preserve_remote_history_and_reopen_deleted_entries() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        for _ in 0..2 {
            replies.extend([
                Reply::ok("GET", "/api/webapp/agents", agents()),
                Reply::ok(
                    "GET",
                    DETAIL,
                    detail(json!([message("m", "Keep me", "outbound", 1)])),
                ),
            ]);
        }
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let core = runtime.core();
    let rpc = core.rpc_service();
    let chat = open(&rpc).await;
    for (action, field, expected) in [
        ("rename", "title", json!("Launch review")),
        ("pin", "pinned", json!(true)),
        ("archive", "archived", json!(true)),
        ("restore", "archived", json!(false)),
    ] {
        let answer = value(
            &rpc,
            "KeikiConversationAction",
            json!({"chatId":chat,"action":action,"title":"Launch review"}),
        )
        .await;
        assert_eq!(answer[field], expected);
        assert!(!answer.to_string().contains("engine-token"));
    }
    assert!(
        rpc.handle(
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"rename","title":" "})
        )
        .await
        .is_err()
    );
    assert!(
        rpc.handle("KeikiConversationInfo", json!({"chatId":"not-bound"}))
            .await
            .is_err()
    );
    assert!(
        rpc.handle(
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"clearHistory"})
        )
        .await
        .is_err()
    );
    let launched = Arc::new(Mutex::new(Vec::new()));
    let output = launched.clone();
    core.keiki
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set_opener(Box::new(move |url| {
            output.lock().unwrap().push(url.to_owned());
            Ok(())
        }));
    assert_eq!(
        value(
            &rpc,
            "KeikiConversationAction",
            json!({"chatId":chat,"action":"openDashboard"})
        )
        .await,
        json!({"opened":true})
    );
    let url = launched.lock().unwrap()[0].clone();
    assert!(url.contains("/conversations/api:agent-a:c?agentId=agent-a"));
    assert!(!url.contains("token"));
    value(
        &rpc,
        "KeikiConversationAction",
        json!({"chatId":chat,"action":"deleteLocal"}),
    )
    .await;
    assert!(core.workspace.chat(&chat).unwrap().is_none());
    assert!(
        rpc.handle("KeikiConversationInfo", json!({"chatId":chat}))
            .await
            .is_err()
    );
    let reopened = open(&rpc).await;
    assert_ne!(chat, reopened);
    assert!(
        !value(&rpc, "KeikiConversationInfo", json!({"chatId":reopened})).await["pinned"]
            .as_bool()
            .unwrap()
    );
    runtime.shutdown().await;
    assert_eq!(server.done.await.unwrap().len(), 6);
}

#[tokio::test]
async fn phase4_group_confirmation_templates_empty_and_errors() {
    let server = Server::start(|url| {
        let mut replies = restored(url);
        replies.extend([
            Reply::ok(
                "POST",
                "/api/webapp/agent-groups/group%2Da/end",
                json!({"threadsEnded":2,"tasksCancelled":3}),
            ),
            Reply::ok(
                "GET",
                "/api/webapp/agent-templates",
                json!({"templates":[]}),
            ),
            Reply::ok(
                "POST",
                "/api/webapp/agents",
                json!({"ok":true,"id":"new-agent","missingSecrets":[]}),
            ),
            Reply::error("POST", "/api/webapp/agent-groups/group%2Da/end", 503),
            Reply::error("GET", "/api/webapp/agent-templates", 401),
        ]);
        replies
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&server, dir.path()).await;
    let rpc = runtime.core().rpc_service();
    assert!(
        rpc.handle("EndKeikiGroup", json!({"groupId":"group-a"}))
            .await
            .is_err()
    );
    assert_eq!(
        value(
            &rpc,
            "EndKeikiGroup",
            json!({"groupId":"group-a","confirmed":true})
        )
        .await,
        json!({"threadsEnded":2,"tasksCancelled":3})
    );
    assert_eq!(
        value(&rpc, "ListKeikiTemplates", json!({})).await["templates"],
        json!([])
    );
    assert!(
        rpc.handle("CreateKeikiAgent", json!({"template":""}))
            .await
            .is_err()
    );
    assert_eq!(
        value(
            &rpc,
            "CreateKeikiAgent",
            json!({"template":"assistant","name":"Nova"})
        )
        .await["id"],
        "new-agent"
    );
    let error = rpc
        .handle(
            "EndKeikiGroup",
            json!({"groupId":"group-a","confirmed":true}),
        )
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("503"));
    assert!(!error.contains("invalid_grant"));
    assert!(rpc.handle("ListKeikiTemplates", json!({})).await.is_err());
    assert!(matches!(
        runtime.core().auth().state(),
        AuthState::SignedOut
    ));
    runtime.shutdown().await;
    let requests = server.done.await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&requests[4].1).unwrap(),
        json!({"template":"assistant","name":"Nova"})
    );
}
