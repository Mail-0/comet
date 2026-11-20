//! Local engine admission. Device-room transport is covered in zeron-rpc's device_room suite.

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use zeron_engine::{
    BranchHeadContext, ChangeRequestError, CheckoutChangeRequestLookup, CheckoutChangeRequests,
    CheckoutSourceContext, EngineCore, HarnessRegistry,
};
use zeron_proto::{ChangeRequestState, ChangeRequestSummary, HarnessId};
use zeron_rpc::{RpcService, methods};

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("device-id"), device_id).unwrap();
    EngineCore::assemble(dir, Arc::new(HarnessRegistry::new()), HarnessId::Mock, None).unwrap()
}

struct StaticChangeRequestLookup {
    source: CheckoutSourceContext,
    summary: ChangeRequestSummary,
    resolves: AtomicUsize,
}

#[async_trait]
impl CheckoutChangeRequestLookup for StaticChangeRequestLookup {
    async fn inspect_checkout(
        &self,
        _cwd: &std::path::Path,
    ) -> Result<CheckoutSourceContext, ChangeRequestError> {
        Ok(self.source.clone())
    }

    async fn resolve_github_source(
        &self,
        _source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
        self.resolves.fetch_add(1, Ordering::AcqRel);
        Ok(Some(self.summary.clone()))
    }
}

fn init_git_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).expect("create git fixture");
    let status = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(path)
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init failed");
}

