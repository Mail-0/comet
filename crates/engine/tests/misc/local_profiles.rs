//! Integrated regressions for local-first profile privacy and lifecycle boundaries.

use std::path::Path;
use std::sync::{Arc, Barrier};

use zeron_engine::{
    Engine, EngineConfig, EngineCore, EngineProfile, HarnessId, WorkspaceScope,
    default_registry,
};

fn config(data_dir: &Path) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
    }
}

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

async fn shutdown(core: EngineCore) {
    core.shutdown().await;
    drop(core);
}

fn concurrent_engine_info(
    config: Arc<EngineConfig>,
    workers: usize,
) -> std::collections::HashSet<String> {
    let barrier = Arc::new(Barrier::new(workers));
    (0..workers)
        .map(|_| {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Engine::engine_info(&config, WorkspaceScope::Local)
                    .expect("resolve concurrent engine info")
                    .device_id
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|call| call.join().expect("engine-info worker"))
        .collect()
}

#[tokio::test]
async fn concurrent_engine_info_and_runtime_share_one_device_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(config(dir.path()));
    let announced = concurrent_engine_info(config, 32);

    assert_eq!(announced.len(), 1, "every viewport announces one identity");
    let announced = announced.into_iter().next().expect("announced id");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("persisted device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(
        core.device_id, announced,
        "the assembled runtime must use the identity already announced"
    );
    shutdown(core).await;
}

#[tokio::test]
async fn empty_legacy_device_identity_is_repaired_once_for_all_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("device-id"), b"").expect("seed truncated identity");
    let config = Arc::new(config(dir.path()));

    let announced = concurrent_engine_info(config, 32);
    assert_eq!(
        announced.len(),
        1,
        "legacy repair must publish one identity"
    );
    let announced = announced.into_iter().next().expect("repaired id");
    assert!(!announced.trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("repaired device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(core.device_id, announced);
    shutdown(core).await;
}


#[tokio::test]
async fn retired_scopes_cannot_open_or_modify_account_stores() {
    let dir = tempfile::tempdir().unwrap();
    let archived = dir.path().join("orgs/old-org/old-user");
    std::fs::create_dir_all(archived.join("uploads")).unwrap();
    let sentinel = archived.join("docs.sqlite3");
    std::fs::write(&sentinel, b"untouched account store").unwrap();
    let upload = archived.join("uploads/kept.bin");
    std::fs::write(&upload, b"untouched upload").unwrap();
    let config = config(dir.path());
    let auth = Engine::build_auth(&config).await;
    for scope in [WorkspaceScope::Synced, WorkspaceScope::Development] {
        assert!(Engine::resolve_profile(&config, &auth, scope).is_err());
    }
    let local = Engine::resolve_profile(&config, &auth, WorkspaceScope::Local).unwrap().unwrap();
    let core = assemble(local);
    assert_eq!(core.workspace_scope(), WorkspaceScope::Local);
    shutdown(core).await;
    let legacy_constructor = EngineCore::assemble_with_identity(
        dir.path(), Arc::new(default_registry()), HarnessId::Mock, None, "old-org", "old-user",
    ).unwrap();
    assert_eq!(legacy_constructor.workspace_scope(), WorkspaceScope::Local);
    shutdown(legacy_constructor).await;
    assert_eq!(std::fs::read(sentinel).unwrap(), b"untouched account store");
    assert_eq!(std::fs::read(upload).unwrap(), b"untouched upload");
    assert!(!archived.join("local-profile.json").exists());
    assert!(!dir.path().join("legacy-uploads-owner.json").exists());
}
