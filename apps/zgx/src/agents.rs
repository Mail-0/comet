//! The engine's Copilot descriptor, projected into the existing provider cards.

use serde_json::{Value as Json, json};
use std::{cell::RefCell, rc::Rc};
use zgx_script::{ScriptHost, Shared, Value};

use crate::{auth::text, engine, plugins::preview::refuse};

#[derive(Default)]
struct Agents {
    shared: RefCell<Vec<Shared>>,
    list: RefCell<Vec<Json>>,
    target: RefCell<Option<String>>,
    state: RefCell<&'static str>,
}

thread_local! {
    static AGENTS: Rc<Agents> = Rc::new(Agents::default());
}

fn listed(list: Vec<Json>) -> Vec<Json> {
    list.into_iter()
        .filter(|h| h["id"].as_str() == Some("copilot"))
        .collect()
}

impl Agents {
    fn value(&self) -> Value {
        let list: Vec<Value> = self
            .list
            .borrow()
            .iter()
            .map(|h| {
                Value::record([
                    ("id", Value::from("copilot")),
                    ("name", Value::from("Copilot")),
                    ("mark", Value::from("copilot")),
                    ("installed", Value::Bool(true)),
                    ("can_install", Value::Bool(false)),
                    (
                        "enabled",
                        Value::Bool(h["enabled"].as_bool().unwrap_or(true)),
                    ),
                    ("accounts", Value::List(Vec::new().into())),
                    ("login", Value::from("idle")),
                    ("login_methods", Value::List(Vec::new().into())),
                    ("can_sign_out", Value::Bool(false)),
                ])
            })
            .collect();
        let state = *self.state.borrow();
        Value::record([
            (
                "state",
                Value::from(if state.is_empty() { "looking" } else { state }),
            ),
            ("list", Value::List(list.into())),
            (
                "target",
                self.target
                    .borrow()
                    .as_deref()
                    .map_or(Value::Nil, Value::from),
            ),
        ])
    }

    fn addressed(&self, mut params: Json) -> Json {
        if let Some(target) = self.target.borrow().as_deref() {
            params["targetDeviceId"] = json!(target);
        }
        params
    }

    fn publish(&self) {
        for shared in self.shared.borrow().iter() {
            shared.set(self.value());
        }
    }

    async fn load(&self) {
        match engine::get()
            .call("ListHarnesses", self.addressed(json!({})))
            .await
        {
            Ok(Json::Array(list)) => {
                *self.list.borrow_mut() = listed(list);
                *self.state.borrow_mut() = "ready";
            }
            _ => *self.state.borrow_mut() = "unavailable",
        }
        self.publish();
    }
}

pub fn install(host: &ScriptHost) {
    let agents = AGENTS.with(Rc::clone);
    agents
        .shared
        .borrow_mut()
        .push(host.shared("agents", agents.value()));
    host.shared(
        "agents.on_device",
        Value::record(Vec::<(&str, Value)>::new()),
    );
    host.shared("agents.logins", Value::record(Vec::<(&str, Value)>::new()));
    host.shared("agent_icons", Value::record(Vec::<(&str, Value)>::new()));
    if agents.shared.borrow().len() == 1 {
        let weak = Rc::downgrade(&agents);
        engine::get().on_connect(move |_| {
            if let Some(agents) = weak.upgrade() {
                engine::get().spawn(async move { agents.load().await });
            }
        });
    }
    let a = agents.clone();
    host.action("agents.load", move |_| {
        let a = a.clone();
        engine::get().spawn(async move { a.load().await });
    });
    let a = agents.clone();
    host.action("agents.target", move |id| {
        let id = text(&id);
        *a.target.borrow_mut() = (!id.is_empty()).then_some(id);
        let a = a.clone();
        engine::get().spawn(async move { a.load().await });
    });
    host.async_action("agents.set_enabled", move |args| {
        let a = agents.clone();
        async move {
            if args.field("id").map(text).as_deref() != Some("copilot") {
                return Err("That agent is unavailable".into());
            }
            let enabled = matches!(args.field("enabled"), Some(Value::Bool(true)));
            let params = a.addressed(json!({"harness": "copilot", "enabled": enabled}));
            engine::get()
                .call("SetHarnessEnabled", params)
                .await
                .map_err(|e| e.to_string())?;
            a.load().await;
            Ok(Value::Nil)
        }
    });
    for action in [
        "agents.sign_in",
        "agents.sign_out",
        "agents.complete",
        "agents.switch",
        "agents.switch_on",
        "agents.remove",
        "agents.add_key",
        "agents.rename",
        "agents.set_key",
    ] {
        host.async_action(action, move |_| async move {
            match action {
                "agents.sign_in" => refuse("agents.sign_in"),
                "agents.sign_out" => refuse("agents.sign_out"),
                "agents.remove" => refuse("agents.remove"),
                "agents.add_key" => refuse("agents.add_key"),
                "agents.rename" => refuse("agents.rename"),
                "agents.set_key" => refuse("agents.set_key"),
                _ => refuse(action),
            }?;
            Err::<Value, String>(
                "Copilot uses Keiki sign-in. Open Account in Settings.".into(),
            )
        });
    }
    for action in [
        "agents.watch_device",
        "agents.refresh_device",
        "agents.cancel",
        "agents.dismiss",
        "agents.open_page",
    ] {
        host.action(action, |_| {});
    }
}

pub(crate) fn mark(id: &str) -> Option<&'static str> {
    matches!(id, "copilot" | "mock").then_some("copilot")
}

pub(crate) fn glyph(id: &str) -> Option<String> {
    mark(id).map(str::to_owned)
}
pub(crate) fn remember_icons(_list: &[Json]) {}
pub(crate) fn icons_generation() -> u64 {
    0
}
pub(crate) fn registry_icon(_harness: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_projection_filters_legacy_and_test_agents() {
        let rows = listed(vec![
            json!({ "id":"copilot"}),
            json!({ "id":"mock"}),
            json!({ "id":"codex"}),
            json!({}),
        ]);
        assert_eq!(rows, vec![json!({ "id":"copilot"})]);
        assert!(listed(Vec::new()).is_empty());
        assert!(listed(vec![Json::Null]).is_empty());
        assert_eq!(glyph("copilot").as_deref(), Some("copilot"));
        assert_eq!(glyph("unavailable"), None);
    }
}
