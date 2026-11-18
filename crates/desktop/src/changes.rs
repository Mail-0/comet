pub fn register(registration: &crate::bridge::Registration<'_>) {
    registration.publish("keiki.changes", serde_json::json!({"available": false}));
}
