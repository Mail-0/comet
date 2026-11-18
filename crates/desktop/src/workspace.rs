use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::watch;
use zeron_engine::{Engine, EngineConfig, EngineProfile, EngineRuntime, InstanceLock};
use zeron_proto::{EngineInfo, Space};
use zeron_rpc::{RpcClient, RpcError, RpcService, connect_ws, memory_client, methods};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct EngineHandle {
    client: RpcClient,
    runtime: Option<EngineRuntime>,
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    info: EngineInfo,
}

impl EngineHandle {
    pub async fn bootstrap(config: EngineConfig) -> Result<Self> {
        static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _gate = GATE.lock().await;
        if let Ok(Ok((client, info))) = tokio::time::timeout(PROBE_TIMEOUT, async {
            let client = connect_ws(&format!("ws://127.0.0.1:{}", config.ipc_port)).await?;
            let info = engine_info(&client).await?;
            Ok::<_, RpcError>((client, info))
        })
        .await
        {
            return Ok(Self {
                client,
                runtime: None,
                ipc_task: None,
                info,
            });
        }
        std::fs::create_dir_all(&config.data_dir)?;
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let profile = EngineProfile::local(&config.data_dir)?;
        let info = Engine::engine_info(&config)?;
        let runtime = Engine::assemble_runtime_with_lock(&config, profile, lock).await?;
        let service: Arc<dyn RpcService> = runtime.core().rpc_service();
        let client = memory_client(service.clone());
        let ipc_task = match zeron_engine::serve_ipc(config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(error) => {
                tracing::warn!(%error, "IPC unavailable; using embedded transport");
                None
            }
        };
        Ok(Self {
            client,
            runtime: Some(runtime),
            ipc_task,
            info,
        })
    }

    pub fn mode(&self) -> &'static str {
        if self.runtime.is_some() {
            "embedded"
        } else {
            "daemon"
        }
    }

    pub async fn shutdown(self) {
        if let Some(task) = self.ipc_task {
            task.abort();
        }
        if let Some(runtime) = self.runtime {
            runtime.shutdown().await;
        }
    }
}

async fn engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
    match client.call_as(methods::ENGINE_INFO, json!({})).await {
        Err(RpcError::UnknownMethod(method)) if method == methods::ENGINE_INFO => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct LocalDevice {
                device_id: String,
            }
            let device: LocalDevice = client.call_as(methods::LOCAL_DEVICE, json!({})).await?;
            Ok(EngineInfo {
                device_id: device.device_id,
            })
        }
        result => result,
    }
}

pub fn initial() -> Value {
    json!({"status": "Connecting to local workspace…", "names": []})
}

pub async fn run(
    config: EngineConfig,
    state: watch::Sender<Value>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            return;
        }
        let boot = EngineHandle::bootstrap(config.clone()).await;
        match boot {
            Ok(handle) => {
                if *stop.borrow() {
                    handle.shutdown().await;
                    return;
                }
                let result = tokio::select! {
                    result = watch_spaces(&handle, &state) => result,
                    _ = stop.changed() => {
                        handle.shutdown().await;
                        tracing::info!("desktop engine shutdown drained");
                        return;
                    }
                };
                handle.shutdown().await;
                state.send_replace(json!({
                    "status": "Workspace unavailable; reconnecting…",
                    "error": result.err().map(|error| error.to_string()), "names": [],
                }));
            }
            Err(error) => {
                state.send_replace(json!({
                    "status": "Workspace unavailable; reconnecting…",
                    "error": error.to_string(), "names": [],
                }));
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            _ = stop.changed() => return,
        }
    }
}

async fn watch_spaces(handle: &EngineHandle, state: &watch::Sender<Value>) -> Result<()> {
    let mut spaces = tokio::time::timeout(
        PROBE_TIMEOUT,
        handle
            .client
            .subscribe_checked(methods::WATCH_SPACES, json!({})),
    )
    .await
    .context("workspace watch did not answer")??;
    let mut item = tokio::time::timeout(PROBE_TIMEOUT, spaces.recv())
        .await
        .context("workspace did not send its first snapshot")?;
    while let Some(snapshot) = item {
        let rows: Vec<Space> = serde_json::from_value(snapshot)?;
        state.send_replace(json!({
            "status": "Local workspace connected",
            "mode": handle.mode(), "device_id": handle.info.device_id,
            "names": rows.iter().map(Space::display_name).collect::<Vec<_>>(),
        }));
        tracing::info!(mode = handle.mode(), "desktop workspace connected");
        item = spaces.recv().await;
    }
    anyhow::bail!("local workspace disconnected")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::HarnessId;

    fn config(data_dir: &std::path::Path) -> EngineConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        EngineConfig {
            data_dir: data_dir.into(),
            ipc_port: listener.local_addr().unwrap().port(),
            default_harness: HarnessId::Copilot,
        }
    }

    #[tokio::test]
    async fn embeds_attaches_and_drains_without_stopping_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let embedded = EngineHandle::bootstrap(config.clone()).await.unwrap();
        assert_eq!(embedded.mode(), "embedded");
        let remote = EngineHandle::bootstrap(config.clone()).await.unwrap();
        assert_eq!(remote.mode(), "daemon");
        assert_eq!(remote.info.device_id, embedded.info.device_id);
        remote.shutdown().await;
        assert!(engine_info(&embedded.client).await.is_ok());
        embedded.shutdown().await;
        let replacement = EngineHandle::bootstrap(config).await.unwrap();
        assert_eq!(replacement.mode(), "embedded");
        replacement.shutdown().await;
    }

    #[tokio::test]
    async fn workspace_publishes_empty_snapshot_and_stops_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let (state, mut snapshots) = watch::channel(initial());
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(config, state, stopped));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots.borrow()["status"] == "Local workspace connected" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow()["mode"], "embedded");
        assert_eq!(snapshots.borrow()["names"], json!([]));
        stop.send_replace(true);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn workspace_reports_locked_profile_and_can_be_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let _lock = InstanceLock::acquire(dir.path()).unwrap();
        let (state, mut snapshots) = watch::channel(initial());
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(config, state, stopped));
        tokio::time::timeout(Duration::from_secs(10), snapshots.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(snapshots.borrow()["error"].is_string());
        stop.send_replace(true);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn workspace_reconnects_and_embeds_after_daemon_loss() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let daemon_config = config.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (exit, exited) = tokio::sync::oneshot::channel();
        let daemon = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let engine = EngineHandle::bootstrap(daemon_config).await.unwrap();
                ready.send(()).unwrap();
                exited.await.unwrap();
                engine.shutdown().await;
            });
        });
        started.await.unwrap();
        let (state, mut snapshots) = watch::channel(initial());
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(config, state, stopped));
        tokio::time::timeout(Duration::from_secs(10), async {
            while snapshots.borrow()["mode"] != "daemon" {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        exit.send(()).unwrap();
        daemon.join().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while snapshots.borrow()["mode"] != "embedded" {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        stop.send_replace(true);
        task.await.unwrap();
    }
}
