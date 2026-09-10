//! The right pane's Browser surface: the cloud browser a Keiki agent handed
//! off in this conversation, streamed live. The platform mints a short-lived
//! capability for the runner's screencast WebSocket; frames arrive as JPEG,
//! are decoded off the UI thread and painted as the newest frame only — a
//! late frame replaces, never queues. Pointer and keys go back the same way.

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, Styled as _, Window,
    canvas, div, prelude::FluentBuilder as _, px,
};
use keiki_api::{BrowserScreencast, ConversationLocator};
use tokio_tungstenite::tungstenite::Message;

use crate::state::AppState;
use crate::theme::Theme;

const MSG_AUTHENTICATE: &str = "browserbase.screencast.authenticate";
const MSG_READY: &str = "browserbase.screencast.ready";
const MSG_GEOMETRY: &str = "browserbase.screencast.geometry";
const MSG_NAVIGATED: &str = "browserbase.screencast.navigated";
const MSG_MOUSE: &str = "browserbase.screencast.mouse";
const MSG_KEY: &str = "browserbase.screencast.key";

/// Frame size cap asked of the runner (the page keeps its aspect).
const MAX_FRAME_EDGE: u32 = 1280;
/// One wheel "line" in CSS pixels, Chrome's own default.
const WHEEL_LINE_PX: f32 = 40.0;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Connecting,
    Live,
    Ended(String),
}

enum Inbound {
    Frame(Arc<RenderImage>),
    Geometry(Size<f32>),
    Navigated(String),
    Ready,
    Ended(String),
}

#[derive(serde::Deserialize)]
struct Control {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<u16>,
    error: Option<String>,
    #[serde(rename = "deviceWidth")]
    device_width: Option<f32>,
    #[serde(rename = "deviceHeight")]
    device_height: Option<f32>,
    url: Option<String>,
}

pub struct BrowserPanel {
    state: Entity<AppState>,
    locator: ConversationLocator,
    focus_handle: FocusHandle,
    status: Status,
    frame: Option<Arc<RenderImage>>,
    /// The page's CSS viewport, per the runner — the input coordinate space.
    viewport: Size<f32>,
    url: Option<SharedString>,
    /// Where the last frame was painted, for pointer mapping.
    painted: Option<Bounds<Pixels>>,
    outbound: Option<futures::channel::mpsc::UnboundedSender<String>>,
    generation: u64,
}

