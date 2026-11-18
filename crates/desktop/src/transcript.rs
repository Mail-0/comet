use serde_json::{Value, json};

pub fn snapshot(account: &Value) -> Value {
    json!({"reply": account["reply"]})
}