fn change_request_lookup(root: &std::path::Path) -> Arc<StaticChangeRequestLookup> {
    Arc::new(StaticChangeRequestLookup {
        source: CheckoutSourceContext {
            checkout_root: root.to_owned(),
            branch: BranchHeadContext::resolve(
                "feature/status",
                Some("origin/feature/status"),
                Some("origin"),
                Some("https://github.com/acme/zeron.git"),
            ),
            default_branch: Some("main".into()),
        },
        summary: ChangeRequestSummary {
            provider: "github".into(),
            number: 90,
            title: "Stream checkout pull request".into(),
            url: "https://github.com/acme/zeron/pull/90".into(),
            state: ChangeRequestState::Open,
            base_ref: "main".into(),
            head_ref: "feature/status".into(),
            signals: None,
        },
        resolves: AtomicUsize::new(0),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkout_change_request_accepts_a_local_chats_source_root() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let checkout = dirs.path().join("checkout");
    init_git_repo(&checkout);
    let app = checkout.join("app");
    std::fs::create_dir_all(&app).expect("app folder");
    let repo_root = std::fs::canonicalize(&checkout).expect("canonical checkout");

    let mut core = assemble(&dirs.path().join("b"), "device-b");
    core.change_requests = CheckoutChangeRequests::new(
        core.repos.clone(),
        "device-b",
        change_request_lookup(&checkout),
    );
    core.workspace
        .create_chat(
            "chat-outside-projects",
            None,
            Some("device-b"),
            None,
            Some(app.to_string_lossy().into_owned()),
        )
        .expect("chat");
    core.workspace
        .set_chat_source_context(
            "chat-outside-projects",
            &zeron_proto::ConversationSourceContext {
                checkout_id: "checkout".into(),
                repo_root: repo_root.to_string_lossy().into_owned(),
                cwd: app.to_string_lossy().into_owned(),
                branch: "feature/status".into(),
                head_sha: None,
                observed_at: chrono::Utc::now(),
            },
        )
        .expect("source context");
    let client = zeron_rpc::memory_client(core.rpc_service());

    let mut watch = client
        .subscribe_checked(
            methods::WATCH_CHECKOUT_CHANGE_REQUEST,
            serde_json::json!({ "cwd": repo_root }),
        )
        .await
        .expect("a local chat's source root is a known checkout");
    tokio::time::timeout(Duration::from_secs(5), watch.recv())
        .await
        .expect("frame before timeout")
        .expect("initial frame");

    std::fs::remove_dir_all(&checkout).expect("remove checkout");
    let rejected = match client
        .subscribe_checked(
            methods::WATCH_CHECKOUT_CHANGE_REQUEST,
            serde_json::json!({ "cwd": repo_root }),
        )
        .await
    {
        Ok(_) => panic!("a removed checkout must be rejected"),
        Err(error) => error,
    };
    assert!(rejected.to_string().contains("not a known checkout"));

    core.shutdown().await;
}

#[tokio::test]
async fn device_addressed_methods_reject_remote_targets_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), "device-local");
    let rpc = core.rpc_service();
    for method in [
        methods::APPLY_UPDATE,
        methods::CLOSE_TERMINAL,
        methods::CREATE_WORKSPACE_ENTRY,
        methods::CREATE_WORKTREE,
        methods::DELETE_PROJECT_ACTION,
        methods::DELETE_WORKSPACE_ENTRY,
        methods::DELETE_WORKTREE,
        methods::DEVICE_STATS,
        methods::GET_WORKTREE_SETTINGS,
        methods::LIST_FOLDERS,
        methods::LIST_HARNESSES,
        methods::LIST_PROJECT_ACTIONS,
        methods::LIST_WORKSPACE_DIRECTORY,
        methods::MOVE_WORKSPACE_ENTRY,
        methods::MUTATE,
        methods::OPEN_TERMINAL,
        methods::QUEUE_COMMAND,
        methods::QUEUE_MESSAGE,
        methods::READ_ATTACHMENT_CHUNK,
        methods::READ_CHAT_IMAGE_CHUNK,
        methods::READ_WORKSPACE_FILE,
        methods::READ_WORKSPACE_IMAGE,
        methods::RESIZE_TERMINAL,
        methods::RUN_PROJECT_ACTION,
        methods::SEARCH_WORKSPACE_CONTENT,
        methods::SEARCH_WORKSPACE_FILES,
        methods::SEND_QUEUED_MESSAGE_NOW,
        methods::SET_WORKTREE_SETTINGS,
        methods::SUBSCRIBE_TERMINAL,
        methods::UPDATE_STATUS,
        methods::UPLOAD_CHUNK,
        methods::UPLOAD_COMMIT,
        methods::UPSERT_PROJECT_ACTION,
        methods::WARM_START,
        methods::WARM_WORKSPACE_SEARCH,
        methods::WATCH_CHECKOUT_CHANGE_REQUEST,
        methods::WATCH_DOC_MESSAGES,
        methods::WATCH_QUEUE,
        methods::WATCH_WORKSPACE_FILES,
        methods::WATCH_WORKSPACE_GIT_STATUS,
        methods::WRITE_TERMINAL,
        methods::WRITE_WORKSPACE_FILE,
    ] {
        let error = match rpc
            .handle(
                method,
                serde_json::json!({
                    "targetDeviceId": "device-remote",
                    "op": "createChat", "chatId": "must-not-exist",
                    "cwd": dir.path(), "path": "must-not-be-written", "text": "must-not-run",
                }),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("{method} accepted a remote target"),
        };
        assert!(
            matches!(error, zeron_rpc::RpcError::BadParams(ref message)
            if message == "Only the local device is supported"),
            "{method}: {error}"
        );
    }
    assert!(core.workspace.chat_config("must-not-exist").is_none());
    assert!(!dir.path().join("must-not-be-written").exists());
    core.shutdown().await;
}

#[tokio::test]
async fn remote_streams_are_rejected_and_local_targets_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), "device-local");
    let client = zeron_rpc::memory_client(core.rpc_service());
    for method in [
        methods::WATCH_QUEUE,
        methods::WATCH_CHECKOUT_CHANGE_REQUEST,
        methods::WATCH_WORKSPACE_FILES,
        methods::WATCH_DOC_MESSAGES,
    ] {
        let error = match client
            .subscribe_checked(
                method,
                serde_json::json!({
                    "targetDeviceId": "device-remote", "chatId": "missing", "cwd": dir.path(),
                }),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("{method} opened a remote stream"),
        };
        assert!(
            error
                .to_string()
                .contains("Only the local device is supported"),
            "{method}: {error}"
        );
    }
    let expected = client
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .unwrap();
    for target in [serde_json::Value::Null, serde_json::json!("device-local")] {
        let local = client
            .call(
                methods::LIST_HARNESSES,
                serde_json::json!({"targetDeviceId": target}),
            )
            .await
            .unwrap();
        assert_eq!(local, expected);
    }
    core.shutdown().await;
}
