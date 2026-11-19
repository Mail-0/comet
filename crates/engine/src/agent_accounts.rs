//! Compatibility replies for the retired CLI-account RPC surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeron_proto::{AgentAccountsSnapshot, AgentLoginPoll, AgentLoginStart, HarnessId};

use crate::EngineError;

#[derive(Clone)]
pub struct AgentAccountsConfig {
    pub data_dir: PathBuf,
}

impl AgentAccountsConfig {
    pub fn detect(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn isolated(data_dir: &Path) -> Self {
        Self::detect(data_dir)
    }
}

pub type CaptureSink = Arc<
    dyn Fn(serde_json::Value) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

#[derive(serde::Serialize)]
pub struct RelayedCallback {
    pub status: u16,
    pub location: Option<String>,
}

#[derive(Clone)]
pub struct AgentAccounts {
    snapshots: tokio::sync::watch::Sender<Option<AgentAccountsSnapshot>>,
}

fn unavailable() -> EngineError {
    EngineError::Other("Agent CLI accounts are unavailable. Copilot uses Keiki sign-in.".into())
}

impl AgentAccounts {
    pub fn new(_config: AgentAccountsConfig) -> Self {
        let (snapshots, _) = tokio::sync::watch::channel(Some(AgentAccountsSnapshot::default()));
        Self { snapshots }
    }

    pub fn with_callback_routes<T>(config: AgentAccountsConfig, _routes: T) -> Self {
        Self::new(config)
    }

    pub fn watch_snapshots(&self) -> tokio::sync::watch::Receiver<Option<AgentAccountsSnapshot>> {
        self.snapshots.subscribe()
    }

    pub async fn list(&self, _force: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        Ok(AgentAccountsSnapshot::default())
    }

    pub async fn activate(
        &self,
        _harness: HarnessId,
        _account: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(unavailable())
    }

    pub async fn forget(
        &self,
        _harness: HarnessId,
        _account: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(unavailable())
    }

    pub async fn add_api_key(
        &self,
        _harness: HarnessId,
        _provider: &str,
        _key: &str,
        _name: Option<&str>,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(unavailable())
    }

    pub async fn rename_account(
        &self,
        _harness: HarnessId,
        _account: &str,
        _name: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(unavailable())
    }

    pub async fn start_login_with(
        &self,
        _harness: HarnessId,
        _provider: Option<&str>,
        _requester: Option<&str>,
    ) -> Result<AgentLoginStart, EngineError> {
        Err(unavailable())
    }

    pub async fn start_capture_login(
        &self,
        _harness: HarnessId,
        _capture: CaptureSink,
    ) -> Result<AgentLoginStart, EngineError> {
        Err(unavailable())
    }

    pub async fn complete_login(
        &self,
        _id: &str,
        _code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(unavailable())
    }

    pub async fn poll_login(&self, _id: &str) -> Result<AgentLoginPoll, EngineError> {
        Err(unavailable())
    }

    pub async fn relay_callback(
        &self,
        _id: &str,
        _requester: &str,
        _path: &str,
    ) -> Result<RelayedCallback, EngineError> {
        Err(unavailable())
    }

    pub fn cancel_login(&self, _id: &str) {}
    pub fn shutdown(&self) {}
}

pub(crate) fn tunnel_port_allowed(_port: u16, _url: Option<&str>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_accounts_are_empty_and_mutations_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(AgentAccountsConfig::isolated(dir.path()));
        assert_eq!(
            accounts.list(false).await.unwrap(),
            AgentAccountsSnapshot::default()
        );
        assert!(accounts.activate(HarnessId::Copilot, "old").await.is_err());
        assert!(
            accounts
                .start_login_with(HarnessId::Unknown("codex"), None, None)
                .await
                .is_err()
        );
    }
}
