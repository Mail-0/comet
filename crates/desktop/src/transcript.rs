use serde_json::{Value, json};

pub fn register(registration: &crate::bridge::Registration<'_>) {
    registration.project("keiki.transcript", registration.account.clone(), snapshot);
}

pub fn snapshot(account: &Value) -> Value {
    json!({"reply": account["reply"]})
}
