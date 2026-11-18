use serde_json::{Value, json};

pub fn initial() -> Value {
    json!({"route": "chat"})
}
