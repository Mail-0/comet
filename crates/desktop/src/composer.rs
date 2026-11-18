pub fn register(registration: &crate::bridge::Registration<'_>) {
    register_host(registration.host);
}

pub fn register_host(host: &zgx_script::ScriptHost) {
    let draft = host.shared("keiki.composer", zgx_script::Value::from(""));
    host.action("composer.clear", move |_| {
        draft.set(zgx_script::Value::from(""));
    });
}
