use serde_json::{Value, json};

pub fn register(registration: &crate::bridge::Registration<'_>) {
    registration.publish("keiki.sandbox", initial());
}

pub fn initial() -> Value {
    json!({"available": false})
}
