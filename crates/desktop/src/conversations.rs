use serde_json::{Value, json};

pub fn register(registration: &crate::bridge::Registration<'_>) {
    registration.project(
        "keiki.conversations",
        registration.account.clone(),
        snapshot,
    );
    registration.account_action(
        "conversations.choose_agent",
        crate::account::Command::ChooseAgent,
    );
    registration.account_action("conversations.send", crate::account::Command::Send);
}

pub fn snapshot(account: &Value) -> Value {
    json!({"agent_id": account["agent_id"], "busy": account["busy"]})
}
