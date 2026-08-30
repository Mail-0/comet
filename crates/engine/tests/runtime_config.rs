use zeron_engine::{Engine, EngineConfig, EngineProfile};
use zeron_proto::{HarnessId, RunRequest, SandboxLevel};

#[tokio::test]
async fn runtime_uses_configured_default_harness() {
    let temp = tempfile::tempdir().expect("temporary data directory");
    let config = EngineConfig {
        data_dir: temp.path().join("data"),
        ipc_port: 0,
        default_harness: HarnessId::Copilot,
    };
    let profile = EngineProfile::local(&config.data_dir).expect("local profile");
    let runtime = Engine::assemble_runtime(&config, profile)
        .await
        .expect("runtime assembles");

    assert_eq!(
        runtime.core().doc_host.harness_for_request(
            "unconfigured-chat",
            &RunRequest {
                prompt: "hello".into(),
                harness: None,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                cwd: "/".into(),
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
                worktree: None,
            },
        ),
        HarnessId::Copilot
    );
}
