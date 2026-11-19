//! Credential-free Keiki browsing; transcript delivery stays in `chat`.
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
};

use base64::Engine as _;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use zgx_script::{ScriptHost, Shared, Value};

use crate::{
    auth::text,
    engine::{
        self,
        rpc::{Rpc, Watch},
    },
    plugins::{json_value, value_json},
};

const CALLS: &[&str] = &[
    "ListKeikiAgents",
    "ListKeikiAgentGroups",
    "GetKeikiAgentGroupRoster",
    "ListKeikiConversations",
    "OpenKeikiConversation",
    "NewKeikiConversation",
    "LoadKeikiHistory",
    "SetKeikiTakeover",
    "KeikiConversationAction",
    "KeikiConversationInfo",
    "EndKeikiGroup",
    "CancelKeikiTask",
    "ListKeikiTemplates",
    "CreateKeikiAgent",
    "KeikiPresetConnection",
    "GetKeikiAvatar",
];

#[derive(Default)]
struct AvatarFiles {
    dir: Option<tempfile::TempDir>,
    files: VecDeque<PathBuf>,
}

impl AvatarFiles {
    fn source(&mut self, reply: &mut Json) -> Result<(), String> {
        let Some(uri) = reply.get("src").and_then(Json::as_str) else {
            return Ok(());
        };
        let (kind, encoded) = uri.split_once(";base64,").ok_or("Invalid Keiki avatar")?;
        let extension = match kind {
            "data:image/gif" => "gif",
            "data:image/png" => "png",
            "data:image/jpeg" => "jpg",
            _ => return Err("Invalid Keiki avatar".into()),
        };
        if encoded.len() > 1_398_104 {
            return Err("Keiki avatar is too large".into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid Keiki avatar")?;
        if self.dir.is_none() {
            self.dir = Some(
                tempfile::Builder::new()
                    .prefix("keiki-avatars-")
                    .tempdir()
                    .map_err(|_| "Avatar cache is unavailable")?,
            );
        }
        let path = self
            .dir
            .as_ref()
            .unwrap()
            .path()
            .join(format!("{:x}.{extension}", Sha256::digest(&bytes)));
        if let Some(index) = self.files.iter().position(|file| *file == path) {
            self.files.remove(index);
        } else {
            std::fs::write(&path, bytes).map_err(|_| "Avatar cache is unavailable")?;
            if self.files.len() == 64 {
                if let Some(old) = self.files.pop_front() {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
        self.files.push_back(path.clone());
        reply["src"] = json!(path.to_string_lossy());
        Ok(())
    }
}

struct View {
    rpc: RefCell<Option<Rpc>>,
    sidebar: Shared,
    conversation: Shared,
    sidebar_params: RefCell<Option<Json>>,
    chat: RefCell<Option<String>>,
    watches: RefCell<HashMap<&'static str, Watch>>,
}

impl View {
    fn subscribe(&self, method: &'static str) {
        self.watches.borrow_mut().remove(method);
        let Some(rpc) = self.rpc.borrow().clone() else {
            return;
        };
        let (params, shared) = if method == "WatchKeikiSidebar" {
            (self.sidebar_params.borrow().clone(), self.sidebar.clone())
        } else {
            (
                self.chat
                    .borrow()
                    .as_ref()
                    .map(|chat| json!({"chatId":chat})),
                self.conversation.clone(),
            )
        };
        let Some(params) = params else { return };
        let ended = shared.clone();
        self.watches.borrow_mut().insert(
            method,
            rpc.watch_ending(
                method,
                params,
                move |item| {
                    shared.set(json_value(&item));
                },
                move |_| {
                    ended.set(json_value(
                        &json!({"error":"Keiki connection ended. Retry to reconnect."}),
                    ));
                },
            ),
        );
    }
}

pub fn install(host: &ScriptHost) {
    host.shared("keiki.available", Value::Bool(true));
    let view = Rc::new(View {
        rpc: RefCell::default(),
        sidebar: host.shared("keiki.sidebar", Value::Nil),
        conversation: host.shared("keiki.conversation", Value::Nil),
        sidebar_params: RefCell::default(),
        chat: RefCell::default(),
        watches: RefCell::default(),
    });
    let weak = Rc::downgrade(&view);
    engine::get().on_connect(move |rpc| {
        if let Some(view) = weak.upgrade() {
            *view.rpc.borrow_mut() = Some(rpc.clone());
            view.subscribe("WatchKeikiSidebar");
            view.subscribe("WatchKeikiConversation");
        }
    });
    let v = view.clone();
    host.action("keiki.sidebar", move |params| {
        if crate::plugins::preview::refuse_aloud("keiki.sidebar") {
            return;
        }
        *v.sidebar_params.borrow_mut() =
            (!matches!(params, Value::Nil)).then(|| value_json(&params));
        if v.sidebar_params.borrow().is_none() {
            v.sidebar.set(Value::Nil);
        }
        v.subscribe("WatchKeikiSidebar");
    });
    host.action("keiki.conversation", move |id| {
        if crate::plugins::preview::refuse_aloud("keiki.conversation") {
            return;
        }
        let id = text(&id);
        *view.chat.borrow_mut() = (!id.is_empty()).then_some(id);
        view.conversation.set(Value::Nil);
        view.subscribe("WatchKeikiConversation");
    });
    let avatars = Rc::new(RefCell::new(AvatarFiles::default()));
    host.async_action("keiki.call", move |args| {
        let method = args.field("method").map(text).unwrap_or_default();
        let params = args.field("params").map(value_json).unwrap_or(json!({}));
        let avatars = avatars.clone();
        async move {
            crate::plugins::preview::refuse("keiki.call")?;
            if !CALLS.contains(&method.as_str()) || !params.is_object() {
                return Err("Unsupported Keiki request".into());
            }
            let mut reply = engine::get()
                .call_soon(&method, params)
                .await
                .map_err(|_| {
                    String::from("Could not reach Keiki. Check your connection and try again.")
                })?;
            if method == "GetKeikiAvatar" {
                avatars.borrow_mut().source(&mut reply)?;
            }
            Ok(json_value(&reply))
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{AvatarFiles, CALLS};
    use base64::Engine as _;
    use serde_json::json;

    #[test]
    fn phase4_avatar_files_are_deduplicated_bounded_and_removed_with_the_host() {
        let mut cache = AvatarFiles::default();
        let mut missing = json!({});
        cache.source(&mut missing).unwrap();
        assert!(cache.dir.is_none());
        assert!(
            cache
                .source(&mut json!({"src":"data:image/gif;base64,%%%"}))
                .is_err()
        );
        assert!(
            cache
                .source(&mut json!({"src":"https://example.com/avatar.gif"}))
                .is_err()
        );
        assert!(
            cache
                .source(
                    &mut json!({"src":format!("data:image/gif;base64,{}", "a".repeat(1_398_105))})
                )
                .is_err()
        );
        let mut first = String::new();
        for n in 0..66u8 {
            let bytes = [b'G', b'I', b'F', n];
            let source = format!(
                "data:image/gif;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            let mut reply = json!({"src":source});
            cache.source(&mut reply).unwrap();
            let path = reply["src"].as_str().unwrap().to_owned();
            if n == 0 {
                first = path.clone();
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            let mut again = json!({"src":source});
            cache.source(&mut again).unwrap();
            assert_eq!(again["src"], path);
            assert!(cache.files.len() <= 64);
        }
        assert!(!std::path::Path::new(&first).exists());
        let dir = cache.dir.as_ref().unwrap().path().to_owned();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 64);
        drop(cache);
        assert!(!dir.exists());
    }

    #[test]
    fn phase4_bridge_exposes_actions_without_tokens_or_deferred_capabilities() {
        for method in [
            "KeikiConversationAction",
            "GetKeikiAvatar",
            "CreateKeikiAgent",
            "CancelKeikiTask",
            "EndKeikiGroup",
            "KeikiPresetConnection",
        ] {
            assert!(CALLS.contains(&method));
        }
        assert!(!CALLS.contains(&"AccessToken"));
        assert!(
            CALLS
                .iter()
                .all(|method| !method.contains("Desktop") && !method.contains("Browser"))
        );
    }
}
