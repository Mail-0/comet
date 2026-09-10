//! The Desktop surface: a live view of the conversation sandbox's graphical
//! desktop (Daytona Computer Use), with the pointer and keyboard passed
//! through.
//!
//! Keiki mints a preview token for the sandbox's noVNC port; Comet then talks
//! RFB to it directly over that websocket (`vnc-rs` decodes, tungstenite
//! carries), so frames never route through the platform. The session runs on
//! the Tokio runtime and paints into a shared framebuffer; the entity turns
//! that into a GPUI image whenever the session says something changed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use gpui::{
    AsyncApp, Bounds, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString,
    Styled, StyledImage as _, Task, Window, canvas, div, img, px,
};
use keiki_api::{Client as KeikiClient, ConversationLocator, DesktopViewer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use vnc::{
    ClientKeyEvent, ClientMouseEvent, PixelFormat, VncConnector, VncEncoding, VncError, VncEvent,
    X11Event,
};

use crate::icons;
use crate::theme::Theme;

/// The desktop is offered wherever the platform's records say the sandbox
/// exists; the provider gets the last word when the viewer actually opens.
pub enum DesktopEvent {
    /// The sandbox is gone — nothing to show, and the offer should go too.
    Gone,
}

enum Status {
    Opening,
    Connected,
    Closed(String),
}

/// BGRA pixels, the wire format asked of the server ([`PixelFormat::bgra`])
/// and what [`RenderImage`] expects, so rectangles blit straight in.
#[derive(Default)]
struct Framebuffer {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    dirty: bool,
}

const BYTES_PER_PIXEL: usize = 4;

impl Framebuffer {
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0; usize::from(width) * usize::from(height) * BYTES_PER_PIXEL];
        self.dirty = true;
    }

    fn row_range(&self, x: u16, y: u16, width: u16) -> Option<std::ops::Range<usize>> {
        if x.checked_add(width)? > self.width || y >= self.height {
            return None;
        }
        let start = (usize::from(y) * usize::from(self.width) + usize::from(x)) * BYTES_PER_PIXEL;
        Some(start..start + usize::from(width) * BYTES_PER_PIXEL)
    }

    fn blit(&mut self, rect: vnc::Rect, data: &[u8]) {
        let stride = usize::from(rect.width) * BYTES_PER_PIXEL;
        for (row, source) in data
            .chunks_exact(stride)
            .take(usize::from(rect.height))
            .enumerate()
        {
            let Some(y) = u16::try_from(row)
                .ok()
                .and_then(|row| rect.y.checked_add(row))
            else {
                break;
            };
            let Some(range) = self.row_range(rect.x, y, rect.width) else {
                break;
            };
            let target = &mut self.pixels[range];
            target.copy_from_slice(source);
            // Servers leave the padding byte at 0; the image is opaque.
            for pixel in target.as_chunks_mut::<BYTES_PER_PIXEL>().0 {
                pixel[3] = 0xff;
            }
        }
        self.dirty = true;
    }

    fn copy(&mut self, destination: vnc::Rect, source: vnc::Rect) {
        let stride = usize::from(destination.width) * BYTES_PER_PIXEL;
        let mut moved = vec![0; stride * usize::from(destination.height)];
        for row in 0..destination.height {
            let Some(range) =
                self.row_range(source.x, source.y.saturating_add(row), destination.width)
            else {
                break;
            };
            let at = usize::from(row) * stride;
            moved[at..at + stride].copy_from_slice(&self.pixels[range]);
        }
        self.blit(destination, &moved);
    }

    fn apply(&mut self, event: VncEvent) -> Result<(), String> {
        match event {
            VncEvent::SetResolution(screen) => self.resize(screen.width, screen.height),
            VncEvent::RawImage(rect, data) => self.blit(rect, &data),
            VncEvent::Copy(destination, source) => self.copy(destination, source),
            VncEvent::Error(message) => return Err(message),
            // Tight/cursor pseudo-encodings are not negotiated; bell,
            // clipboard and pixel-format echoes need no paint.
            _ => {}
        }
        Ok(())
    }

    fn image(&self) -> Option<RenderImage> {
        let buffer = image::RgbaImage::from_raw(
            u32::from(self.width),
            u32::from(self.height),
            self.pixels.clone(),
        )?;
        Some(RenderImage::new(vec![image::Frame::new(buffer)]))
    }
}

/// What the session tells the entity, coalesced: one `Frame` per burst of
/// paints, not one per rectangle.
enum Signal {
    Connected,
    Frame,
}

