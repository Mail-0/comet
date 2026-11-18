mod account;
mod bridge;
mod changes;
mod composer;
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
use serde_json::Value as Json;
use tokio::sync::{mpsc, watch};
use zgx_desktop::{Application, ApplicationEvent, WindowOptions};
use zgx_script::{Bundle, ScriptHost, Value};

static UI: Bundle = zgx_script::include_bundle!("keiki");

pub fn run_app(config: UiConfig) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let (commands, command_rx) = mpsc::channel(1);
    let (account_tx, account_rx) = watch::channel(account::initial());
    let account_task = runtime.spawn(account::run(config.keiki_api_url, command_rx, account_tx));
    let (workspace_tx, workspace_rx) = watch::channel(workspace::initial());
    let (stop, stopped) = watch::channel(false);
    let workspace_task = runtime.spawn(workspace::run(
        zeron_engine::EngineConfig {
            data_dir: config.data_dir,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
        },
        workspace_tx,
        stopped.clone(),
    ));
    let signal_stop = stop.clone();
    let signal_task = runtime.spawn(async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "Could not install desktop shutdown signal");
            return;
        }
        signal_stop.send_replace(true);
    });
    let route = std::env::var("ZERON_OPEN_ROUTE").ok();
    let settings = Rc::new(RefCell::new(settings::Settings::new(
        route.as_deref(),
        config.initial_url.as_deref(),
    )));
    let (settings_tx, settings_rx) = watch::channel(settings.borrow().snapshot());
    let event_settings = settings.clone();
    let failure = Rc::new(RefCell::new(None));
    let window_failure = failure.clone();
    let application = Application::new()
        .application_id("sh.zeron.app")?
        .desktop_id("zeron")
        .on_application_event(move |event, _| {
            if let ApplicationEvent::OpenUrls(urls) = event {
                for url in urls {
                    if event_settings.borrow_mut().open_url(&url) {
                        settings_tx.send_replace(event_settings.borrow().snapshot());
                    }
                }
            }
        })
        .window(WindowOptions {
            title: "Keiki".into(),
            width: 900.,
            height: 630.,
            ..Default::default()
        })
        .run(move |cx| {
            let host = ScriptHost::bundled(&cx.ui.runtime, &UI, "app.luau").app_id("sh.zeron.app");
            let registration = bridge::Registration {
                host: &host,
                tasks: &cx.tasks,
                commands: &commands,
                account: &account_rx,
                workspace: &workspace_rx,
                settings: &settings_rx,
            };
            account::register(&registration);
            conversations::register(&registration);
            transcript::register(&registration);
            composer::register(&registration);
            workspace::register(&registration);
            copilot::register(&registration);
            sandbox::register(&registration);
            changes::register(&registration);
            settings::register(&registration);
            updater::register(&registration);
            if let Err(error) = host.reload() {
                *window_failure.borrow_mut() = Some(error);
                cx.window.close();
                return;
            }
            cx.render(host.view());
            tracing::info!("main window opened");
            let window = cx.window.clone();
            let mut stopped = stopped.clone();
            cx.tasks.spawn(async move {
                if stopped.changed().await.is_ok() {
                    window.close();
                }
            });
        })
        .map_err(|error| anyhow::anyhow!("{error}"));
    stop.send_replace(true);
    runtime.block_on(workspace_task)?;
    account_task.abort();
    signal_task.abort();
    application?;
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
    use serde_json::json;
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
            composer::register_host(&host);
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

        account.set(script_value(&json!({
            "signed_in": false, "busy": false, "agents": [],
            "error": "Session unavailable",
        })));
        let texts = app.texts();
        assert!(texts.iter().any(|text| text == "Session unavailable"));
        assert!(!texts.iter().any(|text| text == "Current org agent"));
        assert!(app.host.error().get().is_none());
    }
}
