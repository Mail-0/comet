//! Local plugin services and rejection of remote device operations.
#![cfg(unix)]

use futures::StreamExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zeron_engine::{EngineCore, HarnessId, HarnessRegistry};
use zeron_rpc::{RpcClient, RpcReply, RpcService, methods};

struct Engine {
    core: EngineCore,
    client: RpcClient,
}

fn engine(dir: &Path, device: &str) -> Engine {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("device-id"), device).unwrap();
    let core =
        EngineCore::assemble(dir, Arc::new(HarnessRegistry::new()), HarnessId::Mock, None).unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    Engine { core, client }
}

const SERVICE: &str = r#"
local zeron = require("./zeron")("hello-devices")

zeron.serve("where", function(args, call)
	local ran = zeron.run("echo", { zeron.device.id }):await()
	return { device = zeron.device.id, from = call.from.id, echoed = ran.stdout }
end, { safe = true })

zeron.serve("shout", function(args)
	zeron.emit("shouted", { word = args.word, at = zeron.device.id })
	return true
end)
"#;

/// Its icon: listed with it, so another device's window can draw it.
const ICON: &str =
    r#"<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6" fill="currentColor"/></svg>"#;

/// The plugin, installed in `dir`'s engine as `version`, signed as yours.
async fn install(e: &Engine, dir: &Path, version: &str) {
    install_asking(e, dir, version, &[]).await;
}

/// As `install`, asking for `more` besides.
async fn install_asking(e: &Engine, dir: &Path, version: &str, more: &[&str]) {
    let folder = dir.join("plugins").join("hello-devices");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("init.luau"), "return nil").unwrap();
    std::fs::write(folder.join("service.luau"), SERVICE).unwrap();
    std::fs::write(
        folder.join("zeron.luau"),
        zeron_engine::plugins::scaffold::SDK,
    )
    .unwrap();
    std::fs::write(folder.join("icon.svg"), ICON).unwrap();
    let permissions: Vec<&str> = ["service", "commands"]
        .into_iter()
        .chain(more.iter().copied())
        .collect();
    let manifest = json!({
        "id": "hello-devices", "name": "Hello devices", "version": version, "entry": "init.luau",
        "service": "service.luau",
        "permissions": permissions,
        "commands": ["echo"],
        "icon": "icon.svg",
    });
    std::fs::write(
        folder.join("zeron-plugin.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    // As Zeron's consent window signs: with a review for it as listed.
    let review = review(e, "hello-devices").await;
    let signed = e
        .client
        .call(
            methods::SIGN_PLUGIN,
            json!({ "id": "hello-devices", "review": review }),
        )
        .await
        .unwrap();
    assert_eq!(signed["version"], version, "{signed}");
}

/// A consent-window review for plugin `id` as `e` lists it now.
async fn review(e: &Engine, id: &str) -> Value {
    let list = e
        .client
        .call(methods::LIST_PLUGINS, json!({}))
        .await
        .unwrap();
    let item = list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap();
    let minted = e
        .client
        .call(
            methods::REVIEW_PLUGIN,
            json!({ "id": id, "hash": item["hash"] }),
        )
        .await
        .unwrap();
    minted["review"].clone()
}

async fn allow(e: &Engine) {
    let list = e
        .client
        .call(methods::LIST_PLUGINS, json!({}))
        .await
        .unwrap();
    let item = list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "hello-devices")
        .cloned()
        .unwrap();
    let review = review(e, "hello-devices").await;
    let decided = e
        .client
        .call(
            methods::DECIDE_PLUGIN,
            json!({ "id": "hello-devices", "allow": true, "hash": item["hash"], "review": review }),
        )
        .await
        .unwrap();
    assert_eq!(decided["status"], "on", "{decided}");
}

async fn ask(e: &Engine, device: &str, method: &str, args: Value) -> Value {
    e.client
        .call(
            methods::CALL_PLUGIN_SERVICE,
            json!({"plugin": "hello-devices", "device": device, "method": method, "args": args}),
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_service_requires_consent_and_publishes_to_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), "device-local");
    install(&e, dir.path(), "1.0.0").await;
    let refused = ask(&e, "", "where", json!({})).await;
    assert_eq!(refused["reason"], "not_approved", "{refused}");
    allow(&e).await;
    let here = ask(&e, "", "where", json!({})).await;
    assert_eq!(here["ok"], true, "{here}");
    assert_eq!(here["value"]["device"], "device-local", "{here}");
    assert_eq!(here["value"]["echoed"], "device-local\n", "{here}");
    let remote = ask(&e, "device-remote", "where", json!({})).await;
    assert_eq!(remote["ok"], false, "{remote}");
    assert_eq!(remote["reason"], "offline", "{remote}");

    let reply = e
        .core
        .rpc_service()
        .handle(methods::WATCH_PLUGIN_CHANNEL, json!({}))
        .await
        .unwrap();
    let RpcReply::Stream(mut channel) = reply else {
        panic!("expected a channel stream")
    };
    assert_eq!(
        ask(&e, "", "shout", json!({"word": "local-only"})).await["ok"],
        true
    );
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = channel.next().await.expect("channel remains open");
            if event.to_string().contains("local-only") {
                break event;
            }
        }
    })
    .await
    .expect("local event");
    assert!(event.to_string().contains("device-local"), "{event}");
    drop(channel);
    e.core.shutdown().await;
}

#[tokio::test]
async fn remote_plugin_management_is_rejected_without_changing_local_consent() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), "device-local");
    install(&e, dir.path(), "1.0.0").await;
    let before = e
        .client
        .call(methods::LIST_PLUGINS, json!({}))
        .await
        .unwrap();
    let item = &before["plugins"][0];
    let local_review = review(&e, "hello-devices").await;
    for method in [
        methods::LIST_PLUGINS,
        methods::REVIEW_PLUGIN,
        methods::DECIDE_PLUGIN,
        methods::STAGE_PLUGIN_PACKAGE,
        methods::INSTALL_PLUGIN,
        methods::EXPORT_PLUGIN,
    ] {
        let error = e.client.call(method, json!({
            "targetDeviceId": "device-remote", "id": item["id"], "hash": item["hash"],
            "review": local_review, "allow": true, "data": "aGk=", "path": "/does-not-exist",
        })).await.expect_err("remote management is unavailable");
        assert!(
            error
                .to_string()
                .contains("Only the local device is supported"),
            "{method}: {error}"
        );
    }
    let after = e
        .client
        .call(methods::LIST_PLUGINS, json!({}))
        .await
        .unwrap();
    assert_eq!(after, before);
    allow(&e).await;
    assert_eq!(ask(&e, "", "where", json!({})).await["ok"], true);
    e.core.shutdown().await;
}
