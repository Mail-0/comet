//! Credential-free Keiki browsing; transcript delivery stays in `chat`.
use std::{cell::RefCell, collections::HashMap, rc::Rc};

use serde_json::{Value as Json, json};
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
];

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
    host.async_action("keiki.call", |args| {
        let method = args.field("method").map(text).unwrap_or_default();
        let params = args.field("params").map(value_json).unwrap_or(json!({}));
        async move {
            crate::plugins::preview::refuse("keiki.call")?;
            if !CALLS.contains(&method.as_str()) || !params.is_object() {
                return Err("Unsupported Keiki request".into());
            }
            engine::get()
                .call_soon(&method, params)
                .await
                .map(|v| json_value(&v))
                .map_err(|_| "Could not reach Keiki. Check your connection and try again.".into())
        }
    });
}
