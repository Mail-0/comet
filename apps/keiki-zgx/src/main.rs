mod account;

use std::{cell::RefCell, rc::Rc, time::Duration};

use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, watch};
use zeron_proto::Space;
use zeron_rpc::{connect_ws, methods};
use zgx_desktop::{Application, WindowOptions};
use zgx_script::{Bundle, ScriptHost, Value};

static UI: Bundle = zgx_script::include_bundle!("keiki");

fn main() -> Result<()> {
    let port = std::env::var("KEIKI_PREVIEW_IPC_PORT")
        .ok()
        .map(|port| {
            port.parse::<u16>()
                .context("invalid KEIKI_PREVIEW_IPC_PORT")
        })
        .transpose()?;
    let smoke = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--smoke") if port.is_some() => true,
        _ => anyhow::bail!("usage: keiki-zgx [--smoke (requires KEIKI_PREVIEW_IPC_PORT)]"),
    };
    let api_url = std::env::var("KEIKI_API_URL").unwrap_or_else(|_| "https://onkeiki.com".into());
    let runtime = tokio::runtime::Runtime::new()?;
    let (commands, command_rx) = mpsc::channel(1);
    let (account_tx, account_rx) = watch::channel(account::initial());
    runtime.spawn(account::run(api_url, command_rx, account_tx));
    let (workspace_tx, workspace_rx) = watch::channel(json!({"status": "No workspace connected"}));
    if let Some(port) = port {
        runtime.spawn(watch_workspaces(port, workspace_tx.clone()));
    }
    let failure = Rc::new(RefCell::new(None));
    let window_failure = failure.clone();
    Application::new()
        .window(WindowOptions {
            title: "Keiki Desktop Preview".into(),
            width: 900.,
            height: 630.,
            ..Default::default()
        })
        .run(move |cx| {
            let host = ScriptHost::bundled(&cx.ui.runtime, &UI, "app.luau").app_id("keiki-preview");
            for (action, command) in [
                ("sign_in", account::Command::sign_in as fn(String) -> _),
                ("sign_out", account::Command::sign_out),
                ("choose_org", account::Command::ChooseOrg),
                ("choose_agent", account::Command::ChooseAgent),
                ("send", account::Command::Send),
            ] {
                let commands = commands.clone();
                let account = account_rx.clone();
                host.action(action, move |payload| {
                    if account.borrow()["busy"] == true {
                        return;
                    }
                    let text = match payload {
                        Value::Str(text) => text.to_string(),
                        _ => String::new(),
                    };
                    let _ = commands.try_send(command(text));
                });
            }
            for (key, mut rx) in [
                ("keiki.account", account_rx.clone()),
                ("keiki.workspace", workspace_rx.clone()),
            ] {
                let value = host.shared(key, script_value(&rx.borrow_and_update()));
                cx.tasks.spawn(async move {
                    while rx.changed().await.is_ok() {
                        value.set(script_value(&rx.borrow_and_update()));
                    }
                });
            }
            if let Err(error) = host.reload() {
                *window_failure.borrow_mut() = Some(error);
                cx.window.close();
                return;
            }
            cx.render(host.view());
            if smoke {
                let mut workspace = workspace_rx.clone();
                let window = cx.window.clone();
                let failure = window_failure.clone();
                cx.tasks.spawn(async move {
                    loop {
                        let state = workspace.borrow_and_update().clone();
                        if state["status"] == "Local workspace connected" {
                            println!("Keiki preview connected to the Comet workspace watch");
                            break;
                        }
                        if let Some(error) = state["error"].as_str() {
                            *failure.borrow_mut() = Some(error.to_string());
                            break;
                        }
                        if workspace.changed().await.is_err() {
                            *failure.borrow_mut() = Some("workspace watch stopped".into());
                            break;
                        }
                    }
                    window.close();
                });
            }
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(error) = failure.borrow_mut().take() {
        anyhow::bail!("{error}");
    }
    Ok(())
}

fn script_value(json: &Json) -> Value {
    Value::from_json(&json.to_string()).expect("serialize preview state")
}

async fn watch_workspaces(port: u16, state: watch::Sender<Json>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use zgx_script::testing::TestApp;

    #[test]
    fn bundled_shell_has_no_zgx_lint_findings() {
        let diagnostics = zgx_script::lint::lint_source(
            std::path::Path::new("ui/app.luau"),
            include_str!("../ui/app.luau"),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn bundled_shell_updates_agent_list_and_sends_the_composer_text() {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let (mut account, mut workspace) = (None, None);
        let mut app = TestApp::new(|runtime| {
            let host = ScriptHost::bundled(runtime, &UI, "app.luau");
            account = Some(host.shared("keiki.account", script_value(&account::initial())));
            workspace = Some(host.shared("keiki.workspace", script_value(&json!({}))));
            let sent = sent.clone();
            host.action("send", move |value| sent.borrow_mut().push(value));
            host
        });
        let account = account.unwrap();
        let workspace = workspace.unwrap();
        workspace.set(script_value(&json!({"names": ["A local workspace"]})));
        account.set(script_value(&json!({
            "signed_in": true,
            "agents": [{"id": "old", "name": "Previous org agent"}],
        })));
        let texts = app.texts();
        assert!(texts.iter().any(|text| text == "A local workspace"));
        assert!(texts.iter().any(|text| text == "Previous org agent"));

        account.set(script_value(&json!({
            "signed_in": true,
            "agents": [{"id": "current", "name": "Current org agent"}],
            "agent_id": "current",
            "reply": "Partial response",
        })));
        let texts = app.texts();
        assert!(!texts.iter().any(|text| text == "Previous org agent"));
        assert!(texts.iter().any(|text| text == "Current org agent"));
        assert_eq!(app.text("reply"), "Partial response");
        app.type_text("message", "Hello from Keiki");
        app.click("send");
        assert!(
            matches!(sent.borrow().as_slice(), [Value::Str(text)] if &**text == "Hello from Keiki")
        );
        app.click("send");
        assert_eq!(sent.borrow().len(), 1);
        assert!(app.host.error().get().is_none());
    }
}
