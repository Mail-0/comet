use super::*;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub(super) const PREFIX: &str = "keiki-pty-";

#[derive(Clone)]
pub(super) struct Terminal {
    owner: Owner,
    binding: Binding,
    remote: String,
    cancel: CancellationToken,
    _slot: Arc<tokio::sync::OwnedSemaphorePermit>,
}

pub(super) type Terminals = std::sync::Mutex<HashMap<String, Terminal>>;

impl Inner {
    pub(super) fn close_chat_terminals(&self, owner: &Owner, chat: &str) -> Result<(), RpcError> {
        for terminal in self
            .terminals
            .lock()
            .map_err(|_| failed("Terminals are unavailable"))?
            .values()
        {
            if &terminal.owner == owner && terminal.binding.chat_id == chat {
                terminal.cancel.cancel();
            }
        }
        Ok(())
    }
}

pub(super) fn handles(method: &str, params: &Value) -> bool {
    (method == "OpenTerminal"
        && params["chatId"]
            .as_str()
            .is_some_and(|id| id.starts_with(BOUND_PREFIX)))
        || (matches!(
            method,
            "SubscribeTerminal"
                | "WriteTerminal"
                | "ResizeTerminal"
                | "CloseTerminal"
                | "TerminalStatus"
        ) && params["terminalId"]
            .as_str()
            .is_some_and(|id| id.starts_with(PREFIX)))
}

fn size(params: &Value) -> Result<keiki_api::TerminalSize, RpcError> {
    let dimension = |key, default| match params.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| (1..=500).contains(value))
            .map(|value| value as u16)
            .ok_or_else(|| RpcError::BadParams("Terminal dimensions must be 1..500".into())),
    };
    Ok(keiki_api::TerminalSize {
        cols: dimension("cols", 80)?,
        rows: dimension("rows", 24)?,
    })
}