impl BrowserPanel {
    pub fn new(
        state: Entity<AppState>,
        locator: ConversationLocator,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state,
            locator,
            focus_handle: cx.focus_handle(),
            status: Status::Connecting,
            frame: None,
            viewport: Size {
                width: MAX_FRAME_EDGE as f32,
                height: MAX_FRAME_EDGE as f32 * 9.0 / 16.0,
            },
            url: None,
            painted: None,
            outbound: None,
            generation: 0,
        };
        this.connect(cx);
        this
    }

    /// Mint a capability and hold the socket until it closes; a later
    /// `connect` (reconnect) orphans the previous socket via `generation`.
    fn connect(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = Status::Connecting;
        self.outbound = None;
        let (client, token) = {
            let state = self.state.read(cx);
            match (state.keiki_client.clone(), state.keiki_token.as_ref()) {
                (Some(client), Some(token)) => (client, token.access_token().to_string()),
                _ => {
                    self.status = Status::Ended("Signed out".into());
                    return;
                }
            }
        };
        let locator = self.locator.clone();
        let (out_tx, out_rx) = futures::channel::mpsc::unbounded::<String>();
        let (in_tx, mut in_rx) = futures::channel::mpsc::unbounded::<Inbound>();
        self.outbound = Some(out_tx);
        cx.spawn(async move |this, cx| {
            let stream = cx.update(|cx| {
                gpui_tokio::Tokio::spawn(cx, async move {
                    let capability = client.browser_screencast(&token, &locator).await;
                    match capability {
                        Ok(capability) => run_socket(capability, out_rx, in_tx).await,
                        Err(error) => {
                            let _ = in_tx.unbounded_send(Inbound::Ended(error.to_string()));
                        }
                    }
                })
            });
            stream.detach();
            while let Some(inbound) = in_rx.next().await {
                let live = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        this.apply(inbound, cx);
                        true
                    })
                    .unwrap_or(false);
                if !live {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn apply(&mut self, inbound: Inbound, cx: &mut Context<Self>) {
        match inbound {
            Inbound::Frame(frame) => {
                if let Some(previous) = self.frame.replace(frame) {
                    cx.drop_image(previous, None);
                }
            }
            Inbound::Geometry(viewport) => self.viewport = viewport,
            Inbound::Navigated(url) => self.url = Some(url.into()),
            Inbound::Ready => self.status = Status::Live,
            Inbound::Ended(reason) => {
                self.status = Status::Ended(reason);
                self.outbound = None;
            }
        }
        cx.notify();
    }

    fn send(&self, message: serde_json::Value) {
        if let Some(outbound) = &self.outbound {
            let _ = outbound.unbounded_send(message.to_string());
        }
    }

    /// Window position → CSS pixels of the page, if over the painted frame.
    fn page_point(&self, position: Point<Pixels>) -> Option<Point<f32>> {
        let painted = self.painted?;
        if !painted.contains(&position) {
            return None;
        }
        let x = f32::from(position.x - painted.origin.x) / f32::from(painted.size.width);
        let y = f32::from(position.y - painted.origin.y) / f32::from(painted.size.height);
        Some(Point {
            x: x * self.viewport.width,
            y: y * self.viewport.height,
        })
    }

    fn send_mouse(&self, kind: &str, position: Point<Pixels>, extra: serde_json::Value) {
        let Some(point) = self.page_point(position) else {
            return;
        };
        let mut message = serde_json::json!({
            "type": MSG_MOUSE,
            "kind": kind,
            "x": point.x,
            "y": point.y,
        });
        if let (Some(target), Some(extra)) = (message.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        self.send(message);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.send_mouse(
            "mousePressed",
            event.position,
            serde_json::json!({
                "button": button_name(event.button),
                "clickCount": event.click_count.min(3),
                "modifiers": cdp_modifiers(&event.modifiers),
            }),
        );
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.send_mouse(
            "mouseReleased",
            event.position,
            serde_json::json!({
                "button": button_name(event.button),
                "clickCount": event.click_count.min(3),
                "modifiers": cdp_modifiers(&event.modifiers),
            }),
        );
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        let button = event.pressed_button.map(button_name).unwrap_or("none");
        self.send_mouse(
            "mouseMoved",
            event.position,
            serde_json::json!({ "button": button, "modifiers": cdp_modifiers(&event.modifiers) }),
        );
    }

    fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let (dx, dy) = match event.delta {
            ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
            ScrollDelta::Lines(delta) => (delta.x * WHEEL_LINE_PX, delta.y * WHEEL_LINE_PX),
        };
        // gpui's delta is content motion; CDP's is wheel motion.
        self.send_mouse(
            "mouseWheel",
            event.position,
            serde_json::json!({ "deltaX": -dx, "deltaY": -dy, "modifiers": cdp_modifiers(&event.modifiers) }),
        );
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = cdp_key(
            &event.keystroke.key,
            event.keystroke.key_char.as_deref(),
            &event.keystroke.modifiers,
        ) else {
            return;
        };
        self.send(key.message("keyDown"));
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = cdp_key(
            &event.keystroke.key,
            event.keystroke.key_char.as_deref(),
            &event.keystroke.modifiers,
        ) else {
            return;
        };
        self.send(key.message("keyUp"));
        cx.stop_propagation();
    }

    fn render_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (label, action) = match &self.status {
            Status::Live => return None,
            Status::Connecting => ("Connecting to the browser…", false),
            Status::Ended(reason) => (reason.as_str(), true),
        };
        let mut card = div()
            .id("browser-status")
            .px(px(14.0))
            .py(px(10.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(label.to_string())),
            );
        if action {
            card = card.child(
                div()
                    .id("browser-reconnect")
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text)
                    .child(SharedString::from("Reconnect"))
                    .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }
}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let focused = self.focus_handle.is_focused(window);
        let frame = self.frame.clone();
        let entity = cx.entity();
        let picture = canvas(
            move |bounds, _, _| bounds,
            move |bounds, _, window, cx| {
                let Some(frame) = frame else {
                    return;
                };
                let fitted = fit_bounds(bounds, frame.size(0).map(|d| d.0 as f32));
                entity.update(cx, |this, _| this.painted = Some(fitted));
                let _ = window.paint_image(fitted, gpui::Corners::default(), frame, 0, false);
            },
        )
        .size_full();
        let status = self.render_status(&theme, cx);
        let url = self.url.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .overflow_hidden()
                    .child(url.unwrap_or_else(|| SharedString::from("about:blank"))),
            )
            .child(
                div()
                    .id("browser-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .bg(theme.surface)
                    .key_context("Browser")
                    .track_focus(&self.focus_handle)
                    .when(focused, |el| {
                        el.border_1().border_color(theme.border_strong)
                    })
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_key_up(cx.listener(Self::on_key_up))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(picture)
                    .children(status),
            )
    }
}