pub struct Desktop {
    focus_handle: FocusHandle,
    status: Status,
    framebuffer: Arc<Mutex<Framebuffer>>,
    image: Option<Arc<RenderImage>>,
    /// Superseded frames, evicted from the sprite atlas on the next paint
    /// (dropping the `Arc` alone leaks the tile).
    retired: Vec<Arc<RenderImage>>,
    /// Where the last paint put the desktop, for pointer mapping.
    viewport: Arc<Mutex<Bounds<Pixels>>>,
    input: Option<mpsc::UnboundedSender<X11Event>>,
    /// RFB button mask as currently held.
    buttons: u8,
    _session: Task<()>,
}

impl EventEmitter<DesktopEvent> for Desktop {}

impl Focusable for Desktop {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Desktop {
    pub fn new(
        client: KeikiClient,
        access_token: String,
        locator: ConversationLocator,
        cx: &mut Context<Self>,
    ) -> Self {
        let framebuffer = Arc::new(Mutex::new(Framebuffer::default()));
        let session = cx.spawn({
            let framebuffer = framebuffer.clone();
            async move |this, cx| {
                let opened = on_tokio(cx, async move {
                    client.open_desktop(&access_token, &locator).await
                })
                .await;
                let viewer = match opened {
                    Ok(Some(viewer)) => viewer,
                    Ok(None) => {
                        this.update(cx, |this, cx| {
                            this.status =
                                Status::Closed("This conversation's sandbox is gone.".into());
                            cx.emit(DesktopEvent::Gone);
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    Err(message) => {
                        this.update(cx, |this, cx| this.close(message, cx)).ok();
                        return;
                    }
                };
                let (input_tx, input_rx) = mpsc::unbounded_channel();
                let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
                if this
                    .update(cx, |this, _| this.input = Some(input_tx))
                    .is_err()
                {
                    return;
                }
                let session = cx.update(|cx| {
                    gpui_tokio::Tokio::spawn(
                        cx,
                        run_session(viewer, framebuffer, signal_tx, input_rx),
                    )
                });
                while let Some(signal) = signal_rx.recv().await {
                    if this
                        .update(cx, |this, cx| this.on_signal(signal, cx))
                        .is_err()
                    {
                        return;
                    }
                }
                let outcome = match session.await {
                    Ok(Ok(())) => "The desktop closed the connection.".to_string(),
                    Ok(Err(message)) => message,
                    Err(error) => error.to_string(),
                };
                this.update(cx, |this, cx| this.close(outcome, cx)).ok();
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            status: Status::Opening,
            framebuffer,
            image: None,
            retired: Vec::new(),
            viewport: Arc::new(Mutex::new(Bounds::default())),
            input: None,
            buttons: 0,
            _session: session,
        }
    }

    fn close(&mut self, message: String, cx: &mut Context<Self>) {
        tracing::warn!(message, "desktop session ended");
        self.status = Status::Closed(message);
        self.input = None;
        cx.notify();
    }

    fn on_signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        match signal {
            Signal::Connected => self.status = Status::Connected,
            Signal::Frame => {
                let image = {
                    let Ok(mut framebuffer) = self.framebuffer.lock() else {
                        return;
                    };
                    if !framebuffer.dirty {
                        return;
                    }
                    framebuffer.dirty = false;
                    framebuffer.image()
                };
                if let Some(image) = image {
                    self.retired.extend(self.image.replace(Arc::new(image)));
                }
            }
        }
        cx.notify();
    }

    fn send(&self, event: X11Event) {
        if let Some(input) = &self.input
            && let Err(error) = input.send(event)
        {
            tracing::debug!(%error, "desktop input after session end");
        }
    }

    /// Window point → framebuffer point, through the letterbox the image is
    /// painted in (`ObjectFit::Contain`, replicated here).
    fn to_desktop(&self, position: gpui::Point<Pixels>) -> Option<(u16, u16)> {
        let (width, height) = {
            let framebuffer = self.framebuffer.lock().ok()?;
            (f32::from(framebuffer.width), f32::from(framebuffer.height))
        };
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let viewport = *self.viewport.lock().ok()?;
        let scale =
            (f32::from(viewport.size.width) / width).min(f32::from(viewport.size.height) / height);
        let origin_x =
            f32::from(viewport.origin.x) + (f32::from(viewport.size.width) - width * scale) / 2.0;
        let origin_y =
            f32::from(viewport.origin.y) + (f32::from(viewport.size.height) - height * scale) / 2.0;
        let x = ((f32::from(position.x) - origin_x) / scale).clamp(0.0, width - 1.0);
        let y = ((f32::from(position.y) - origin_y) / scale).clamp(0.0, height - 1.0);
        Some((x as u16, y as u16))
    }

    fn pointer(&self, position: gpui::Point<Pixels>, buttons: u8) {
        if let Some((position_x, position_y)) = self.to_desktop(position) {
            self.send(X11Event::PointerEvent(ClientMouseEvent {
                position_x,
                position_y,
                bottons: buttons,
            }));
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.buttons |= button_mask(event.button);
        self.pointer(event.position, self.buttons);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.buttons &= !button_mask(event.button);
        self.pointer(event.position, self.buttons);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        self.pointer(event.position, self.buttons);
    }

    /// RFB has no wheel axis: a tick is a press-and-release of buttons 4–7.
    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let (dx, dy) = match event.delta {
            ScrollDelta::Lines(lines) => (lines.x, lines.y),
            ScrollDelta::Pixels(pixels) => (f32::from(pixels.x) / 20.0, f32::from(pixels.y) / 20.0),
        };
        let mut ticks = Vec::new();
        if dy > 0.0 {
            ticks.push(1 << 3);
        } else if dy < 0.0 {
            ticks.push(1 << 4);
        }
        if dx > 0.0 {
            ticks.push(1 << 5);
        } else if dx < 0.0 {
            ticks.push(1 << 6);
        }
        for wheel in ticks {
            self.pointer(event.position, self.buttons | wheel);
            self.pointer(event.position, self.buttons);
        }
    }

    /// One keystroke as the X server wants it: held modifiers down, the key
    /// pressed and released, modifiers up. GPUI repeats key-down while a key
    /// is held, so a full press per event keeps auto-repeat working too.
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let Some(keysym) = keysym(&keystroke.key, keystroke.key_char.as_deref()) else {
            return;
        };
        let modifiers = &keystroke.modifiers;
        let held: Vec<u32> = [
            (modifiers.control, XK_CONTROL_L),
            (modifiers.alt, XK_ALT_L),
            (modifiers.shift, XK_SHIFT_L),
            (modifiers.platform, XK_SUPER_L),
        ]
        .into_iter()
        .filter_map(|(down, keysym)| down.then_some(keysym))
        .collect();
        for modifier in &held {
            self.key(*modifier, true);
        }
        self.key(keysym, true);
        self.key(keysym, false);
        for modifier in held.iter().rev() {
            self.key(*modifier, false);
        }
        cx.stop_propagation();
    }

    fn key(&self, keycode: u32, down: bool) {
        self.send(X11Event::KeyEvent(ClientKeyEvent { keycode, down }));
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired.drain(..) {
            if let Err(error) = window.drop_image(image) {
                tracing::debug!(%error, "desktop frame eviction");
            }
        }
        let theme = Theme::of(cx).clone();
        let notice = match &self.status {
            Status::Opening => Some(SharedString::from("Opening the desktop…")),
            Status::Connected if self.image.is_none() => {
                Some(SharedString::from("Waiting for the first frame…"))
            }
            Status::Connected => None,
            Status::Closed(message) => Some(SharedString::from(message.clone())),
        };
        let viewport = self.viewport.clone();
        let measure = canvas(
            move |bounds, _, _| {
                if let Ok(mut viewport) = viewport.lock() {
                    *viewport = bounds;
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .id("desktop-surface")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .bg(gpui::black())
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_any_mouse_down(cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(measure)
            .children(
                self.image
                    .clone()
                    .map(|image| img(image).size_full().object_fit(gpui::ObjectFit::Contain)),
            )
            .children(notice.map(|notice| {
                div()
                    .absolute()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(
                        icons::icon(icons::LAPTOP)
                            .size(px(20.0))
                            .text_color(theme.text_muted),
                    )
                    .child(notice)
            }))
    }
}

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

/// The websocket is a byte pipe to x11vnc; each side of the RFB session sees
/// a plain duplex stream.
async fn connect(viewer: &DesktopViewer) -> Result<DuplexStream, String> {
    let mut url = url::Url::parse(&viewer.url).map_err(|e| e.to_string())?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        _ => "ws",
    };
    url.set_scheme(scheme)
        .map_err(|()| "unsupported preview URL scheme".to_string())?;
    url.set_path("/websockify");
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "x-daytona-preview-token",
        HeaderValue::from_str(&viewer.token).map_err(|e| e.to_string())?,
    );
    request
        .headers_mut()
        .insert("sec-websocket-protocol", HeaderValue::from_static("binary"));
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sink, mut source) = socket.split();
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (mut reader, mut writer) = tokio::io::split(ours);
    tokio::spawn(async move {
        let inbound = async {
            while let Some(message) = source.next().await {
                match message? {
                    Message::Binary(bytes) => writer.write_all(&bytes).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        let outbound = async {
            let mut buffer = vec![0; 1 << 14];
            loop {
                let read = reader.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                sink.send(Message::Binary(buffer[..read].to_vec())).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let ended = tokio::select! {
            result = inbound => result,
            result = outbound => result,
        };
        if let Err(error) = ended {
            tracing::debug!(%error, "desktop websocket ended");
        }
        sink.send(Message::Close(None)).await.ok();
    });
    Ok(theirs)
}

const REFRESH_INTERVAL: Duration = Duration::from_millis(16);
const IDLE_POLL: Duration = Duration::from_millis(4);

async fn run_session(
    viewer: DesktopViewer,
    framebuffer: Arc<Mutex<Framebuffer>>,
    signal: mpsc::UnboundedSender<Signal>,
    mut input: mpsc::UnboundedReceiver<X11Event>,
) -> Result<(), String> {
    let stream = connect(&viewer).await?;
    // x11vnc under Computer Use runs without a password: the preview token
    // already gated the door, so no VNC auth callback is set.
    let vnc = VncConnector::<_, std::future::Ready<Result<String, VncError>>>::new(stream)
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::bgra())
        .build()
        .map_err(|e| e.to_string())?
        .try_start()
        .await
        .map_err(|e| e.to_string())?
        .finish()
        .map_err(|e| e.to_string())?;
    signal
        .send(Signal::Connected)
        .map_err(|_| "viewer closed".to_string())?;
    vnc.input(X11Event::FullRefresh)
        .await
        .map_err(|e| e.to_string())?;
    let mut last_refresh = Instant::now();
    loop {
        let mut painted = false;
        while let Some(event) = vnc.poll_event().await.map_err(|e| e.to_string())? {
            let mut framebuffer = framebuffer
                .lock()
                .map_err(|_| "framebuffer poisoned".to_string())?;
            let was_dirty = framebuffer.dirty;
            framebuffer.apply(event)?;
            painted |= framebuffer.dirty && !was_dirty;
        }
        if painted {
            signal
                .send(Signal::Frame)
                .map_err(|_| "viewer closed".to_string())?;
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            vnc.input(X11Event::Refresh)
                .await
                .map_err(|e| e.to_string())?;
            last_refresh = Instant::now();
        }
        tokio::select! {
            event = input.recv() => match event {
                Some(event) => vnc.input(event).await.map_err(|e| e.to_string())?,
                None => {
                    vnc.close().await.map_err(|e| e.to_string())?;
                    return Ok(());
                }
            },
            _ = tokio::time::sleep(IDLE_POLL) => {}
        }
    }
}

fn button_mask(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1 << 0,
        MouseButton::Middle => 1 << 1,
        MouseButton::Right => 1 << 2,
        _ => 0,
    }
}

const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

/// X11 keysym for a GPUI keystroke: the typed character when there is one
/// (Latin-1 is its own keysym, the rest go through the Unicode range), else
/// the named key.
fn keysym(key: &str, key_char: Option<&str>) -> Option<u32> {
    let mut typed = key_char.unwrap_or(key).chars();
    if let (Some(character), None) = (typed.next(), typed.next()) {
        let code = u32::from(character);
        return Some(if code < 0x100 {
            code
        } else {
            0x0100_0000 + code
        });
    }
    let named = match key {
        "enter" => 0xff0d,
        "tab" => 0xff09,
        "backspace" => 0xff08,
        "escape" => 0xff1b,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "space" => 0x20,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        _ => {
            let number: u32 = key.strip_prefix('f')?.parse().ok()?;
            (1..=12).contains(&number).then(|| 0xffbd + number)?
        }
    };
    Some(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms_follow_the_typed_character_then_the_key_name() {
        assert_eq!(keysym("a", Some("a")), Some(0x61));
        assert_eq!(keysym("a", Some("A")), Some(0x41));
        assert_eq!(keysym("a", None), Some(0x61));
        assert_eq!(keysym("s", Some("ß")), Some(0xdf));
        assert_eq!(keysym("e", Some("€")), Some(0x0100_20ac));
        assert_eq!(keysym("enter", None), Some(0xff0d));
        assert_eq!(keysym("f5", None), Some(0xffc2));
        assert_eq!(keysym("f13", None), None);
    }

    #[test]
    fn blits_stay_inside_the_framebuffer_and_are_opaque() {
        let mut framebuffer = Framebuffer::default();
        framebuffer.resize(2, 2);
        let rect = vnc::Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        framebuffer.blit(rect, &[7; 16]);
        assert_eq!(framebuffer.pixels[12..16], [0, 0, 0, 0]);
        let rect = vnc::Rect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        };
        framebuffer.blit(rect, &[1, 2, 3, 0]);
        assert_eq!(framebuffer.pixels[4..8], [1, 2, 3, 0xff]);
        framebuffer.copy(
            vnc::Rect {
                x: 0,
                y: 1,
                width: 1,
                height: 1,
            },
            vnc::Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            },
        );
        assert_eq!(framebuffer.pixels[8..12], [1, 2, 3, 0xff]);
    }
}
