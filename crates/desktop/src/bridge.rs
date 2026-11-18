use serde_json::Value as Json;
use tokio::sync::{mpsc, watch};
use zgx_desktop::TaskSpawner;
use zgx_script::{ScriptHost, Value};

use crate::{account, script_value};

pub struct Registration<'a> {
    pub host: &'a ScriptHost,
    pub tasks: &'a TaskSpawner,
    pub commands: &'a mpsc::Sender<account::Command>,
    pub account: &'a watch::Receiver<Json>,
    pub workspace: &'a watch::Receiver<Json>,
    pub settings: &'a watch::Receiver<Json>,
}

impl Registration<'_> {
    pub fn publish(&self, key: &str, initial: Json) {
        self.host.shared(key, script_value(&initial));
    }

    pub fn watch(&self, key: &str, source: watch::Receiver<Json>) {
        self.project(key, source, Clone::clone);
    }

    pub fn project(
        &self,
        key: &str,
        mut source: watch::Receiver<Json>,
        project: fn(&Json) -> Json,
    ) {
        let value = self
            .host
            .shared(key, script_value(&project(&source.borrow_and_update())));
        self.tasks.spawn(async move {
            while source.changed().await.is_ok() {
                value.set(script_value(&project(&source.borrow_and_update())));
            }
        });
    }

    pub fn account_action(&self, action: &str, command: fn(String) -> account::Command) {
        let commands = self.commands.clone();
        let account = self.account.clone();
        self.host.action(action, move |payload| {
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
}