/// Largest `aspect`-shaped rectangle centred in `bounds`.
fn fit_bounds(bounds: Bounds<Pixels>, aspect: Size<f32>) -> Bounds<Pixels> {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if aspect.width <= 0.0 || aspect.height <= 0.0 || w <= 0.0 || h <= 0.0 {
        return bounds;
    }
    let scale = (w / aspect.width).min(h / aspect.height);
    let (fw, fh) = (aspect.width * scale, aspect.height * scale);
    Bounds {
        origin: Point {
            x: bounds.origin.x + px((w - fw) / 2.0),
            y: bounds.origin.y + px((h - fh) / 2.0),
        },
        size: Size {
            width: px(fw),
            height: px(fh),
        },
    }
}

/// Dial the runner, authenticate with the capability, then pump frames in
/// and input out until either side closes.
async fn run_socket(
    capability: BrowserScreencast,
    mut outbound: futures::channel::mpsc::UnboundedReceiver<String>,
    inbound: futures::channel::mpsc::UnboundedSender<Inbound>,
) {
    let end = |reason: String| {
        let _ = inbound.unbounded_send(Inbound::Ended(reason));
    };
    let (socket, _) = match tokio_tungstenite::connect_async(&capability.url).await {
        Ok(connected) => connected,
        Err(error) => return end(error.to_string()),
    };
    let (mut sink, mut source) = socket.split();
    let hello = serde_json::json!({
        "type": MSG_AUTHENTICATE,
        "browserbaseSessionId": capability.session_id,
        "token": capability.token,
        "maxWidth": MAX_FRAME_EDGE,
        "maxHeight": MAX_FRAME_EDGE,
    });
    if let Err(error) = sink.send(Message::Text(hello.to_string())).await {
        return end(error.to_string());
    }
    loop {
        tokio::select! {
            message = outbound.next() => match message {
                Some(text) => {
                    if sink.send(Message::Text(text)).await.is_err() {
                        return end("Browser stream closed".into());
                    }
                }
                None => {
                    let _ = sink.close().await;
                    return;
                }
            },
            message = source.next() => match message {
                Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes) {
                    Ok(frame) => {
                        if inbound.unbounded_send(Inbound::Frame(frame)).is_err() {
                            return;
                        }
                    }
                    Err(error) => tracing::debug!(%error, "browser: undecodable frame"),
                },
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_control(&text)
                        && inbound.unbounded_send(event).is_err()
                    {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) | None => return end("Browser session ended".into()),
                Some(Ok(_)) => {}
                Some(Err(error)) => return end(error.to_string()),
            },
        }
    }
}

fn parse_control(text: &str) -> Option<Inbound> {
    let control: Control = serde_json::from_str(text).ok()?;
    if let Some(status) = control.status
        && status >= 400
    {
        return Some(Inbound::Ended(
            control
                .error
                .unwrap_or_else(|| format!("Browser stream refused ({status})")),
        ));
    }
    match control.kind.as_deref() {
        Some(MSG_READY) => Some(Inbound::Ready),
        Some(MSG_GEOMETRY) => Some(Inbound::Geometry(Size {
            width: control.device_width?,
            height: control.device_height?,
        })),
        Some(MSG_NAVIGATED) => Some(Inbound::Navigated(control.url?)),
        _ => None,
    }
}

/// JPEG → gpui's BGRA render image, on the tokio worker (never the UI thread).
fn decode_frame(bytes: &[u8]) -> Result<Arc<RenderImage>, image::ImageError> {
    let mut rgba =
        image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.into_rgba8();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        MouseButton::Navigate(gpui::NavigationDirection::Back) => "back",
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => "forward",
    }
}

/// CDP `Input.dispatchKeyEvent` modifier bits: Alt=1, Ctrl=2, Meta=4, Shift=8.
fn cdp_modifiers(mods: &Modifiers) -> u8 {
    (mods.alt as u8)
        | ((mods.control as u8) << 1)
        | ((mods.platform as u8) << 2)
        | ((mods.shift as u8) << 3)
}

