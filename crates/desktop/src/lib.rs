mod account;
mod conversations;
mod copilot;
mod sandbox;
mod settings;
mod transcript;
mod updater;
mod workspace;

use std::path::PathBuf;
pub use zeron_proto::HarnessId;

#[derive(Clone)]
pub struct UiConfig {
    pub data_dir: PathBuf,
    pub ipc_port: u16,
    pub default_harness: HarnessId,
    pub initial_url: Option<String>,
    pub keiki_api_url: String,
}

use std::{cell::RefCell, rc::Rc};

use anyhow::Result;
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, watch};
use zgx_desktop::{Application, WindowOptions};
use zgx_script::{Bundle, ScriptHost, Value};

static UI: Bundle = zgx_script::include_bundle!("keiki");

pub fn run_app(_config: UiConfig) -> Result<()> {
    let port = Some(_config.ipc_port);
    let api_url = _config.keiki_api_url;
    let runtime = tokio::runtime::Runtime::new()?;
    let (commands, command_rx) = mpsc::channel(1);
    let (account_tx, account_rx) = watch::channel(account::initial());
    runtime.spawn(account::run(api_url, command_rx, account_tx));
    let (workspace_tx, workspace_rx) = watch::channel(json!({"status": "No workspace connected"}));
    if let Some(port) = port {
        runtime.spawn(workspace::watch_workspaces(port, workspace_tx.clone()));
    }
    let failure = Rc::new(RefCell::new(None));
    let window_failure = failure.clone();
    Application::new()
        .window(WindowOptions {
            title: "Keiki".into(),
            width: 900.,
            height: 630.,
            ..Default::default()
        })
        .run(move |cx| {
            let host = ScriptHost::bundled(&cx.ui.runtime, &UI, "app.luau").app_id("sh.zeron.app");
            for (action, command) in [
                (
                    "account.sign_in",
                    account::Command::sign_in as fn(String) -> _,
                ),
                ("account.sign_out", account::Command::sign_out),
                ("account.choose_org", account::Command::ChooseOrg),
                ("conversations.choose_agent", account::Command::ChooseAgent),
                ("conversations.send", account::Command::Send),
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
            for (key, project) in [
                (
                    "keiki.conversations",
                    conversations::snapshot as fn(&Json) -> Json,
                ),
                ("keiki.transcript", transcript::snapshot),
            ] {
                let mut rx = account_rx.clone();
                let value = host.shared(key, script_value(&project(&rx.borrow_and_update())));
                cx.tasks.spawn(async move {
                    while rx.changed().await.is_ok() {
                        value.set(script_value(&project(&rx.borrow_and_update())));
                    }
                });
            }
            for (key, initial) in [
                ("keiki.copilot", copilot::initial()),
                ("keiki.sandbox", sandbox::initial()),
                ("keiki.settings", settings::initial()),
                ("keiki.updater", updater::initial()),
            ] {
                host.shared(key, script_value(&initial));
            }
            if let Err(error) = host.reload() {
                *window_failure.borrow_mut() = Some(error);
                cx.window.close();
                return;
            }
            cx.render(host.view());
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(error) = failure.borrow_mut().take() {
        anyhow::bail!("{error}");
    }
    Ok(())
}

fn script_value(json: &Json) -> Value {
    Value::from_json(&json.to_string()).expect("serialize desktop state")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zgx_script::testing::TestApp;

    #[test]
    fn bundled_shell_has_no_zgx_lint_findings() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        for path in zgx_script::check::scripts(&root) {
            let source = std::fs::read_to_string(&path).unwrap();
            let diagnostics = zgx_script::lint::lint_source(&path, &source);
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        }
    }

    #[test]
    fn bundled_shell_updates_agent_list_and_sends_the_composer_text() {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let (mut account, mut workspace) = (None, None);
        let (mut conversations, mut transcript) = (None, None);
        let mut app = TestApp::new(|runtime| {
            let host = ScriptHost::bundled(runtime, &UI, "app.luau");
            account = Some(host.shared("keiki.account", script_value(&account::initial())));
            workspace = Some(host.shared("keiki.workspace", script_value(&json!({}))));
            conversations = Some(host.shared("keiki.conversations", script_value(&json!({}))));
            transcript = Some(host.shared("keiki.transcript", script_value(&json!({}))));
            let sent = sent.clone();
            host.action("conversations.send", move |value| {
                sent.borrow_mut().push(value)
            });
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
        conversations
            .unwrap()
            .set(script_value(&json!({"agent_id": "current"})));
        transcript
            .unwrap()
            .set(script_value(&json!({"reply": "Partial response"})));
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