impl KeikiConversations {
    pub(super) async fn terminal(
        &self,
        scope: Owner,
        method: &str,
        params: Value,
    ) -> Result<RpcReply, RpcError> {
        let client = keiki_api::Client::new(self.0.auth.api_url());
        let token = self.0.authenticated(&scope).await?;
        if method == "OpenTerminal" {
            let slot = Arc::new(
                self.0
                    .terminal_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| failed("Close a sandbox terminal before opening more"))?,
            );
            let mut auth = self.0.auth.watch_scope();
            let binding = self.0.binding(&scope, text(&params, "chatId")?)?;
            let size = size(&params)?;
            {
                let mut terminals = self
                    .0
                    .terminals
                    .lock()
                    .map_err(|_| failed("Terminals are unavailable"))?;
                terminals.retain(|_, terminal| !terminal.cancel.is_cancelled());
                if terminals.len() >= 32 {
                    return Err(failed("Close a sandbox terminal before opening more"));
                }
            }
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                client.open_terminal(&token, &locator(&binding), size),
            )
            .await
            .map_err(|_| failed("Opening the sandbox terminal timed out"))?;
            if self.0.check(&scope).is_err() {
                if let Ok(remote) = result {
                    let _ = client
                        .close_terminal(&token, &locator(&binding), &remote)
                        .await;
                }
                return Err(failed("Keiki account or organization changed"));
            }
            let remote = self
                .0
                .api_result(&scope, &token, result)?
                .as_str()
                .ok_or_else(|| failed("Invalid terminal response"))?
                .to_owned();
            let id = format!("{PREFIX}{}", uuid::Uuid::new_v4());
            let cancel = CancellationToken::new();
            self.0
                .terminals
                .lock()
                .map_err(|_| failed("Terminals are unavailable"))?
                .insert(
                    id.clone(),
                    Terminal {
                        owner: scope.clone(),
                        binding: binding.clone(),
                        remote: remote.clone(),
                        cancel: cancel.clone(),
                        _slot: slot.clone(),
                    },
                );
            let weak = Arc::downgrade(&self.0);
            let cleanup_id = id.clone();
            tokio::spawn(async move {
                tokio::select! { _ = cancel.cancelled() => {}, _ = auth.changed() => {} }
                cancel.cancel();
                let inner = weak.upgrade();
                if let Some(inner) = &inner {
                    if let Ok(mut terminals) = inner.terminals.lock() {
                        terminals.remove(&cleanup_id);
                    }
                }
                let cleanup = async {
                    let close_token = if let Some(inner) = inner {
                        if inner.check(&scope).is_ok() {
                            inner.authenticated(&scope).await.unwrap_or(token)
                        } else {
                            token
                        }
                    } else {
                        token
                    };
                    let _ = client
                        .close_terminal(&close_token, &locator(&binding), &remote)
                        .await;
                };
                let _ = tokio::time::timeout(Duration::from_secs(10), cleanup).await;
                drop(slot);
            });
            return Ok(RpcReply::Value(
                json!({"id":id,"cwd":"Sandbox","shell":"shell","hostReplies":false}),
            ));
        }
        let id = text(&params, "terminalId")?;
        let terminal = self
            .0
            .terminals
            .lock()
            .map_err(|_| failed("Terminals are unavailable"))?
            .get(id)
            .cloned()
            .ok_or_else(|| failed("Sandbox terminal is closed"))?;
        if terminal.owner != scope {
            return Err(failed(
                "Terminal belongs to another account or organization",
            ));
        }
        self.0.binding(&scope, &terminal.binding.chat_id)?;
        match method {
            "SubscribeTerminal" => {
                let result = client
                    .terminal_stream(&token, &locator(&terminal.binding), &terminal.remote)
                    .await;
                self.0.check(&scope)?;
                let response = match result {
                    Ok(response) => response,
                    Err(error) => {
                        self.0.api_result::<Value>(&scope, &token, Err(error))?;
                        unreachable!()
                    }
                };
                let (tx, rx) = mpsc::channel(64);
                let mut body = response.bytes_stream();
                let mut auth = self.0.auth.watch_scope();
                let this = self.clone();
                let cancel = terminal.cancel;
                tokio::spawn(async move {
                    let mut decoder = zeron_copilot::SseDecoder::new();
                    loop {
                        tokio::select! {
                            biased;
                            _ = tx.closed() => break,
                            _ = auth.changed() => break,
                            _ = cancel.cancelled() => break,
                            chunk = body.next() => {
                                let Some(Ok(chunk)) = chunk else { break };
                                if this.0.check(&scope).is_err() { break; }
                                let Ok(frames) = decoder.push(&chunk) else { break };
                                for frame in frames {
                                    if frame.event.as_deref() == Some("error") { return; }
                                    if !matches!(frame.event.as_deref(), Some("data" | "exit")) { continue; }
                                    if let Ok(event) = serde_json::from_str::<zeron_proto::TerminalEvent>(&frame.data) {
                                        let value = serde_json::to_value(event).unwrap_or(Value::Null);
                                        tokio::select! {
                                            _ = auth.changed() => return,
                                            _ = cancel.cancelled() => return,
                                            sent = tx.send(value) => if sent.is_err() { return; }
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
                Ok(RpcReply::Stream(
                    futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|value| (value, rx))
                    })
                    .boxed(),
                ))
            }
            "WriteTerminal" => {
                let data = text(&params, "data")?;
                if data.len() > 131_072 {
                    return Err(RpcError::BadParams("Terminal input is too large".into()));
                }
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| RpcError::BadParams("Terminal input must be base64".into()))?;
                self.0.api_result(
                    &scope,
                    &token,
                    client
                        .write_terminal(
                            &token,
                            &locator(&terminal.binding),
                            &terminal.remote,
                            data.into(),
                        )
                        .await,
                )?;
                Ok(RpcReply::Value(Value::Null))
            }
            "ResizeTerminal" => {
                self.0.api_result(
                    &scope,
                    &token,
                    client
                        .resize_terminal(
                            &token,
                            &locator(&terminal.binding),
                            &terminal.remote,
                            size(&params)?,
                        )
                        .await,
                )?;
                Ok(RpcReply::Value(Value::Null))
            }
            "CloseTerminal" => {
                terminal.cancel.cancel();
                Ok(RpcReply::Value(Value::Null))
            }
            "TerminalStatus" => Ok(RpcReply::Value(
                json!({"busy":!terminal.cancel.is_cancelled(),"exited":terminal.cancel.is_cancelled()}),
            )),
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }
}
