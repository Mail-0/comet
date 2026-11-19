use super::*;

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
