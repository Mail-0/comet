use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use std::time::Duration;
use tokio::sync::watch;
use zeron_proto::Space;
use zeron_rpc::{connect_ws, methods};

pub async fn watch_workspaces(port: u16, state: watch::Sender<Json>) {
    state.send_replace(json!({"status": "Connecting to local workspace…"}));
    if let Err(error) = watch_spaces(port, &state).await {
        state.send_replace(json!({"status": "Workspace unavailable", "error": error.to_string()}));
    }
}

async fn watch_spaces(port: u16, state: &watch::Sender<Json>) -> Result<()> {
    let (_client, mut spaces) = tokio::time::timeout(Duration::from_secs(5), async {
        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
        let spaces = client
            .subscribe_checked(methods::WATCH_SPACES, json!({}))
            .await?;
        Ok::<_, zeron_rpc::RpcError>((client, spaces))
    })
    .await
    .context("local workspace did not answer")??;
    let mut item = tokio::time::timeout(Duration::from_secs(5), spaces.recv())
        .await
        .context("local workspace did not send its first snapshot")?;
    while let Some(snapshot) = item {
        let rows: Vec<Space> =
            serde_json::from_value(snapshot).context("invalid workspace snapshot")?;
        let names: Vec<_> = rows.iter().map(Space::display_name).collect();
        state.send_replace(json!({"status": "Local workspace connected", "names": names}));
        item = spaces.recv().await;
    }
    anyhow::bail!("local workspace disconnected")
}
