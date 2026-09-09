//! Where a tab's PTY lives. Local chats talk to the engine's terminal RPCs
//! (the shell runs on the chat's host device); Keiki conversations talk to
//! the platform's sandbox terminal routes, so the shell runs inside the
//! conversation's sandbox — whichever provider the platform backs it with.
//! The panel drives both through the same five calls and the same
//! [`TerminalEvent`] stream.

use std::pin::Pin;

use futures::{Stream, StreamExt as _};
use gpui::AsyncApp;
use keiki_api::{ConversationLocator, TerminalSize};
use zeron_proto::{TerminalEvent, TerminalSession};
use zeron_rpc::methods;

use crate::state::EngineHandle;

pub type EventStream = Pin<Box<dyn Stream<Item = TerminalEvent> + Send>>;

#[derive(Clone)]
pub enum TerminalTransport {
    Engine {
        engine: EngineHandle,
        chat: String,
        /// The chat's host device when it differs from the connected engine's
        /// own — the PTY lives there, so every RPC carries `targetDeviceId`.
        target: Option<String>,
    },
    Keiki {
        client: keiki_api::Client,
        access_token: String,
        locator: ConversationLocator,
    },
}

impl TerminalTransport {
    pub async fn open(&self, cols: u16, rows: u16, cx: &mut AsyncApp) -> Result<String, String> {
        match self {
            Self::Engine {
                engine,
                chat,
                target,
            } => engine
                .client()
                .call_as::<TerminalSession>(
                    methods::OPEN_TERMINAL,
                    with_target(
                        serde_json::json!({ "chatId": chat, "cols": cols, "rows": rows }),
                        target,
                    ),
                )
                .await
                .map(|session| session.id)
                .map_err(|e| e.to_string()),
            Self::Keiki {
                client,
                access_token,
                locator,
            } => {
                let (client, token, locator) =
                    (client.clone(), access_token.clone(), locator.clone());
                on_tokio(cx, async move {
                    client
                        .open_terminal(&token, &locator, TerminalSize { cols, rows })
                        .await
                })
                .await
            }
        }
    }

    pub async fn subscribe(
        &self,
        terminal_id: &str,
        after_seq: u64,
        cx: &mut AsyncApp,
    ) -> Result<EventStream, String> {
        match self {
            Self::Engine { engine, target, .. } => {
                let rx = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "afterSeq": after_seq }),
                            target,
                        ),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let frames = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|value| (value, rx))
                });
                Ok(Box::pin(frames.filter_map(|value| async move {
                    parse_event(serde_json::from_value(value))
                })))
            }
            Self::Keiki {
                client,
                access_token,
                locator,
            } => {
                let (client, token, locator, id) = (
                    client.clone(),
                    access_token.clone(),
                    locator.clone(),
                    terminal_id.to_string(),
                );
                let response = on_tokio(cx, async move {
                    client.terminal_stream(&token, &locator, &id).await
                })
                .await?;
                let (tx, rx) = futures::channel::mpsc::unbounded();
                cx.update(|cx| gpui_tokio::Tokio::spawn(cx, decode_sse(response, tx)))
                    .detach();
                Ok(Box::pin(rx))
            }
        }
    }

    /// `data` is base64.
    pub async fn write(&self, terminal_id: &str, data: String, cx: &mut AsyncApp) {
        match self {
            Self::Engine { engine, target, .. } => {
                let _ = engine
                    .client()
                    .call(
                        methods::WRITE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "data": data }),
                            target,
                        ),
                    )
                    .await;
            }
            Self::Keiki {
                client,
                access_token,
                locator,
            } => {
                let (client, token, locator, id) = (
                    client.clone(),
                    access_token.clone(),
                    locator.clone(),
                    terminal_id.to_string(),
                );
                let _ = on_tokio(cx, async move {
                    client.write_terminal(&token, &locator, &id, data).await
                })
                .await;
            }
        }
    }

    pub async fn resize(&self, terminal_id: &str, cols: u16, rows: u16, cx: &mut AsyncApp) {
        match self {
            Self::Engine { engine, target, .. } => {
                let _ = engine
                    .client()
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "cols": cols, "rows": rows }),
                            target,
                        ),
                    )
                    .await;
            }
            Self::Keiki {
                client,
                access_token,
                locator,
            } => {
                let (client, token, locator, id) = (
                    client.clone(),
                    access_token.clone(),
                    locator.clone(),
                    terminal_id.to_string(),
                );
                let _ = on_tokio(cx, async move {
                    client
                        .resize_terminal(&token, &locator, &id, TerminalSize { cols, rows })
                        .await
                })
                .await;
            }
        }
    }

    pub async fn close(&self, terminal_id: &str, cx: &mut AsyncApp) {
        match self {
            Self::Engine { engine, target, .. } => {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(serde_json::json!({ "terminalId": terminal_id }), target),
                    )
                    .await;
            }
            Self::Keiki {
                client,
                access_token,
                locator,
            } => {
                let (client, token, locator, id) = (
                    client.clone(),
                    access_token.clone(),
                    locator.clone(),
                    terminal_id.to_string(),
                );
                let _ = on_tokio(cx, async move {
                    client.close_terminal(&token, &locator, &id).await
                })
                .await;
            }
        }
    }
}

/// Merge the `targetDeviceId` passthrough into RPC params (no-op for chats on
/// the connected engine's own device).
pub fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

/// reqwest needs the tokio runtime; gpui's executor is not it.
async fn on_tokio<T, F>(cx: &mut AsyncApp, fut: F) -> Result<T, String>
where
    T: Send + 'static,
    F: Future<Output = Result<T, keiki_api::Error>> + Send + 'static,
{
    cx.update(|cx| gpui_tokio::Tokio::spawn(cx, fut))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn parse_event(parsed: Result<TerminalEvent, serde_json::Error>) -> Option<TerminalEvent> {
    match parsed {
        Ok(event) => Some(event),
        Err(err) => {
            tracing::warn!(error = %err, "terminal: malformed stream frame");
            None
        }
    }
}

/// Forward the platform's `data`/`exit` frames; `ping` keeps the connection
/// warm and `error` ends it (the panel reconnects from `afterSeq`).
async fn decode_sse(
    response: reqwest::Response,
    events: futures::channel::mpsc::UnboundedSender<TerminalEvent>,
) {
    let mut decoder = zeron_copilot::SseDecoder::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let frames = match chunk
            .map_err(|e| e.to_string())
            .and_then(|bytes| decoder.push(&bytes).map_err(|e| e.to_string()))
        {
            Ok(frames) => frames,
            Err(err) => {
                tracing::debug!(error = %err, "terminal: sandbox stream broke");
                return;
            }
        };
        for frame in frames {
            match frame.event.as_deref() {
                Some("data") | Some("exit") => {}
                Some("error") => return,
                _ => continue,
            }
            let Some(event) = parse_event(serde_json::from_str(&frame.data)) else {
                continue;
            };
            if events.unbounded_send(event).is_err() {
                return;
            }
        }
    }
}