struct CdpKey {
    key: String,
    code: String,
    text: Option<String>,
    vk: u16,
    modifiers: u8,
}

impl CdpKey {
    fn message(&self, kind: &str) -> serde_json::Value {
        // Text only travels with the press: Chrome types on keyDown, and
        // without it Enter/Backspace are dead keys to input fields.
        let text = (kind == "keyDown")
            .then_some(self.text.as_deref())
            .flatten();
        serde_json::json!({
            "type": MSG_KEY,
            "kind": kind,
            "key": self.key,
            "code": self.code,
            "text": text,
            "windowsVirtualKeyCode": self.vk,
            "modifiers": self.modifiers,
        })
    }
}

/// gpui keystroke → CDP key fields. `None` leaves the keystroke to the app
/// (platform-primary shortcuts drive Comet, never the remote page).
fn cdp_key(key: &str, key_char: Option<&str>, mods: &Modifiers) -> Option<CdpKey> {
    if mods.platform {
        return None;
    }
    let modifiers = cdp_modifiers(mods);
    let named = |name: &str, code: &str, vk: u16, text: Option<&str>| CdpKey {
        key: name.to_string(),
        code: code.to_string(),
        text: text.map(str::to_string),
        vk,
        modifiers,
    };
    Some(match key {
        "enter" => named("Enter", "Enter", 13, Some("\r")),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" => named("Escape", "Escape", 27, None),
        "space" => named(" ", "Space", 32, Some(" ")),
        "delete" => named("Delete", "Delete", 46, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        "up" => named("ArrowUp", "ArrowUp", 38, None),
        "down" => named("ArrowDown", "ArrowDown", 40, None),
        "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "right" => named("ArrowRight", "ArrowRight", 39, None),
        _ => {
            let typed = key_char.filter(|_| !mods.control && !mods.alt);
            let mut chars = key.chars();
            let single = match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => return None,
            };
            let upper = single.to_ascii_uppercase();
            let code = if single.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if single.is_ascii_digit() {
                format!("Digit{single}")
            } else {
                String::new()
            };
            CdpKey {
                key: typed.unwrap_or(key).to_string(),
                code,
                text: typed.map(str::to_string),
                vk: upper as u16,
                modifiers,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes_and_centres() {
        let bounds = Bounds {
            origin: Point {
                x: px(10.0),
                y: px(10.0),
            },
            size: Size {
                width: px(400.0),
                height: px(400.0),
            },
        };
        let fitted = fit_bounds(
            bounds,
            Size {
                width: 16.0,
                height: 9.0,
            },
        );
        assert_eq!(fitted.size.width, px(400.0));
        assert_eq!(fitted.size.height, px(225.0));
        assert_eq!(fitted.origin.x, px(10.0));
        assert_eq!(fitted.origin.y, px(10.0 + (400.0 - 225.0) / 2.0));
    }

    #[test]
    fn keys_map_to_cdp_fields() {
        let mods = Modifiers::default();
        let enter = cdp_key("enter", None, &mods).unwrap();
        assert_eq!((enter.vk, enter.text.as_deref()), (13, Some("\r")));
        let a = cdp_key("a", Some("a"), &mods).unwrap();
        assert_eq!(
            (a.code.as_str(), a.vk, a.text.as_deref()),
            ("KeyA", 65, Some("a"))
        );
        let ctrl_a = cdp_key(
            "a",
            Some("a"),
            &Modifiers {
                control: true,
                ..mods
            },
        )
        .unwrap();
        assert_eq!((ctrl_a.text, ctrl_a.modifiers), (None, 2));
        assert!(
            cdp_key(
                "s",
                Some("s"),
                &Modifiers {
                    platform: true,
                    ..mods
                }
            )
            .is_none()
        );
        assert!(cdp_key("capslock", None, &mods).is_none());
    }

    #[test]
    fn control_messages_parse() {
        assert!(matches!(
            parse_control(r#"{"type":"browserbase.screencast.ready","status":200}"#),
            Some(Inbound::Ready)
        ));
        assert!(matches!(
            parse_control(r#"{"type":"browserbase.screencast.geometry","deviceWidth":800,"deviceHeight":600}"#),
            Some(Inbound::Geometry(Size { width, height })) if width == 800.0 && height == 600.0
        ));
        assert!(matches!(
            parse_control(r#"{"type":"browserbase.screencast.ready","status":401,"error":"nope"}"#),
            Some(Inbound::Ended(reason)) if reason == "nope"
        ));
    }
}
