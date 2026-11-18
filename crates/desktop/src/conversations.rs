use serde_json::{Value, json};

pub fn snapshot(account: &Value) -> Value {
    json!({"agent_id": account["agent_id"], "busy": account["busy"]})
}
