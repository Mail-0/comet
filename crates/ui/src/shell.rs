//! The app shell (zeron `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is zeron's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360px floor, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::time::Duration;

use chrono::Utc;
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Empty, Entity, Focusable as _, IntoElement,
    KeyBinding, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, SharedString, Subscription, Task, Window, WindowControlArea, actions,
    div, prelude::*, px,
};

use keiki_model::{AgentTemplateSummary, CreateAgentFromTemplate};
use zeron_rpc::methods;

use crate::avatars::{self, AvatarKey, AvatarSnapshot};
use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, SPLASH_OUT, TAB_SLIDE};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    self, CHAT_PANEL_MIN, JUMP_SLOTS, KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MIN,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, SavePolicy, ShortcutId, SidebarOrganization,
    SidebarSort, TERMINAL_DEFAULT_HEIGHT, UiSettings, badge_combo, jump_hints_visible,
    platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator, KeikiSessionInfo,
    format_time_ago,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript, TranscriptEvent};

mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        NewKeikiAgent,
        NewSession,
        OpenSettings,
        NextSession,
        PrevSession,
        ArchiveSession
    ]
);

#[derive(Clone, Copy)]
enum ChatMenuPage {
    Root,
    Copy,
}

#[derive(Clone)]
struct ChatMenuState {
    chat_id: String,
    position: Point<Pixels>,
    page: ChatMenuPage,
}

struct KeikiAgentDialog {
    templates: Loadable<Vec<AgentTemplateSummary>>,
    selected_template: Option<usize>,
    name: Entity<ComposerInput>,
    line_number: Entity<ComposerInput>,
    error: Option<SharedString>,
    focus_pending: bool,
    template_task: Option<Task<()>>,
    create_task: Option<Task<()>>,
    create_pending: bool,
}

fn keiki_menu_is_selected(chat_id: &str, selected_chat: Option<&str>) -> bool {
    crate::keiki::is_keiki_chat(chat_id) && selected_chat == Some(chat_id)
}

fn prune_pinned_keiki_conversations(pinned: &mut Vec<String>, chats: &[zeron_proto::Chat]) {
    let current_keiki_chat_ids: std::collections::HashSet<&str> = chats
        .iter()
        .filter(|chat| crate::keiki::is_keiki_chat(&chat.id))
        .map(|chat| chat.id.as_str())
        .collect();
    if !current_keiki_chat_ids.is_empty() {
        pinned.retain(|id| current_keiki_chat_ids.contains(id.as_str()));
    }
}

/// Interruptible height tween for the sidebar's device/archive disclosures.
/// The rendered element owns the frame clock; this state preserves the current
/// interpolated height when a second click reverses an in-flight transition.
#[derive(Clone, Copy)]
pub(super) struct SidebarDisclosureMotion {
    pub(super) epoch: u64,
    pub(super) from: f32,
    pub(super) to: f32,
    started: std::time::Instant,
}

impl SidebarDisclosureMotion {
    fn new(epoch: u64, from: f32, to: f32) -> Self {
        Self {
            epoch,
            from,
            to,
            started: std::time::Instant::now(),
        }
    }

    fn current(self) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32();
        let raw = if total > 0.0 {
            self.started.elapsed().as_secs_f32() / total
        } else {
            1.0
        };
        motion::lerp(self.from, self.to, motion::COLLAPSE.progress(raw))
    }

    fn animating(self) -> bool {
        self.started.elapsed() < motion::COLLAPSE.total() + spaces::SIDEBAR_DISCLOSURE_TWEEN_GRACE
    }
}

/// Vertical pane resize hitboxes yield the global titlebar. Keeping this in
/// the shared constructor makes left/right seams mirror each other and avoids
/// relying on paint order when chrome crosses an animated pane boundary.
const PANE_RESIZE_HITBOX_TOP: f32 = Theme::TITLEBAR_HEIGHT;

fn stable_panel_content_width(target: f32, transition: Option<(f32, f32)>) -> f32 {
    transition.map(|(from, to)| from.max(to)).unwrap_or(target)
}

fn right_panel_content_width(
    target: f32,
    transition: Option<(f32, f32)>,
    takeover_width: Option<f32>,
) -> f32 {
    takeover_width.unwrap_or_else(|| stable_panel_content_width(target, transition))
}

fn conversation_width(viewport: f32, sidebar: f32, right: f32) -> f32 {
    (viewport - sidebar - right).max(0.0)
}

fn titlebar_new_session_alpha(is_chat_route: bool) -> f32 {
    if is_chat_route { 1.0 } else { 0.0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyStateVariant {
    SignIn,
    Loading,
    Ready,
}

fn empty_state_variant(status: crate::keiki::SessionStatus) -> EmptyStateVariant {
    match status {
        crate::keiki::SessionStatus::Loading => EmptyStateVariant::Loading,
        crate::keiki::SessionStatus::SignedIn => EmptyStateVariant::Ready,
        crate::keiki::SessionStatus::SignedOut | crate::keiki::SessionStatus::Error => {
            EmptyStateVariant::SignIn
        }
    }
}

fn empty_state_action_label(variant: EmptyStateVariant) -> Option<&'static str> {
    match variant {
        EmptyStateVariant::SignIn => Some("Sign in to Keiki"),
        EmptyStateVariant::Loading => Some("Opening Keiki…"),
        EmptyStateVariant::Ready => None,
    }
}

/// Open the session at `slot` (zero-based) of the sidebar's active list. One
/// action carrying the slot, rather than nine near-identical action types.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct JumpSession(pub usize);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (zeron window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Within-group rhythm for Back/Forward.
pub const TITLEBAR_CONTROL_GAP: f32 = 2.0;
/// Structural separation between titlebar groups: sidebar, navigation,
/// transcript identity, and trailing actions.
pub const TITLEBAR_GROUP_GAP: f32 = Theme::SPACE_SM;
/// Breathing room between the navigation cluster and transcript identity.
pub const TITLEBAR_IDENTITY_GAP: f32 = Theme::SPACE_MD;
/// A 28px action centered in the 38px titlebar with its 2px downward optical
/// shift lands 6px from the top; use the same inset at the trailing edge.
pub const TITLEBAR_ACTION_EDGE_INSET: f32 = 6.0;
/// Width of the persistent top-left button cluster itself: a 24px sidebar
/// trigger, an 8px group gap, then two 24px history buttons on a 2px rhythm.
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + TITLEBAR_GROUP_GAP + TITLEBAR_CONTROL_GAP;
/// Extra width consumed when the collapsed-sidebar New Session action joins
/// the left controls as its own group.
pub const TITLEBAR_ACTION_SLOT_WIDTH: f32 = TITLEBAR_GROUP_GAP + 24.0;
/// Horizontal inset owned by the titlebar control row itself. Keep this value
/// paired with [`Self::titlebar_spacer`]: using a different number for the
/// spacer shifts every control while leaving the declared cluster geometry
/// unchanged.
const TITLEBAR_CLUSTER_PAD: f32 = 10.0;

/// Width of a row of `count` Linux caption buttons, drawn at the cluster's
/// own 24px-button / 2px-gap rhythm.
pub fn caption_buttons_width(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    count as f32 * 24.0 + (count as f32 - 1.0) * 2.0
}

/// Where the cluster's first button starts, from the window's left edge.
/// `linux_left_captions` is the number of caption buttons zeron draws at the
/// top-left on Linux (GNOME `close:…` layouts) — the app cluster follows them
/// at the shared 2px rhythm.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool, linux_left_captions: usize) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else if linux_left_captions > 0 {
        10.0 + caption_buttons_width(linux_left_captions) + 2.0
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(
    is_macos: bool,
    fullscreen: bool,
    linux_left_captions: usize,
    container_pad: f32,
) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen, linux_left_captions) + CLUSTER_BUTTONS_WIDTH + 8.0
        - container_pad)
        .max(0.0)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (Settings on every platform; ⌘Q quit, ⌘W
    // close, ⌘M minimize, ⌘H hide on macOS) — these back the native menu
    // key equivalents and must survive keymap re-application.
    crate::app_menus::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.new_session, "mod-n"),
            NewSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.next_session,
                crate::settings::ShortcutId::NextSession.default_combo(),
            ),
            NextSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.prev_session,
                crate::settings::ShortcutId::PrevSession.default_combo(),
            ),
            PrevSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.archive_session, "mod-shift-a"),
            ArchiveSession,
            None,
        ),
        // Fixed: ⌘K summons the add-space palette (the ⌘K chip in its search
        // bar); pressing it again dismisses.
        KeyBinding::new(&platform_combo("mod-k"), AddSpacePalette, None),
    ]);
    // ⌘1..⌘9 open the sidebar's first nine rows. A slot left unbound (an empty
    // combo in a hand-edited file) binds nothing rather than falling back —
    // the user cleared it on purpose.
    cx.bind_keys((0..JUMP_SLOTS).filter_map(|slot| {
        let id = ShortcutId::JumpSession(slot);
        let combo = keymap.get(id);
        if combo.is_empty() {
            return None;
        }
        Some(KeyBinding::new(
            &valid_or_default(combo, id.default_combo()),
            JumpSession(slot),
            None,
        ))
    }));
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Notifications,
    Shortcuts,
    Archived,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::Appearance,
        SettingsSection::Notifications,
        SettingsSection::Shortcuts,
        SettingsSection::Archived,
    ];

    /// Sidebar + header label (zeron settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Archived sessions",
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

/// Maximum width the right pane may occupy while retaining the conversation
/// floor. On unusually small windows this deliberately falls below the right
/// pane's preferred minimum: the chat remains usable and the side surface
/// yields the scarce space.
fn right_pane_max_width(viewport: f32, sidebar: f32) -> f32 {
    (viewport - sidebar - CHAT_PANEL_MIN).max(0.0)
}

/// Width used by right-pane takeover. Unlike manual resizing, takeover is
/// intentionally allowed to consume the conversation column completely.
fn right_pane_takeover_width(viewport: f32, sidebar: f32) -> f32 {
    (viewport - sidebar).max(0.0)
}

/// One right-pane surface tab (t3code RightPanelSurface, narrowed to our two
/// kinds): a git-diff page (each tab its own [`Changes`] viewer — multiple
/// diff panels, user request) or one embedded terminal keyed by its
/// [`TerminalPanel`] tab key. `Picker` is the empty surface chooser.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RightSurface {
    #[default]
    Picker,
    Diff(u64),
    Terminal(u64),
    /// A subagent's transcript, read-only (per-subagent viz) — the handle
    /// keys [`Shell::subagent_tabs`].
    Subagent(u64),
}

/// Per-chat panel open flags (zeron parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global).
///
/// Everything defaults CLOSED — the right pane included (user request,
/// revising the earlier default-open: it popped open on every session you
/// visited). Opening is an explicit act, remembered per chat for the rest of
/// the app run; a fresh open with no surface tabs lands on the picker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    /// Right pane visible (the surface host — historically the Changes pane).
    pub changes_open: bool,
    /// Which surface tab renders; validated against the live tab list each
    /// frame (a closed tab falls back gracefully).
    pub right_active: RightSurface,
}

/// The session-scoped panel map. Keys are chat ids; no selection uses the
/// empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }

    /// Mutate `key`'s flags in place (right-pane surface bookkeeping).
    pub fn update(&mut self, key: &str, f: impl FnOnce(&mut ChatPanels)) {
        f(self.map.entry(key.to_string()).or_default());
    }
}

/// One route-history entry (zeron parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat, when any.
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (zeron window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (zeron's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (zeron window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Height changes do not constitute a list reorder. In particular, sidebar
/// disclosures animate their own height and must not also trigger FLIP offsets
/// on every following keyed section.
fn sidebar_key_order_changed(old: &[(String, f32)], new: &[(String, f32)]) -> bool {
    old.len() != new.len()
        || old
            .iter()
            .zip(new)
            .any(|((old_key, _), (new_key, _))| old_key != new_key)
}

/// Exact active-session row height. Harness identity lives on the title line
/// and the Working glyph lives in the status corner, so neither adds a third
/// line. Compact rows omit the metadata line and its preceding gap entirely;
/// branch / pull-request rows add the exact height of their tallest child.
/// Keeping this calculation beside the renderer's metrics prevents disclosure
/// clips when view options alter the row structure.
pub(super) fn chat_row_height(shows_branch: bool, shows_pull_request: bool) -> f32 {
    let mut metadata_height: f32 = 0.0;
    if shows_branch {
        metadata_height = metadata_height.max(14.0);
    }
    if shows_pull_request {
        metadata_height = metadata_height.max(16.0);
    }
    if metadata_height == 0.0 {
        45.0
    } else {
        47.0 + metadata_height
    }
}
/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;
/// Harness/title geometry follows the row hierarchy: active multi-line cards
/// keep identity close on the standard 8px rhythm, while the one-line archived
/// shelf gives its larger mark a little more separation.
const SIDEBAR_ACTIVE_HARNESS_ICON_SIZE: f32 = 13.0;
const SIDEBAR_KEIKI_AVATAR_SIZE: f32 = 16.0;
const SIDEBAR_ACTIVE_HARNESS_TITLE_GAP: f32 = Theme::SPACE_SM;
const SIDEBAR_ARCHIVED_HARNESS_ICON_SIZE: f32 = 14.0;
const SIDEBAR_ARCHIVED_HARNESS_TITLE_GAP: f32 = 10.0;

/// Ramp height of the sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 24.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;

/// The dragged surface-tab payload (strip reorder).
struct RightTabDrag {
    panel_key: String,
    from: usize,
    title: SharedString,
}

/// Live drag-over state for the surface-tab strip — the terminal drawer's
/// [`crate::terminal::panel`] DragState, ported: `epoch` keys the 150ms
/// slide-animation restarts as the hovered slot changes.
struct RightTabDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// Ghost chip following the pointer while a surface tab drags.
struct SurfaceTabGhost {
    title: SharedString,
}

impl Render for SurfaceTabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .w(px(112.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

fn account_identity(
    status: crate::keiki::SessionStatus,
    session: Option<&KeikiSessionInfo>,
) -> (SharedString, Option<SharedString>, SharedString) {
    match status {
        crate::keiki::SessionStatus::SignedIn => {
            let name = session.and_then(|session| session.display_name.as_deref());
            let email = session.and_then(|session| session.email.as_deref());
            let line = name.or(email).unwrap_or("Keiki account").into();
            let subline = session
                .and_then(|session| session.active_org_name.as_deref().or(email))
                .map(Into::into);
            let identity = email.unwrap_or("Keiki account").into();
            (line, subline, identity)
        }
        crate::keiki::SessionStatus::Loading => {
            ("Signing in…".into(), None, "Keiki account".into())
        }
        _ => (
            "Not signed in".into(),
            Some("Keiki".into()),
            "Keiki account".into(),
        ),
    }
}

/// One right-pane subagent tab: the doc it shows, its strip title, and the
/// read-only transcript entity whose drop tears the view down.
struct SubagentTab {
    doc_id: String,
    title: SharedString,
    transcript: Entity<Transcript>,
    /// Spawn chips INSIDE the subagent transcript open their own tabs.
    _events: Subscription,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Measured height of the bottom chrome stack (status strip + composer +
    /// terminal dock) the full-height transcript scrolls under — written by a
    /// paint-time canvas each frame, read the NEXT frame for the fade inset,
    /// the transcript's bottom clearance, and the jump pill's anchor (the
    /// same one-frame lag every fade here rides).
    bottom_stack: std::rc::Rc<std::cell::Cell<f32>>,
    /// The sidebar's archived accordion (t3code Sidebar): OPEN by default
    /// (user request), session-transient. `archived_shown` pages the
    /// expanded list ("Show more" reveals another page).
    pub(super) archived_open: bool,
    pub(super) archived_shown: usize,
    /// Archived slim row under the pointer — swaps its time label for the
    /// Unarchive affordance and restores the dimmed harness mark (t3code's
    /// settled-row hover).
    pub(super) archived_hover: Option<String>,
    /// Ephemeral collapsed project/device sections, keyed by organization + id.
    pub(super) sidebar_collapsed_groups: std::collections::HashSet<String>,
    /// In-flight disclosure tweens, shared by device groups and Archived.
    pub(super) sidebar_disclosure_motion:
        std::collections::HashMap<String, SidebarDisclosureMotion>,
    /// The jump-hint overlay: true while the held modifiers exactly match a
    /// jump shortcut, which swaps the first nine rows' time-ago for their
    /// key-cap chip (t3code's `showJumpHints`). Frame-transient — window
    /// deactivation clears it, so a chip cannot stick after an app switch
    /// swallows the key-up.
    pub(super) jump_hints: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    /// Embedded terminal host for right-pane Terminal surfaces — a SEPARATE
    /// entity from the bottom drawer's (own PTYs, own grid geometry; one
    /// panel can only size one visible grid at a time).
    right_terminal: Option<Entity<TerminalPanel>>,
    /// The surface-tab strip's `+` menu (Terminal / Git diff rows).
    right_plus: popover::Popup<()>,
    /// Diff surfaces by id — each tab its own [`Changes`] viewer with its own
    /// scope/base pick and diff watch (multiple diff panels, user request).
    diffs: std::collections::HashMap<u64, Entity<Changes>>,
    /// Event hookups for [`Self::diffs`] (History rows opening commit tabs).
    diff_subs: std::collections::HashMap<u64, Subscription>,
    diff_seq: u64,
    /// Subagent transcript surfaces by id — each tab a read-only
    /// [`Transcript`] pinned to its subagent doc.
    subagent_tabs: std::collections::HashMap<u64, SubagentTab>,
    subagent_seq: u64,
    /// Ordered surface tabs per panel key (drag-reorderable; stale entries —
    /// closed terminals/diffs — are skipped at read time).
    right_tabs: std::collections::HashMap<String, Vec<RightSurface>>,
    /// In-flight surface-tab drag (slide animation state).
    right_tab_drag: Option<RightTabDragState>,
    /// Surface-tab strip scroll (the strip overflows horizontally, t3
    /// ScrollArea-style; drag drop-math reads the offset back out).
    right_tab_scroll: gpui::ScrollHandle,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    notifications_page: Option<Entity<NotificationsPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    shortcuts_sub: Option<Subscription>,
    notifications_sub: Option<Subscription>,
    /// Session-row context menu, including the Copy submenu.
    chat_menu: popover::Popup<ChatMenuState>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Space-row context menu (dropdown rows): (space id, window position).
    space_menu: popover::Popup<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// Keiki's template-backed agent creation dialog.
    keiki_agent_dialog: Option<KeikiAgentDialog>,
    /// The sidebar's space-filter dropdown.
    spaces_menu: popover::Popup<spaces::SpacesMenu>,
    /// Persisted organization/sort/metadata controls beside the project filter.
    sidebar_view_menu: popover::Popup<spaces::SidebarViewMenu>,
    /// Natural-tab-order focus target for the icon-only view-options button.
    sidebar_view_trigger_focus: gpui::FocusHandle,
    /// Chat id whose STATUS CORNER is under the pointer — just that corner
    /// swaps to the archive button (t3code's settle-on-hover); hovering the
    /// row body leaves the status readable.
    chat_status_hover: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, zeron_proto::SessionStatus>,
    user_menu: popover::Popup<()>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Access token last synchronized into the device-local Copilot holder.
    copilot_synced_token: Option<String>,
    mutate_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown.
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()`.
    was_window_active: bool,
    /// Dev/testing knobs (`ZERON_OPEN_DIALOG`, `ZERON_FORCE_GATE`,
    /// `ZERON_DEMO_UPLOAD`) — see [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    debug_upload: Option<String>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    /// Mirrors `right_tween` only for takeover entry/exit, allowing the visible
    /// right-panel contents to resize with their outer frame in that mode.
    right_takeover_content_tween: Option<WidthTween>,
    /// Conversation-width tween used only while entering/leaving right-pane
    /// takeover. Normal right-pane open/close keeps the upstream flex behavior.
    main_takeover_tween: Option<WidthTween>,
    /// Changes-panel takeover (the header's expand button): the panel fills
    /// everything right of the sidebar and the conversation column collapses
    /// to zero. Session-local view state — never persisted, reset on close.
    right_pane_expanded: bool,
    /// Viewport width stamped each frame at render — the expanded panel's
    /// width target and the physical ceiling for free-form resizing
    /// ([`Self::right_target`] has no `Window`).
    viewport_width: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// The caption buttons zeron itself draws on Linux under client-side
    /// decorations, per side, already filtered to what the compositor
    /// supports — `None` off Linux or under server decorations (where the WM
    /// draws real buttons). Re-resolved every frame at the top of `render`.
    linux_captions: Option<gpui::WindowButtonLayout>,
    /// Re-renders when the desktop's button layout changes (GNOME
    /// `button-layout` gsetting). Registered on first paint — [`Shell::new`]
    /// has no window.
    button_layout_sub: Option<Subscription>,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// Clears the jump hints when the window deactivates: a Cmd+Tab away
    /// swallows the key-up, so without this the chips stay on screen for good.
    activation_sub: Option<Subscription>,
    avatar_loads: std::collections::HashMap<AvatarKey, Task<()>>,
    avatar_retries: std::collections::HashMap<AvatarKey, Task<()>>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    /// The primary transcript's spawn-chip events (subagent tabs).
    _transcript_events: Subscription,
}

impl Shell {
    pub(crate) fn set_sidebar_notice(&mut self, notice: impl Into<SharedString>) {
        self.sidebar_notice = Some(notice.into());
    }

    fn pinned_keiki_conversation_ids(&self) -> std::collections::HashSet<&str> {
        self.settings
            .pinned_keiki_conversations
            .iter()
            .map(String::as_str)
            .collect()
    }

    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Every send glides the prompt to the viewport top and reserves the
        // reply's space below it (notes-app parity).
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
            }
        });
        // Spawn chips open their subagent's transcript as a right-pane tab.
        let transcript_events = cx.subscribe(&transcript, Self::on_transcript_event);
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut settings = settings::current(cx);
        if state.read(cx).keiki_client.is_some()
            && settings.sidebar_organization == SidebarOrganization::InOneList
        {
            settings.sidebar_organization = SidebarOrganization::ByAgent;
        }
        state.update(cx, |state, cx| {
            state.set_change_requests_visible(settings.sidebar_show_pull_request, cx)
        });
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `ZERON_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("ZERON_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") | Some("settings/appearance") => {
                Route::Settings(SettingsSection::Appearance)
            }
            Some("settings/notifications") => Route::Settings(SettingsSection::Notifications),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` suppresses boot auto-select and leaves the empty state.
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `ZERON_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land;
        // `ZERON_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("ZERON_OPEN_DIALOG").ok();
        // `ZERON_DEMO_UPLOAD=<pct>:<image path>` fabricates an in-flight image
        // send on the selected chat (echo bubble + frozen thumbnail progress
        // ring) — display-only; a real upload can't be paused for a capture.
        let debug_upload = std::env::var("ZERON_DEMO_UPLOAD").ok();
        let debug_gate = match std::env::var("ZERON_FORCE_GATE").ok().as_deref() {
            Some("signin") => None,
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the Keiki engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        Self {
            state,
            transcript,
            composer,
            // Seed with the compact composer stack's rough height so the
            // first frame's clearance isn't zero (the measure corrects it).
            bottom_stack: std::rc::Rc::new(std::cell::Cell::new(120.0)),
            archived_open: true,
            archived_shown: 0,
            archived_hover: None,
            sidebar_collapsed_groups: std::collections::HashSet::new(),
            sidebar_disclosure_motion: std::collections::HashMap::new(),
            jump_hints: false,
            terminal: None,
            right_terminal: None,
            right_plus: popover::Popup::default(),
            diffs: std::collections::HashMap::new(),
            diff_subs: std::collections::HashMap::new(),
            diff_seq: 0,
            subagent_tabs: std::collections::HashMap::new(),
            subagent_seq: 0,
            right_tabs: std::collections::HashMap::new(),
            right_tab_drag: None,
            right_tab_scroll: gpui::ScrollHandle::new(),
            route,
            nav,
            archived_page: None,
            appearance_page: None,
            notifications_page: None,
            shortcuts_page: None,
            shortcuts_sub: None,
            notifications_sub: None,
            chat_menu: popover::Popup::default(),
            rename_dialog: None,
            delete_confirm: None,
            space_menu: popover::Popup::default(),
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            keiki_agent_dialog: None,
            spaces_menu: popover::Popup::default(),
            sidebar_view_menu: popover::Popup::default(),
            sidebar_view_trigger_focus: cx.focus_handle().tab_stop(true),
            chat_status_hover: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            user_menu: popover::Popup::default(),
            sidebar_notice: None,
            copilot_synced_token: None,
            mutate_task: None,
            boot,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            debug_upload,
            sidebar_tween: None,
            right_tween: None,
            right_takeover_content_tween: None,
            main_takeover_tween: None,
            right_pane_expanded: false,
            viewport_width: 1280.0,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            linux_captions: None,
            button_layout_sub: None,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            focus_sub: None,
            activation_sub: None,
            avatar_loads: std::collections::HashMap::new(),
            avatar_retries: std::collections::HashMap::new(),
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let copilot_sync = {
            let current = state.read(cx);
            if current.keiki_token.is_none() {
                self.copilot_synced_token = None;
                None
            } else {
                match (
                    current.keiki_token.as_ref(),
                    current.keiki_client.clone(),
                    current.engine(),
                ) {
                    (Some(token), Some(client), Some(_)) => {
                        let access_token = token.access_token().to_string();
                        if self.copilot_synced_token.as_deref() == Some(access_token.as_str()) {
                            None
                        } else {
                            Some((client, token.clone(), access_token))
                        }
                    }
                    _ => None,
                }
            }
        };
        if let Some((client, token, access_token)) = copilot_sync {
            let state = state.downgrade();
            cx.spawn(async move |this, cx| {
                if crate::keiki::sync_copilot_credentials(&state, &client, &token, cx).await {
                    this.update(cx, |shell, _| {
                        shell.copilot_synced_token = Some(access_token);
                    })
                    .ok();
                }
            })
            .detach();
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Capture knob: `ZERON_DEMO_UPLOAD=<pct>:<image path>` — once a chat
        // is selected, push a fake sending echo carrying that image as a
        // pending attachment and freeze upload progress at <pct>, so the
        // thumbnail progress ring can be styled/screenshotted (a real upload
        // is too fast to pause).
        if let Some(spec) = self.debug_upload.clone()
            && let Some(chat_id) = state.read(cx).selected_chat.clone()
        {
            self.debug_upload = None;
            if let Some((pct, img_path)) = spec.split_once(':')
                && let Ok(pct) = pct.parse::<u64>()
                && let Ok(att) = crate::attachments::stage_file(std::path::Path::new(img_path))
            {
                let pending_path = format!("pending/{}/{}", att.id, att.name);
                let device_ids: Vec<String> = {
                    let s = state.read(cx);
                    s.selected_chat_row()
                        .map(|c| c.device_id.clone())
                        .into_iter()
                        .chain(s.local_device_id.clone())
                        .chain(Some("local".to_string()))
                        .collect()
                };
                for device_id in &device_ids {
                    crate::attachments::seed_attachment(
                        device_id,
                        &pending_path,
                        &att.name,
                        att.image.clone(),
                    );
                }
                let text = crate::attachments::with_attachments(
                    "Here is the screenshot of the bug.",
                    std::slice::from_ref(&pending_path),
                );
                let echo = zeron_doc::SessionMessageEntry {
                    id: "demo-upload-echo".into(),
                    role: zeron_doc::MessageRole::User,
                    parts: vec![zeron_doc::MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: chrono::Utc::now().timestamp_millis(),
                    device_id: "local".into(),
                    status: None,
                    continuation_of: None,
                };
                state.update(cx, |s, cx| {
                    s.push_echo(&chat_id, echo);
                    s.begin_upload_progress(
                        100,
                        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pct)),
                    );
                    cx.notify();
                });
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent. Desktop banners
        // (`notify::post`) ride the SAME edges and gates behind their own
        // settings flag — one detector, two outputs, so the banner can never
        // fire where the chime wouldn't.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        //
        // SEND-PENDING-GATED too (`AppState::send_pending`): a send whose
        // queued command the host hasn't executed yet can still surface a
        // phantom Working→Idle (a stale Working row crossing the 45s gate on
        // the send's own re-render, or a late old Idle row) — the done-chime
        // stays quiet for that chat until the host acks, while the baseline
        // keeps tracking silently so the ghost edge never fires later. The
        // question chime is NOT gated: an instant AwaitingInput ack should
        // still ring.
        {
            let now = Utc::now();
            type Ping = (String, zeron_proto::SessionStatus, bool, Option<String>);
            let sessions: Vec<Ping> = {
                let state = state.read(cx);
                state
                    .sessions
                    .iter()
                    .map(|s| {
                        use zeron_proto::view::Indicator;
                        let status = match zeron_proto::view::effective_indicator(Some(s), now) {
                            Indicator::Working => zeron_proto::SessionStatus::Working,
                            Indicator::AwaitingInput => zeron_proto::SessionStatus::AwaitingInput,
                            Indicator::Errored => zeron_proto::SessionStatus::Errored,
                            Indicator::None => zeron_proto::SessionStatus::Idle,
                        };
                        let send_pending = state.send_pending(&s.chat_id, now);
                        let title = state
                            .chats
                            .iter()
                            .find(|c| c.id == s.chat_id)
                            .and_then(|c| c.title.clone());
                        (s.chat_id.clone(), status, send_pending, title)
                    })
                    .collect()
            };
            // Background-only banners: `active_window()` is app-level (any
            // Zeron window being key), so a ping for a *background chat* in a
            // focused app still stays a chime — you're already looking at
            // Zeron; the sidebar dot carries the rest.
            let app_focused = cx.active_window().is_some();
            for (chat_id, status, send_pending, title) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                    && !(send_pending && sound == crate::sound::Sound::Done)
                {
                    if self.settings.sound_enabled {
                        crate::sound::play(sound);
                    }
                    if self.settings.notifications_enabled
                        && !(self.settings.notifications_background_only && app_focused)
                    {
                        let title = title.unwrap_or_else(|| "New session".into());
                        let body = match sound {
                            crate::sound::Sound::Done => "Run finished",
                            crate::sound::Sound::Request => "Waiting on your input",
                        };
                        crate::notify::post(&title, body);
                    }
                }
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // sidebar context (project AND its device) follows it, even
                // over a remembered "no project" opt-out. Otherwise the last
                // selected project stands, unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(&exists);
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        self.settings.last_space_id.clone().filter(&exists)
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched empty state REPLACES that entry —
            // zeron's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.right_takeover_content_tween = None;
            self.main_takeover_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. No selection keys per space so panel state
    /// remains scoped to the visible sidebar context.
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. Still hidden when no chat is
    /// selected because there is no session to inspect.
    fn right_pane_open(&self, cx: &App) -> bool {
        !self.active_chat.is_empty() && self.panels.get(&self.panel_key(cx)).changes_open
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if !self.right_pane_open(cx) {
            0.0
        } else {
            // Manual sizing preserves a usable conversation column. Takeover
            // intentionally consumes it completely. Both ride the sidebar
            // tween so toggling it remains seamless.
            let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            if self.right_pane_expanded {
                right_pane_takeover_width(self.viewport_width, sidebar_now)
            } else {
                self.settings
                    .right_pane_width
                    .min(right_pane_max_width(self.viewport_width, sidebar_now))
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git gate: the pane hosts terminals too (see `right_pane_open`).
        let from = self.right_target(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let from_main = conversation_width(self.viewport_width, sidebar_now, from);
        let was_expanded = self.right_pane_expanded;
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if !open {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        let to = self.right_target(cx);
        self.right_tween = Some(WidthTween::new(from, to));
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = was_expanded.then(|| {
            WidthTween::new(
                from_main,
                conversation_width(self.viewport_width, sidebar_now, to),
            )
        });
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }

    fn right_terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.right_terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new_embedded(self.state.clone(), cx));
        self.right_terminal = Some(terminal.clone());
        terminal
    }

    /// The right pane's surface tabs in the STORED (drag-reorderable) order —
    /// `(surface, title)`; entries whose backing tab/entity is gone are
    /// skipped.
    fn right_surface_rows(&self, cx: &App) -> Vec<(RightSurface, SharedString)> {
        let key = self.panel_key(cx);
        let stored: &[RightSurface] = self
            .right_tabs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let terminals: Vec<(u64, SharedString, bool)> = self
            .right_terminal
            .as_ref()
            .map(|t| t.read(cx).tab_summaries(cx))
            .unwrap_or_default();
        stored
            .iter()
            .filter_map(|surface| match surface {
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    // Contextual title (user request): the pane's scope
                    // label, or the pinned commit's subject.
                    .map(|changes| (*surface, changes.read(cx).tab_title())),
                RightSurface::Terminal(tab) => terminals
                    .iter()
                    .find(|(k, _, _)| k == tab)
                    .map(|(_, title, _)| (*surface, title.clone())),
                RightSurface::Subagent(id) => self
                    .subagent_tabs
                    .get(id)
                    .map(|tab| (*surface, tab.title.clone())),
                RightSurface::Picker => None,
            })
            .collect()
    }

    /// Drag-reorder a surface tab within this chat's strip.
    fn reorder_right_tabs(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key)
            && from < tabs.len()
            && to < tabs.len()
            && from != to
        {
            let surface = tabs.remove(from);
            tabs.insert(to, surface);
            cx.notify();
        }
    }

    /// Track the hovered drop slot mid-drag (the terminal drawer's
    /// `update_drag_over`, ported: epoch bumps restart the slide tween).
    fn update_right_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.right_tab_drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.right_tab_drag = Some(RightTabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// The surface that actually renders: the stored pick when it still
    /// exists, else the first remaining tab, else the picker. Terminal keys
    /// go stale when their tab closes/exits — never render a dead surface.
    fn resolved_right_active(&self, cx: &App) -> RightSurface {
        let picked = self.panels.get(&self.panel_key(cx)).right_active;
        let rows = self.right_surface_rows(cx);
        let exists = match picked {
            RightSurface::Picker => false,
            surface => rows.iter().any(|(s, _)| *s == surface),
        };
        if exists {
            picked
        } else {
            rows.first()
                .map(|(s, _)| *s)
                .unwrap_or(RightSurface::Picker)
        }
    }

    fn set_right_active(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        self.panels.update(&key, |p| p.right_active = surface);
        match surface {
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
            }
            RightSurface::Diff(id) => {
                if let Some(changes) = self.diffs.get(&id).cloned() {
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                }
            }
            // The tab's feed (watch or snapshot) runs from open to close —
            // activation needs no revalidation.
            RightSurface::Subagent(_) => {}
            RightSurface::Picker => {}
        }
        cx.notify();
    }

    /// The picker's Git card / the `+` menu's Diff row: every click opens a
    /// FRESH diff tab with its own scope/base selection (multiple diff
    /// panels, user request).
    fn add_diff_surface(&mut self, cx: &mut Context<Self>) {
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.register_diff_surface(changes, cx);
    }

    /// A History row click: the commit opens as its own pinned diff tab
    /// (user request).
    fn add_commit_diff_surface(
        &mut self,
        commit: zeron_proto::GitHistoryCommit,
        cx: &mut Context<Self>,
    ) {
        let changes = cx.new(|cx| Changes::for_commit(self.state.clone(), commit, cx));
        self.register_diff_surface(changes, cx);
    }

    fn register_diff_surface(&mut self, changes: Entity<Changes>, cx: &mut Context<Self>) {
        self.diff_seq += 1;
        let id = self.diff_seq;
        let sub = cx.subscribe(&changes, |this: &mut Self, _, event, cx| match event {
            ChangesEvent::OpenCommit(commit) => {
                this.add_commit_diff_surface(commit.clone(), cx);
            }
        });
        self.diffs.insert(id, changes);
        self.diff_subs.insert(id, sub);
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Diff(id));
        self.set_right_active(RightSurface::Diff(id), cx);
    }

    /// The picker's Terminal card / the `+` menu's Terminal row: every click
    /// opens a fresh embedded terminal tab.
    fn add_terminal_surface(&mut self, cx: &mut Context<Self>) {
        let panel = self.right_terminal_panel(cx);
        let opened = panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.open_tab_for_selected(cx)
        });
        if let Some(tab) = opened {
            let key = self.panel_key(cx);
            self.right_tabs
                .entry(key)
                .or_default()
                .push(RightSurface::Terminal(tab));
            self.set_right_active(RightSurface::Terminal(tab), cx);
        }
    }

    /// Spawn-chip events from the primary transcript AND from subagent-tab
    /// transcripts (nested spawns open their own tabs).
    fn on_transcript_event(
        &mut self,
        _: Entity<Transcript>,
        event: &TranscriptEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TranscriptEvent::OpenSubagent {
                chat_id,
                doc_id,
                title,
                frozen,
            } => {
                self.add_subagent_surface(
                    chat_id.clone(),
                    doc_id.clone(),
                    title.clone(),
                    *frozen,
                    cx,
                );
            }
        }
    }

    /// A spawn chip's "Open subagent": focus the existing tab for that doc,
    /// or open one. `frozen` (subagent done/failed) tries the uploaded
    /// transcript blob first and falls back to the live doc watch; running
    /// subagents watch the doc directly.
    fn add_subagent_surface(
        &mut self,
        _chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
        cx: &mut Context<Self>,
    ) {
        // The chip lives in the conversation column — the pane it opens into
        // may still be closed.
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
        if let Some((&id, _)) = self
            .subagent_tabs
            .iter()
            .find(|(_, tab)| tab.doc_id == doc_id)
        {
            self.set_right_active(RightSurface::Subagent(id), cx);
            return;
        }
        self.subagent_seq += 1;
        let id = self.subagent_seq;
        // A live subagent follows its streaming end (main-transcript feel);
        // a frozen one reads top-down.
        let transcript =
            cx.new(|cx| Transcript::for_doc(self.state.clone(), doc_id.clone(), !frozen, cx));
        let events = cx.subscribe(&transcript, Self::on_transcript_event);
        self.state
            .update(cx, |s, cx| s.watch_subagent_doc(doc_id.clone(), cx));
        self.subagent_tabs.insert(
            id,
            SubagentTab {
                doc_id,
                title: title.into(),
                transcript,
                _events: events,
            },
        );
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Subagent(id));
        self.set_right_active(RightSurface::Subagent(id), cx);
    }

    /// A surface tab's ✕. The active fallback happens naturally through
    /// [`Self::resolved_right_active`] on the next frame.
    fn close_right_surface(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key) {
            tabs.retain(|s| *s != surface);
        }
        match surface {
            RightSurface::Diff(id) => {
                // Dropping the entity tears down its diff watch.
                self.diffs.remove(&id);
                self.diff_subs.remove(&id);
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.close_tab_by_key(tab, window, cx));
            }
            RightSurface::Subagent(id) => {
                // Unwatch drops the watch task — that cancels the engine-side
                // watch and unpins the subagent doc from the engine LRU.
                if let Some(tab) = self.subagent_tabs.remove(&id) {
                    self.state
                        .update(cx, |s, _| s.unwatch_subagent_doc(&tab.doc_id));
                }
            }
            RightSurface::Picker => {}
        }
        self.panels.update(&key, |p| {
            if p.right_active == surface {
                p.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (zeron `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (zeron terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in zeron's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // No arbitrary percentage ceiling, but retain the chat's usable 300px
        // floor instead of allowing the conversation to collapse to zero.
        let max = right_pane_max_width(viewport, self.sidebar_target());
        self.settings.right_pane_width = if max >= RIGHT_PANE_MIN {
            width.clamp(RIGHT_PANE_MIN, max)
        } else {
            max
        };
        self.right_tween = None;
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Publish this view's working copy to the central settings store. The
    /// store owns the single debounce task and the only production writer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        self.settings.ui_font_family = crate::typography::requested(cx);
        self.settings.ui_font_size = crate::typography::font_size(cx);
        settings::replace(self.settings.clone(), SavePolicy::Debounced, cx);
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    /// Close the user menu through the exit animation (no-op when closed).
    fn close_user_menu(&mut self, cx: &mut Context<Self>) {
        if self.user_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.user_menu);
            cx.notify();
        }
    }

    /// Close the session-row context menu through the exit animation.
    fn close_chat_menu(&mut self, cx: &mut Context<Self>) {
        if self.chat_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.chat_menu);
            cx.notify();
        }
    }

    fn toggle_keiki_conversation_pin(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if !crate::keiki::is_keiki_chat(&chat_id) {
            self.close_chat_menu(cx);
            return;
        }
        let mut pinned = self.settings.pinned_keiki_conversations.clone();
        {
            let state = self.state.read(cx);
            prune_pinned_keiki_conversations(&mut pinned, &state.chats);
        }
        if let Some(index) = pinned.iter().position(|id| id == &chat_id) {
            pinned.remove(index);
        } else {
            pinned.push(chat_id);
        }
        self.settings.pinned_keiki_conversations = pinned.clone();
        settings::update(SavePolicy::Immediate, cx, |current| {
            current.pinned_keiki_conversations = pinned;
        });
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn view_keiki_conversation(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let url = {
            let state = self.state.read(cx);
            state.keiki_client.as_ref().and_then(|client| {
                crate::keiki::conversation_locator(&chat_id).and_then(|locator| {
                    crate::keiki::conversation_dashboard_url(client.base_url(), &locator)
                })
            })
        };
        if let Some(url) = url {
            cx.open_url(&url);
        } else {
            self.set_sidebar_notice("Keiki conversation is not ready yet");
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn open_chat_copy_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.chat_menu.open_mut() {
            menu.page = ChatMenuPage::Copy;
            cx.notify();
        }
    }

    fn copy_harness_session_id(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let id = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.harness_session_id.clone());
        if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(id));
            self.sidebar_notice = Some("Harness session ID copied".into());
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let page = cx.new(|cx| {
                        NotificationsPage::new(
                            self.settings.sound_enabled,
                            self.settings.notifications_enabled,
                            self.settings.notifications_background_only,
                            cx,
                        )
                    });
                    // Persist the flags whenever the page flips one.
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::Changed {
                                sound,
                                desktop,
                                background_only,
                            } = *event;
                            this.settings.sound_enabled = sound;
                            this.settings.notifications_enabled = desktop;
                            this.settings.notifications_background_only = background_only;
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.set_chat_archived(chat_id, true, cx);
    }

    /// The Archive session shortcut. With no chat open, or with an already
    /// archived one, it does nothing — the shortcut archives, it never
    /// unarchives.
    fn archive_selected_chat(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self
            .state
            .read(cx)
            .archivable_selected_chat()
            .map(str::to_string)
        else {
            return;
        };
        self.archive_chat(chat_id, cx);
    }

    pub(super) fn set_chat_archived(
        &mut self,
        chat_id: String,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
            cx,
        );
        cx.notify();
    }

    /// A jump shortcut: open the sidebar row at `slot`. A slot past the end of
    /// a short list does nothing. Reads the DISPLAYED order — sort and
    /// grouping view options permute the list, and the chip on a row must
    /// name the key that opens it.
    fn jump_to_session(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(chat_id) = self.sidebar_visible_order(cx).into_iter().nth(slot) else {
            return;
        };
        // Same path a click on that row takes.
        self.open_chat(chat_id, cx);
    }

    /// Whether the add-space palette owns the keyboard. GPUI runs a matched
    /// binding before any `on_key_down`, so session-navigation shortcuts must
    /// stay quiet underneath it.
    pub(super) fn overlay_owns_keyboard(&self, _cx: &App) -> bool {
        self.add_space.is_some()
    }

    /// Track the held modifiers so the sidebar can show its jump hints. Only a
    /// change in visibility repaints — modifier traffic is otherwise constant.
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let mods = &event.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        // No hints while an overlay owns the keyboard — the jumps they
        // advertise are suppressed there.
        let visible = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && jump_hints_visible(&self.settings.keymap, primary, mods.alt, mods.shift);
        self.set_jump_hints(visible, cx);
    }

    pub(super) fn set_jump_hints(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.jump_hints != visible {
            self.jump_hints = visible;
            cx.notify();
        }
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    fn start_keiki_sign_in(&mut self, cx: &mut Context<Self>) {
        crate::keiki::begin_sign_in(self.state.clone(), "manage", cx);
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `ZERON_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    fn tween_active(&self, tween: Option<WidthTween>) -> bool {
        tween.is_some_and(|tween| {
            !self.reduced_motion
                && tween.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
        })
    }

    fn active_tween_endpoints(&self, tween: Option<WidthTween>) -> Option<(f32, f32)> {
        tween
            .filter(|transition| {
                !self.reduced_motion
                    && transition.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
            })
            .map(|transition| (transition.from, transition.to))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Right-anchored variant for the changes pane. The outer width follows the
    /// existing shell tween, while descendants retain the larger endpoint's
    /// geometry for that 200ms transition. This mirrors the sidebar's stable
    /// inner/clipped outer behavior without changing the center column's
    /// upstream flex layout.
    fn right_pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        let takeover_width = self
            .active_tween_endpoints(self.right_takeover_content_tween)
            .map(|_| self.eval_tween(self.right_takeover_content_tween, target));
        let content_width =
            right_panel_content_width(target, self.active_tween_endpoints(tween), takeover_width);
        div()
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(content_width))
                    .child(inner),
            )
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of zeron __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (zeron remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen, self.linux_left_caption_count()),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + TITLEBAR_IDENTITY_GAP
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_title_bar(cx),
            Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET)));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (zeron's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// zeron window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        // The titlebar owns Copilot session creation on the chat route.
        let plus_alpha = self.titlebar_plus_alpha(cx);
        let show_plus = plus_alpha > 0.01;
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .px(px(TITLEBAR_CLUSTER_PAD))
            .children(self.titlebar_spacer(TITLEBAR_CLUSTER_PAD))
            // Left-side Linux captions (GNOME `close:…` layouts): the
            // root-level caption overlay owns the buttons; the cluster row
            // just starts past them, at the shared 2px rhythm.
            .children((self.linux_left_caption_count() > 0).then(|| {
                div()
                    .flex_none()
                    .h_full()
                    .w(px(caption_buttons_width(self.linux_left_caption_count())))
            }))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(
                div()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(TITLEBAR_CONTROL_GAP))
                    .child(nav_history_button(
                        "nav-back",
                        icons::ARROW_LEFT,
                        can_back,
                        &theme,
                        cx.listener(|this, _, _, cx| this.navigate_back(cx)),
                    ))
                    .child(nav_history_button(
                        "nav-forward",
                        icons::ARROW_RIGHT,
                        can_forward,
                        &theme,
                        cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
                    )),
            )
            .children(show_plus.then(|| {
                div()
                    .flex_none()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .opacity(plus_alpha)
                    .child(window_control_button(
                        "titlebar-new-session",
                        icons::PLUS,
                        &theme,
                        cx.listener(|this, _, _, cx| this.open_new_session(cx)),
                    ))
            }))
            .into_any_element()
    }

    /// The titlebar owns Copilot session creation on the chat route.
    pub(super) fn titlebar_plus_alpha(&self, _cx: &App) -> f32 {
        titlebar_new_session_alpha(matches!(self.route, Route::Chat))
    }

    /// Native Windows caption controls integrated into Zeron's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    /// Which caption buttons zeron itself must draw on Linux: under
    /// client-side decorations (the Wayland default) nobody else will —
    /// without these the window has NO minimize/maximize/close at all.
    /// Server-side decorations (X11 WMs, KDE with SSD) already draw real
    /// buttons, so `None` there. The desktop's layout (GNOME's
    /// `button-layout` gsetting via `cx.button_layout()`) decides side and
    /// order — min/max/close on the right by default; controls the
    /// compositor can't do (e.g. minimize on some Wayland compositors) drop
    /// out, close always stays.
    #[cfg(target_os = "linux")]
    fn resolve_linux_captions(window: &Window, cx: &App) -> Option<gpui::WindowButtonLayout> {
        use gpui::{MAX_BUTTONS_PER_SIDE, WindowButton, WindowButtonLayout};
        if !matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        ) {
            return None;
        }
        let layout = cx
            .button_layout()
            .unwrap_or_else(WindowButtonLayout::linux_default);
        let supported = window.window_controls();
        let filter_side = |side: [Option<WindowButton>; MAX_BUTTONS_PER_SIDE]| {
            let mut out = [None; MAX_BUTTONS_PER_SIDE];
            let mut i = 0;
            for button in side.into_iter().flatten() {
                let keep = match button {
                    WindowButton::Minimize => supported.minimize,
                    WindowButton::Maximize => supported.maximize,
                    WindowButton::Close => true,
                };
                if keep {
                    out[i] = Some(button);
                    i += 1;
                }
            }
            out
        };
        let layout = WindowButtonLayout {
            left: filter_side(layout.left),
            right: filter_side(layout.right),
        };
        (layout.left[0].is_some() || layout.right[0].is_some()).then_some(layout)
    }

    #[cfg(not(target_os = "linux"))]
    fn resolve_linux_captions(_window: &Window, _cx: &App) -> Option<gpui::WindowButtonLayout> {
        None
    }

    pub(super) fn linux_left_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.left.iter().flatten().count())
    }

    pub(super) fn linux_right_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.right.iter().flatten().count())
    }

    /// Right padding titlebar content needs to clear the platform's caption
    /// controls (native Windows cluster / zeron-drawn Linux buttons).
    pub(super) fn titlebar_right_pad(&self, base: f32) -> f32 {
        titlebar_right_padding(
            cfg!(target_os = "windows"),
            self.linux_right_caption_count(),
            base,
        )
    }

    /// Zeron-drawn Linux caption controls, one overlay per populated side.
    /// Shell-level chrome like the Windows cluster: mounted at the root so
    /// they stay above the splash and every auth/org/error gate.
    fn render_linux_caption_controls(&self, window: &Window, cx: &App) -> Vec<AnyElement> {
        let Some(layout) = self.linux_captions else {
            return Vec::new();
        };
        let theme = Theme::of(cx);
        let is_maximized = window.is_maximized();
        // Ids can be per-button (not per-side): the layout parser dedups, so
        // a button never appears on both sides at once.
        let strip = |buttons: &[Option<gpui::WindowButton>]| {
            div()
                .absolute()
                .top_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .pt(px(Theme::TITLEBAR_TOP_PAD))
                .gap(px(2.0))
                .px(px(10.0))
                .children(buttons.iter().flatten().map(|button| {
                    match button {
                        gpui::WindowButton::Minimize => linux_caption_button(
                            "window-minimize",
                            icons::WINDOW_MINIMIZE,
                            false,
                            theme,
                            |_, window, _| window.minimize_window(),
                        )
                        .into_any_element(),
                        gpui::WindowButton::Maximize => {
                            let (id, icon_path) = if is_maximized {
                                ("window-restore", icons::WINDOW_RESTORE)
                            } else {
                                ("window-maximize", icons::WINDOW_MAXIMIZE)
                            };
                            linux_caption_button(id, icon_path, false, theme, |_, window, _| {
                                window.zoom_window()
                            })
                            .into_any_element()
                        }
                        gpui::WindowButton::Close => linux_caption_button(
                            "window-close",
                            icons::CLOSE,
                            true,
                            theme,
                            |_, window, _| window.remove_window(),
                        )
                        .into_any_element(),
                    }
                }))
        };
        let mut out = Vec::new();
        if layout.left[0].is_some() {
            out.push(strip(&layout.left).left_0().into_any_element());
        }
        if layout.right[0].is_some() {
            out.push(strip(&layout.right).right_0().into_any_element());
        }
        out
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The sidebar is part of the resolved theme. A second fixed-Zeron
        // palette here made imported families look split in half and froze
        // activity/glyph personality independently of the selected variant.
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation. The content row spans the
        // full window height (the titlebar overlays it), so the column pads
        // itself below the chrome.
        self.pane_container(
            self.sidebar_tween,
            target,
            div()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .child(inner)
                .into_any_element(),
        )
    }

    /// Settings-mode sidebar (zeron settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Notifications => icons::BELL,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(crate::typography::ui_rems(13.0))
                                .when(selected, |el| {
                                    // Same tokens as the main sidebar's session
                                    // rows — the two sidebars must feel alike.
                                    el.bg(crate::theme::glass_selected_bg())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (zeron settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (zeron settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    fn avatar_element(
        &mut self,
        agent_id: &str,
        element_id: SharedString,
        state: keiki_model::AvatarState,
        rendered_px: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bucket = keiki_api::avatar_size_bucket((rendered_px * 2.0).round() as u32);
        let key = AvatarKey::new(agent_id, state, avatars::avatar_theme(theme), bucket);
        let snapshot = avatars::snapshot(&key);
        match snapshot {
            AvatarSnapshot::Loaded(image) => div()
                .size(px(rendered_px))
                .flex_none()
                .child(
                    gpui::img(image)
                        .id(element_id)
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .into_any_element(),
            AvatarSnapshot::Loading => {
                if avatars::begin_load(&key) {
                    self.spawn_avatar_load(key, cx);
                }
                div().size(px(rendered_px)).flex_none().into_any_element()
            }
            AvatarSnapshot::Error { retry_in } => {
                if avatars::begin_load(&key) {
                    self.spawn_avatar_load(key.clone(), cx);
                }
                self.schedule_avatar_retry(key, retry_in, cx);
                div().size(px(rendered_px)).flex_none().into_any_element()
            }
        }
    }

    fn spawn_avatar_load(&mut self, key: AvatarKey, cx: &mut Context<Self>) {
        let Some(client) = self.state.read(cx).keiki_client.clone() else {
            avatars::store_error(&key);
            return;
        };
        let task_key = key.clone();
        let load_key = key.clone();
        let cleanup_key = key.clone();
        let generation = avatars::generation();
        let task = cx.spawn(async move |this, cx| {
            let request = cx.update(|cx| {
                let request_client = client.clone();
                let request_key = task_key.clone();
                gpui_tokio::Tokio::spawn(cx, async move {
                    request_client
                        .agent_avatar(
                            &request_key.agent_id,
                            request_key.bucket,
                            request_key.state,
                            request_key.theme,
                        )
                        .await
                })
            });
            match request.await {
                Ok(Ok(bytes)) => {
                    if avatars::generation() == generation {
                        avatars::store_loaded(load_key.clone(), bytes);
                    }
                }
                Ok(Err(error)) => {
                    if avatars::generation() == generation {
                        tracing::warn!(%error, agent_id = %load_key.agent_id, "Keiki avatar request failed");
                        avatars::store_error(&load_key);
                    }
                }
                Err(error) => {
                    if avatars::generation() == generation {
                        tracing::warn!(%error, agent_id = %load_key.agent_id, "Keiki avatar request task failed");
                        avatars::store_error(&load_key);
                    }
                }
            }
            if let Err(error) = this.update(cx, |shell, cx| {
                shell.avatar_loads.remove(&cleanup_key);
                cx.notify();
            }) {
                tracing::debug!(%error, "Keiki avatar shell update skipped");
            }
        });
        self.avatar_loads.insert(key, task);
    }

    fn schedule_avatar_retry(&mut self, key: AvatarKey, delay: Duration, cx: &mut Context<Self>) {
        if delay == Duration::MAX || self.avatar_retries.contains_key(&key) {
            return;
        }
        let wake = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(delay + Duration::from_millis(60))
                .await;
            if let Err(error) = this.update(cx, |shell, cx| {
                shell.avatar_retries.remove(&wake);
                cx.notify();
            }) {
                tracing::debug!(%error, "Keiki avatar retry update skipped");
            }
        });
        self.avatar_retries.insert(key, task);
    }

    /// One session row: context + status on line one, harness + title on line
    /// two, and source metadata below. Working uses the live thread glyph in
    /// the status corner. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &mut self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        change_request: Option<zeron_proto::ChangeRequestSummary>,
        harness: Option<zeron_proto::HarnessId>,
        status: zeron_proto::ChatIndicator,
        selected: bool,
        archived: bool,
        // This row's jump combo while the hint overlay is up. It takes the
        // corner outright — above hover and above the status word — so all
        // nine chips appear together instead of leaving a hole on whichever
        // row is busy or under the pointer.
        jump_label: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Activity, not position (t3code Sidebar): status is a small colored
        // word + glyph in the row's top-right corner — Working animates the
        // composer-strip spinner, Done wears a check; Idle rows show the
        // relative time instead. Hovering the ROW swaps the corner for the
        // ARCHIVE button (UNARCHIVE on rows in the sidebar's archived
        // accordion), t3code's settle-on-hover.
        let corner_hovered = self.chat_status_hover.as_deref() == Some(id.as_str());
        let keiki_agent_id =
            crate::keiki::conversation_locator(&id).and_then(|locator| locator.agent_id);
        let keiki_avatar = keiki_agent_id.as_deref().map(|agent_id| {
            self.avatar_element(
                agent_id,
                format!("keiki-avatar-{id}").into(),
                crate::avatars::avatar_state(status),
                SIDEBAR_KEIKI_AVATAR_SIZE,
                theme,
                cx,
            )
        });
        // Send-truth overrides: a send unadopted past the grace window is
        // FAILED (explicit, with the transcript's retry affordance); a send
        // whose delivery path is degraded is QUEUED, not Working — the
        // pending pill tells the truth instead of faking a spinner.
        let (queued, undelivered) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            (false, state.send_undelivered(&id, now))
        };
        let status_color = if undelivered {
            theme.danger
        } else if queued {
            theme.warning
        } else {
            spaces::status_dot_color(status, theme)
        };
        let status_label: Option<&'static str> = if undelivered {
            Some("Failed")
        } else if queued {
            Some("Queued")
        } else {
            match status {
                zeron_proto::ChatIndicator::Working => Some("Working"),
                zeron_proto::ChatIndicator::AwaitingInput => Some("Input"),
                zeron_proto::ChatIndicator::Errored => Some("Failed"),
                zeron_proto::ChatIndicator::Completed => None,
                zeron_proto::ChatIndicator::Idle => None,
            }
        };
        let shows_metadata = branch.is_some() || change_request.is_some();
        let queued = queued && !undelivered;
        let working = status == zeron_proto::ChatIndicator::Working && !queued && !undelivered;
        let corner_body: AnyElement = if let Some(label) = jump_label {
            // The jump hint replaces the status/time corner while the modifier
            // is held, cut to the sidebar PR badge's exact cloth
            // (`pull_request_badge`, Sidebar surface): pinned 16px, px 4,
            // rounded 4, borderless 0.08-fill with 0.85 text of one tone —
            // neutral here — and the label in the badge's mono at 10 MEDIUM.
            // Any other geometry reads as a second badge system on the row.
            {
                let tone = theme.text_muted;
                div()
                    .h(px(16.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .bg(tone.opacity(0.08))
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tone.opacity(0.85))
                    .font_family(theme.font_mono.clone())
                    .child(label)
                    .into_any_element()
            }
        } else if corner_hovered {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .h(px(18.0))
                // The pill's padding bleeds right into the row's padding so
                // its TEXT right-aligns exactly where the status word/time
                // sits — the swap moves pixels around the label, not it.
                // 4px: what's left of the row's 8px padding then equals the
                // 4px of air above the pill (18px tall on the 14px line,
                // 6px row padding minus the 2px overflow).
                .px(px(4.0))
                .mr(px(-4.0))
                .rounded(px(5.0))
                .bg(crate::theme::wash(0.10))
                .hover(|s| s.bg(crate::theme::wash(0.18)))
                .child(
                    icon(if archived {
                        icons::ARCHIVE_UP_MINIMALISTIC
                    } else {
                        icons::ARCHIVE_MINIMALISTIC
                    })
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(if archived {
                            "Unarchive"
                        } else {
                            "Archive"
                        })),
                )
                .into_any_element()
        } else {
            // Glyphs keep their own slot so completed rows can show the check
            // without manufacturing a text gap or falling through to time.
            let glyph: AnyElement = if status == zeron_proto::ChatIndicator::Completed {
                icon(icons::CHECK)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(status_color)
                    .into_any_element()
            } else if working {
                loaders::mini_glyph_spinner(
                    format!("chat-working-{id}"),
                    2.0,
                    theme.glyph,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element()
            } else {
                div()
                    .size(px(6.0))
                    .flex_none()
                    .rounded_full()
                    .bg(status_color)
                    .into_any_element()
            };
            match status_label {
                Some(label) => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(glyph)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(status_color)
                            .child(SharedString::from(label)),
                    )
                    .into_any_element(),
                None if status == zeron_proto::ChatIndicator::Completed => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(glyph)
                    .into_any_element(),
                None => div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(time_ago.clone())
                    .into_any_element(),
            }
        };
        // One stable wrapper across both states (identity keeps the hover
        // from flickering as the content swaps); the swap is driven by the
        // ROW's hover (user request — corner-only felt undiscoverable), but
        // archiving only clicks on the corner itself, so the row's own click
        // stays the selector.
        let corner: AnyElement = {
            let archive_id = id.clone();
            div()
                .id(SharedString::from(format!("chat-corner-{id}")))
                .flex_none()
                // Pin the corner to line 1's text height so the archive pill
                // (taller, padded) overflows vertically instead of growing the
                // row — the swap must not shift the card's content.
                // NO occlude: the ROW's hover drives the swap, and an
                // occluding corner un-hovered the row underneath it —
                // pill mounts, steals the pointer, row un-hovers, pill
                // unmounts, repeat (user-reported flicker). The pill's
                // stop_propagation click is separation enough.
                .h(px(14.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(corner_hovered, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_chat_archived(archive_id.clone(), !archived, cx);
                    }))
                })
                .child(corner_body)
                .into_any_element()
        };
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let select_id = id.clone();
        let menu_id = id.clone();
        // Hover fades over transition-colors (zeron session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A selected row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if selected { selected_wash } else { hover };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            // No selection ring (user request) — the wash alone marks the
            // active row.
            // Row hover drives BOTH the wash blend and the corner's
            // status→Archive swap (one listener — gpui allows a single
            // hover listener per element).
            .on_hover({
                let fade_hover = motion::hover_listener(fade_key.clone());
                let hover_id = id.clone();
                cx.listener(move |this, hovered: &bool, window, cx| {
                    fade_hover(hovered, window, cx);
                    if *hovered {
                        if this.chat_status_hover.as_deref() != Some(hover_id.as_str()) {
                            this.chat_status_hover = Some(hover_id.clone());
                            cx.notify();
                        }
                    } else if this.chat_status_hover.as_deref() == Some(hover_id.as_str()) {
                        this.chat_status_hover = None;
                        cx.notify();
                    }
                })
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu.open(ChatMenuState {
                        chat_id: menu_id.clone(),
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            )
            // Line 1: "project @ device", status word / time-ago right.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .line_height(px(14.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    .child(div().text_color(subline).child(corner)),
            )
            // Line 2: harness identity belongs directly with the title,
            // instead of floating as unrelated metadata below it.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP))
                    .when_some(keiki_avatar, |el, avatar| el.child(avatar))
                    .when_some(
                        keiki_agent_id
                            .is_none()
                            .then(|| harness.map(crate::pickers::harness_brand_icon))
                            .flatten(),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(subline).opacity(0.8)),
                            )
                        },
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(17.0))
                            .child(title),
                    ),
            )
            // Line 3 is structural, not reserved whitespace: compact states
            // omit it completely when both Branch and Pull request are hidden.
            .when(shows_metadata, |row| {
                row.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .when_some(branch, |el, branch| {
                            el.child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(subline),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .line_height(px(14.0))
                                    .text_color(subline)
                                    .child(branch),
                            )
                        })
                        // Stable invisible spring keeps the optional PR badge
                        // pinned right without changing no-PR paint.
                        .child(div().flex_1().min_w_0())
                        .when_some(change_request, |el, summary| {
                            el.child(crate::change_requests::pull_request_badge(
                                format!("chat-pr-{id}").into(),
                                summary,
                                crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                theme,
                            ))
                        }),
                )
            })
            .into_any_element()
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    /// The global connection line. `None` while healthy (`Connected`) or on
    /// local profiles (`Disabled`) — and the engine's degrade grace means it
    /// only exists during REAL outages, never join/wake blips. No surface,
    /// no border (v0.2.12 feedback): a bare spinner + faint caption while
    /// reconnecting; an amber dot only when the OS says offline. The
    /// transport error belongs in logs, not the sidebar.
    fn render_connection_pill(
        &self,
        _theme: &Theme,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }

    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, cx);
        let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
        if self.sidebar_prev_order != order {
            let key_order_changed = sidebar_key_order_changed(&self.sidebar_prev_order, &order);
            if !self.sidebar_prev_order.is_empty() {
                let offsets = if key_order_changed {
                    resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP)
                } else {
                    std::collections::HashMap::new()
                };
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                    .map(|(k, _)| k.clone())
                    .collect();
                if key_order_changed && (!offsets.is_empty() || !new_keys.is_empty()) {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }
        let epoch = self.resort_epoch;
        let list_items: Vec<AnyElement> = keyed
            .into_iter()
            .map(|(key, _, element)| {
                if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                    let id = SharedString::from(format!("resort-{epoch}-{key}"));
                    div()
                        .child(element)
                        .with_animation(id, RESORT.animation(), move |el, t| {
                            el.relative().top(px(dy * (1.0 - t)))
                        })
                        .into_any_element()
                } else if self.sidebar_new_keys.contains(&key) {
                    let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                    motion::fade_quick(id, div().child(element)).into_any_element()
                } else {
                    element
                }
            })
            .collect();
        let archived_section = self.render_archived_section(theme, cx);
        let (user_line, trigger_subline, menu_identity) = {
            let state = self.state.read(cx);
            account_identity(state.keiki_status, state.keiki_session.as_ref())
        };
        let user_menu =
            self.render_user_menu(user_line.clone(), trigger_subline, menu_identity, theme, cx);

        // The space filter lives ABOVE the scroll region (fixed) so its
        // dropdown can float without being clipped by the list's overflow.
        let filter_row = self.render_spaces_filter(theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            .child(filter_row)
            // The (filtered) Sessions list scrolls inside an EdgeFade scope —
            // a true per-glyph gradient at active overflow edges. Glass-safe
            // (no painted overlay can fade content over see-through blur) and
            // equivalent on opaque themes: alpha→0 reveals the surface tone
            // underneath, same as the gradient overlays it replaced. Overflow
            // is read at PAINT time via the scroll handle — render-time gating
            // rode the previous frame's offset, so the last frame of a content
            // shrink (row archived while scrolled) left a phantom fade stuck
            // over an unscrollable list (user report).
            .child(
                crate::edge_fade::edge_faded(
                    SIDEBAR_GLASS_FADE_BAND,
                    true,
                    true,
                    div().relative().flex_1().min_h_0().child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            // No "Sessions" header (user request) — the list
                            // is the whole column; a little air stands in.
                            .pt(px(4.0))
                            .child(if !list_items.is_empty() {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .children(list_items)
                                    .into_any_element()
                            } else {
                                div()
                                    .px(px(Theme::SPACE_SM))
                                    .pb(px(Theme::SPACE_SM))
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from("No sessions yet"))
                                    .into_any_element()
                            })
                            .children(archived_section),
                    ),
                )
                .fade_overflow_y(&self.sidebar_scroll),
            )
            // Local connection state is rendered by the shell when the engine
            // is booting or unavailable.
            .when_some(self.render_connection_pill(theme, cx), |el, pill| {
                el.child(pill)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
    }

    fn open_keiki_agent_dialog(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).keiki_status != crate::keiki::SessionStatus::SignedIn {
            return;
        }
        let name = cx.new(|cx| ComposerInput::new("Agent name", cx));
        let line_number = cx.new(|cx| ComposerInput::new("Optional line number", cx));
        self.keiki_agent_dialog = Some(KeikiAgentDialog {
            templates: Loadable::Loading,
            selected_template: None,
            name,
            line_number,
            error: None,
            focus_pending: true,
            template_task: None,
            create_task: None,
            create_pending: false,
        });
        let state = self.state.downgrade();
        let task = cx.spawn(async move |this, cx| {
            let result = crate::keiki::list_agent_templates(&state, cx).await;
            if let Err(error) = this.update(cx, |shell, cx| {
                let Some(dialog) = shell.keiki_agent_dialog.as_mut() else {
                    return;
                };
                dialog.template_task = None;
                match result {
                    Ok(templates) => {
                        dialog.templates = Loadable::Ready(templates);
                        if let Some(template) = dialog.templates.ready().and_then(|v| v.first()) {
                            dialog.selected_template = Some(0);
                            dialog.name.update(cx, |input, cx| {
                                input.set_text(template.name.clone(), cx);
                            });
                        }
                    }
                    Err(error) => {
                        dialog.templates = Loadable::Error(error.to_string());
                    }
                }
                cx.notify();
            }) {
                tracing::error!("update Keiki template dialog: {error}");
            }
        });
        if let Some(dialog) = self.keiki_agent_dialog.as_mut() {
            dialog.template_task = Some(task);
        }
        cx.notify();
    }

    fn select_keiki_template(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(dialog) = self.keiki_agent_dialog.as_mut() else {
            return;
        };
        let Some(template) = dialog.templates.ready().and_then(|v| v.get(index)).cloned() else {
            return;
        };
        dialog.selected_template = Some(index);
        dialog.name.update(cx, |input, cx| {
            input.set_text(template.name, cx);
        });
        dialog.error = None;
        cx.notify();
    }

    fn submit_keiki_agent(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.keiki_agent_dialog.as_mut() else {
            return;
        };
        if dialog.create_pending {
            return;
        }
        let Some(index) = dialog.selected_template else {
            dialog.error = Some("Select a template to continue.".into());
            cx.notify();
            return;
        };
        let Some(template) = dialog.templates.ready().and_then(|v| v.get(index)) else {
            return;
        };
        let input = CreateAgentFromTemplate {
            template: template.id.clone(),
            name: {
                let value = dialog.name.read(cx).text().trim().to_string();
                (!value.is_empty()).then_some(value)
            },
            line_number: {
                let value = dialog.line_number.read(cx).text().trim().to_string();
                (!value.is_empty()).then_some(value)
            },
        };
        dialog.create_pending = true;
        dialog.error = None;
        let state = self.state.downgrade();
        let task = cx.spawn(async move |this, cx| {
            let result = crate::keiki::create_agent_from_template(&state, input, cx).await;
            let outcome = match result {
                Ok(response) if response.ok => {
                    let refresh = crate::keiki::refresh_keiki_snapshot(state.clone(), cx).await;
                    refresh.map(|()| response)
                }
                Ok(_) => Err(keiki_api::Error::Local(
                    "Keiki did not confirm that the agent was created".into(),
                )),
                Err(error) => Err(error),
            };
            if let Err(error) = this.update(cx, |shell, cx| {
                if shell.keiki_agent_dialog.is_none() {
                    return;
                }
                if let Some(dialog) = shell.keiki_agent_dialog.as_mut() {
                    dialog.create_task = None;
                }
                match outcome {
                    Ok(response) => {
                        let space_id = format!("{}{}", crate::keiki::AGENT_PREFIX, response.id);
                        shell.keiki_agent_dialog = None;
                        if let Err(error) = state.update(cx, |state, cx| {
                            state.select_space(Some(space_id), cx);
                        }) {
                            tracing::error!("select newly created Keiki agent: {error}");
                        }
                        if !response.missing_secrets.is_empty() {
                            let secrets = response
                                .missing_secrets
                                .iter()
                                .map(|secret| secret.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            shell.sidebar_notice = Some(
                                format!(
                                    "Agent created. Add these secrets on onkeiki.com before it can run: {secrets}."
                                )
                                .into(),
                            );
                        }
                    }
                    Err(error) => {
                        if let Some(dialog) = shell.keiki_agent_dialog.as_mut() {
                            dialog.create_pending = false;
                            dialog.error = Some(error.to_string().into());
                        }
                    }
                }
                cx.notify();
            })
            {
                tracing::error!("update Keiki agent creation dialog: {error}");
            }
        });
        dialog.create_task = Some(task);
        cx.notify();
    }

    /// One row per Keiki org the account belongs to (hidden with a single
    /// membership); the active one is marked, the rest re-point the session.
    fn render_org_picker(
        &mut self,
        mut menu: gpui::Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let (orgs, active_org_id, switching) = {
            let state = self.state.read(cx);
            let Some(session) = state.keiki_session.as_ref() else {
                return menu;
            };
            (
                session.switchable_orgs().to_vec(),
                session.active_org_id.clone(),
                state.keiki_org_switch.clone(),
            )
        };
        if orgs.is_empty() {
            return menu;
        }
        menu = menu.child(
            div()
                .px(px(8.0))
                .pt(px(6.0))
                .pb(px(2.0))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from("Organization")),
        );
        for org in orgs {
            let active = active_org_id.as_deref() == Some(org.id.as_str());
            let pending = switching.as_deref() == Some(org.id.as_str());
            let busy = switching.is_some();
            let key = SharedString::from(format!("user-menu-org-{}", org.id));
            let label: SharedString = org.name.clone().unwrap_or_else(|| org.id.clone()).into();
            let org_id = org.id.clone();
            let row = popover::menu_row(theme, active, key.clone())
                .id(key)
                .when(!active && !busy, |row| {
                    row.on_click(cx.listener(move |this, _, _, cx| {
                        crate::keiki::switch_org(this.state.clone(), org_id.clone(), cx).detach();
                        cx.notify();
                    }))
                })
                .when(busy && !pending, |row| row.opacity(0.6))
                .child(
                    div()
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(active, |slot| {
                            slot.child(
                                icon(icons::CHECK)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                        }),
                )
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when(pending, |row| {
                    row.child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Switching…")),
                    )
                });
            menu = menu.child(row);
        }
        menu.child(
            div()
                .mx(px(6.0))
                .my(px(4.0))
                .h(px(1.0))
                .bg(theme.text_muted.opacity(0.15)),
        )
    }

    /// Scope-aware sidebar identity and account menu. Local runtimes advertise
    /// their storage boundary and offer sync; synced runtimes offer sign-out.
    fn render_user_menu(
        &mut self,
        user_line: SharedString,
        trigger_subline: Option<SharedString>,
        menu_identity: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu.is_open();
        let keiki_status = self.state.read(cx).keiki_status;
        let keiki_error = self.state.read(cx).keiki_error.clone();
        let initial: SharedString = user_line
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.user_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.user_menu.take_press_was_open() {
                    this.close_user_menu(cx);
                } else {
                    this.user_menu.open(());
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.bg)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(17.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .truncate()
                            .child(user_line),
                    )
                    .when_some(trigger_subline, |identity, subline| {
                        identity.child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .line_height(px(15.0))
                                .text_color(theme.text_muted)
                                .child(subline),
                        )
                    }),
            );
        if self.user_menu.get().is_some() {
            let closing = self.user_menu.closing_since();
            let mut menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_user_menu(cx)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .truncate()
                        .child(menu_identity),
                );
            if keiki_status == crate::keiki::SessionStatus::SignedIn {
                menu = self.render_org_picker(menu, theme, cx);
                menu = menu.child(
                    popover::menu_row(theme, false, "user-menu-keiki-signout")
                        .id("user-menu-keiki-signout")
                        .on_click(cx.listener(|this, _, _, cx| {
                            crate::keiki::sign_out(this.state.clone(), cx).detach();
                            this.close_user_menu(cx);
                        }))
                        .child(
                            icon(icons::LOGOUT_2)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign out of Keiki")),
                );
            } else {
                let loading = keiki_status == crate::keiki::SessionStatus::Loading;
                let row = popover::menu_row(theme, false, "user-menu-keiki-signin")
                    .id("user-menu-keiki-signin")
                    .when(!loading, |row| {
                        row.on_click(cx.listener(|this, _, _, cx| this.start_keiki_sign_in(cx)))
                    })
                    .when(loading, |row| row.opacity(0.6))
                    .child(
                        icon(icons::GLOBAL)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from(if loading {
                        "Opening Keiki…"
                    } else {
                        "Sign in to Keiki"
                    }));
                menu = menu.child(row).when_some(keiki_error, |menu, error| {
                    menu.child(
                        div()
                            .px(px(8.0))
                            .pb(px(4.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                });
            }
            menu = menu.child(popover::menu_separator()).child(
                popover::menu_row(theme, false, "user-menu-settings")
                    .id("user-menu-settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_settings(SettingsSection::Appearance, cx)
                    }))
                    .child(
                        icon(icons::SETTINGS_MINIMALISTIC)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Settings")),
            );
            trigger = trigger.child(popover::anchored_menu_above(
                "user-menu-popover",
                menu.into_any_element(),
                closing,
            ));
        }
        trigger.into_any_element()
    }

    fn render_keiki_agent_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.keiki_agent_dialog.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            window.focus(&dialog.name.focus_handle(cx), cx);
        }
        let selected = dialog.selected_template;
        let busy = dialog.create_pending;
        let name = dialog.name.clone();
        let line_number = dialog.line_number.clone();
        let templates = dialog.templates.clone();
        let error = dialog.error.clone();
        let template_body: AnyElement = match templates {
            Loadable::Idle | Loadable::Loading => div()
                .py(px(18.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child("Loading Keiki templates…")
                .into_any_element(),
            Loadable::Error(message) => div()
                .py(px(8.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.danger_muted)
                .child(SharedString::from(message))
                .into_any_element(),
            Loadable::Ready(templates) => div()
                .id("keiki-template-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(220.0))
                .overflow_y_scroll()
                .children(templates.into_iter().enumerate().map(|(index, template)| {
                    let is_selected = selected == Some(index);
                    popover::menu_row(&theme, is_selected, format!("keiki-template-{index}"))
                        .id(("keiki-template", index))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_keiki_template(index, cx);
                        }))
                        .child(
                            div()
                                .w(px(28.0))
                                .flex_none()
                                .text_size(crate::typography::ui_rems(18.0))
                                .child(SharedString::from(template.emoji)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(13.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(template.name)),
                                )
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(template.blurb)),
                                ),
                        )
                }))
                .into_any_element(),
        };
        let can_create = !busy
            && dialog.templates.ready().is_some_and(|templates| {
                selected.is_some_and(|index| templates.get(index).is_some())
            });
        let card = popover::dialog_card(&theme)
            .w(px(500.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.keiki_agent_dialog = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, "New Keiki agent"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                "Choose a template, then customize the agent name and line.",
            )))
            .child(div().mt(px(12.0)).child(template_body))
            .child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(popover::dialog_field(name.into_any_element()))
                    .child(popover::dialog_field(line_number.into_any_element())),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .mt(px(10.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(16.0))
                        .text_color(theme.danger_muted)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "keiki-agent-cancel")
                            .id("keiki-agent-cancel")
                            .when(busy, |button| button.opacity(0.5))
                            .when(!busy, |button| {
                                button.on_click(cx.listener(|this, _, _, cx| {
                                    this.keiki_agent_dialog = None;
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        popover::btn_primary(&theme, "Create agent")
                            .id("keiki-agent-create")
                            .when(!can_create, |button| button.opacity(0.45))
                            .when(can_create, |button| {
                                button.on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_keiki_agent(cx);
                                }))
                            }),
                    ),
            )
            .into_any_element();
        Some(popover::modal("keiki-agent-dialog", viewport, card))
    }

    fn render_keiki_chat_action_rows(
        &self,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected_chat = self.state.read(cx).selected_chat.clone();
        if !keiki_menu_is_selected(chat_id, selected_chat.as_deref()) {
            return Vec::new();
        }
        let (keiki_pending, keiki_blocked) = {
            let state = self.state.read(cx);
            let conversation = state.keiki_conversation();
            (
                conversation.and_then(|conversation| conversation.pending),
                conversation.is_some_and(|conversation| conversation.blocked),
            )
        };

        let block_id = chat_id.to_string();
        let unblock_id = chat_id.to_string();
        let mut rows = vec![popover::menu_separator().into_any_element()];
        if !keiki_blocked {
            rows.push(
                popover::menu_row(theme, false, format!("chat-keiki-block-{chat_id}"))
                    .id("chat-keiki-block")
                    .when(
                        keiki_pending == Some(crate::keiki::KeikiConversationPending::Block),
                        |row| row.opacity(0.45),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if keiki_pending != Some(crate::keiki::KeikiConversationPending::Block)
                            && this.state.read(cx).selected_chat.as_deref()
                                == Some(block_id.as_str())
                        {
                            crate::keiki::block(this.state.clone(), cx);
                        }
                    }))
                    .child(SharedString::from("Block"))
                    .into_any_element(),
            );
        } else {
            rows.push(
                popover::menu_row(theme, false, format!("chat-keiki-unblock-{chat_id}"))
                    .id("chat-keiki-unblock")
                    .when(
                        keiki_pending == Some(crate::keiki::KeikiConversationPending::Block),
                        |row| row.opacity(0.45),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if keiki_pending != Some(crate::keiki::KeikiConversationPending::Block)
                            && this.state.read(cx).selected_chat.as_deref()
                                == Some(unblock_id.as_str())
                        {
                            crate::keiki::unblock(this.state.clone(), cx);
                        }
                    }))
                    .child(SharedString::from("Unblock"))
                    .into_any_element(),
            );
        }
        rows
    }

    /// Floating layers owned by the shell: context menus, edit dialogs, and
    /// the local-to-synced account lifecycle.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some(menu_state) = self.chat_menu.get().cloned() {
            let chat_id = menu_state.chat_id;
            let position = menu_state.position;
            let chat_menu_closing = self.chat_menu.closing_since();
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(216.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_chat_menu(cx);
                }))
                .flex()
                .flex_col();
            let menu = match menu_state.page {
                ChatMenuPage::Root => menu
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                            .id("chat-menu-rename")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename_chat(rename_id.clone(), cx)
                            }))
                            .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                            .child(SharedString::from("Rename…")),
                    )
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                            .id("chat-menu-archive")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.archive_chat(archive_id.clone(), cx)
                            }))
                            .child(
                                icon(icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Archive")),
                    )
                    .children(self.render_keiki_chat_action_rows(&chat_id, &theme, cx))
                    .when(crate::keiki::is_keiki_chat(&chat_id), |menu| {
                        let pinned = self
                            .settings
                            .pinned_keiki_conversations
                            .iter()
                            .any(|id| id == &chat_id);
                        let pin_id = chat_id.clone();
                        let view_id = chat_id.clone();
                        menu.child(
                            popover::menu_row(&theme, false, format!("chat-menu-pin-{chat_id}"))
                                .id("chat-menu-pin")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_keiki_conversation_pin(pin_id.clone(), cx);
                                }))
                                .child(
                                    icon(if pinned {
                                        icons::STAR_BOLD
                                    } else {
                                        icons::STAR
                                    })
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(if pinned { "Unpin" } else { "Pin" })),
                        )
                        .child(
                            popover::menu_row(
                                &theme,
                                false,
                                format!("chat-menu-view-conversation-{chat_id}"),
                            )
                            .id("chat-menu-view-conversation")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.view_keiki_conversation(view_id.clone(), cx);
                            }))
                            .child(
                                icon(icons::GLOBAL)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("View conversation")),
                        )
                    })
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-copy-{chat_id}"))
                            .id("chat-menu-copy")
                            .on_click(cx.listener(|this, _, _, cx| this.open_chat_copy_menu(cx)))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(div().flex_1().child(SharedString::from("Copy")))
                            .child(
                                icon(icons::ALT_ARROW_RIGHT)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted.opacity(0.7)),
                            ),
                    )
                    .child(popover::menu_separator())
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                            .id("chat-menu-delete")
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_chat_menu(cx);
                                this.delete_confirm = Some(delete_id.clone());
                                cx.notify();
                            }))
                            .child(
                                icon(icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.danger),
                            )
                            .child(SharedString::from("Delete…")),
                    ),
                ChatMenuPage::Copy => {
                    let chat = self
                        .state
                        .read(cx)
                        .chats
                        .iter()
                        .find(|chat| chat.id == chat_id)
                        .cloned();
                    let session_id = chat
                        .as_ref()
                        .and_then(|chat| chat.harness_session_id.as_deref())
                        .is_some_and(|id| !id.trim().is_empty());
                    let session_chat_id = chat_id.clone();
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-copy-back-{chat_id}"))
                            .id("chat-copy-back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(menu) = this.chat_menu.open_mut() {
                                    menu.page = ChatMenuPage::Root;
                                    cx.notify();
                                }
                            }))
                            .child(
                                icon(icons::ALT_ARROW_LEFT)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Back")),
                    )
                    .child(popover::menu_separator())
                    .when(session_id, |menu| {
                        menu.child(
                            popover::menu_row(
                                &theme,
                                false,
                                format!("chat-copy-session-{chat_id}"),
                            )
                            .id("chat-copy-session")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_harness_session_id(&session_chat_id, cx)
                            }))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Harness session ID")),
                        )
                    })
                }
            }
            .into_any_element();
            overlays.push(popover::menu_at(
                "chat-context-menu",
                position,
                menu,
                chat_menu_closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_keiki_agent_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let theme = Theme::of(cx);
        let fade_key = format!("pane-resize-{id}");
        let highlight = motion::hover_blend(
            &fade_key,
            theme.border_strong.opacity(0.0),
            theme.border_strong,
        );
        let clear = highlight.opacity(0.0);
        div()
            .id(id)
            .absolute()
            .top(px(PANE_RESIZE_HITBOX_TOP))
            .bottom_0()
            .w(px(12.0))
            .flex_none()
            .cursor_col_resize()
            .on_hover(motion::hover_listener(fade_key))
            // Codex-style seam feedback: the existing 1px panel border stays
            // visible at rest; hover adds a stronger center highlight that
            // fades back into that border toward both ends.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(6.0))
                    .w(px(1.0))
                    .flex()
                    .flex_col()
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(clear, 0.0),
                        gpui::linear_color_stop(highlight, 1.0),
                    )))
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(highlight, 0.0),
                        gpui::linear_color_stop(clear, 1.0),
                    ))),
            )
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text) = (theme.border, theme.text);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar). Settings never
        // underlaps: pad below the overlaid titlebar.
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();

        // Content outlet: selected chat → transcript; nothing selected →
        // centered empty-state content.
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else {
            let variant = empty_state_variant(self.state.read(cx).keiki_status);
            let mut content = div().flex().items_center().justify_center();
            if let Some(action_label) = empty_state_action_label(variant) {
                let loading = variant == EmptyStateVariant::Loading;
                content = content.child(
                    div()
                        .id("no-selection-action")
                        .w(px(240.0))
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(crate::typography::ui_rems(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .when(!loading, |button| {
                            button.cursor_pointer().hover(|s| s.opacity(0.9)).on_click(
                                cx.listener(|this, _, _, cx| this.start_keiki_sign_in(cx)),
                            )
                        })
                        .child(SharedString::from(action_label)),
                );
            } else {
                content = content.child(
                    icon(icons::ZERON_LOGO)
                        .w(px(41.9))
                        .h(px(48.0))
                        .text_color(theme.text.opacity(0.09)),
                );
            }
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in("no-selection-canvas", content))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the selected conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil. GPUI derives the
        // veil's visibility from the active payload type: an internal drag
        // such as a pane resize must never resurrect stale external-file state.
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The terminal dock is NOT glass the transcript may slide
                    // under: with the dock's translucent fill, transcript text
                    // ghosted through the grid (user report). The underlay
                    // ends at the dock's top instead, riding the same height
                    // tween the dock animates with; `stack_h` below is only
                    // the chrome that still overlaps the transcript (status
                    // strip + composer).
                    let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                    let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .bottom(px(term_h))
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                true,
                                true,
                                div().size_full().child(outlet),
                            )
                            // Fully faded BY the titlebar's bottom edge (the
                            // title text is opaque — overlap read as collision),
                            // ramping in the band just below it.
                            .inset_top(Theme::TITLEBAR_HEIGHT)
                            .band_top(Theme::TRANSCRIPT_FADE_BAND)
                            .band_bottom(bottom_band),
                        )
                        .children(self.render_jump_to_bottom(stack_h, cx))
                },
            )
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts), composer, terminal dock. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(status)
                    .when(has_selection, |el| el.child(self.composer.clone()))
                    .child(self.render_terminal_container(cx))
            })
            .when(has_selection, |element| {
                element.child(
                    div()
                        .invisible()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child("Drop images to attach")
                        .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.visible())
                        .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                            let paths = paths.paths().to_vec();
                            this.composer
                                .update(cx, |composer, cx| composer.add_paths(paths, cx));
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    /// `stack_h` is the measured bottom chrome stack the full-height
    /// transcript scrolls under — the pill anchors just above it (the -14
    /// carries the old status-strip overlap).
    fn render_jump_to_bottom(
        &mut self,
        stack_h: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom(px(stack_h - 14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(self.jump_pill("jump-to-bottom", "jump-pill", self.transcript.clone(), cx))
                .into_any_element(),
        )
    }

    /// The jump pill itself — shared between the conversation overlay and
    /// the subagent pane so both read as one control. `anim_key`/`hover_key`
    /// must be distinct per instance (they key global animation state).
    ///
    /// Glass-forward like the composer pill it floats near: a backdrop blur
    /// under the floating-card tint ([`Theme::glass_overlay`]), hover
    /// brightening via the standard glass wash painted OVER the tint —
    /// mixing the tint TOWARD the wash would thin the pill on hover, the
    /// exact see-through regression the old opaque pill's comment warned
    /// about. Opaque appearances keep the raised-surface treatment
    /// (`frosted` passes through there anyway).
    fn jump_pill(
        &self,
        anim_key: &'static str,
        hover_key: &'static str,
        transcript: Entity<Transcript>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx);
        let glass = theme.is_glass();
        let base = if glass {
            theme.glass_overlay()
        } else {
            motion::hover_blend(hover_key, theme.surface_raised, theme.surface_raised_hover)
        };
        let wash = if glass {
            motion::hover_blend(hover_key, gpui::transparent_black(), theme.glass_hover())
        } else {
            gpui::transparent_black()
        };
        let pill = div()
            .id(anim_key)
            .h(px(30.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .cursor_pointer()
            .bg(base)
            .on_hover(motion::hover_listener(hover_key))
            .on_click(cx.listener(move |_, _, _, cx| {
                transcript.update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
            }))
            .child(
                // The hover wash rides an inner full-height layer so it
                // composites over the tint (a div has one bg).
                div()
                    .h_full()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(11.0))
                    .pr(px(13.0))
                    .bg(wash)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("↓")),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Scroll to bottom")),
                    ),
            );
        // Frost OUTSIDE the entry animation (the composer pill's exact
        // composition): one scene layer — blur, then the pill's quads, then
        // glyphs — so the pill always composes over the transcript content
        // scrolling under it, and never loses its washes to the kind-sorted
        // draw order (frost.rs module docs).
        crate::frost::frosted(15.0, 16.0, motion::dialog_in(anim_key, pill)).into_any_element()
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its 5px read as
        // dead air between the seam and the tab bar (user report).
        let inner = div()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(panel))
            .child(handle.absolute().top_0().left_0().right_0());

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (zeron's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(crate::typography::ui_rems(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::gradient_spinner(
                    "sending-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right pane — the surface host (t3code RightPanelTabs): hidden by
    /// default, drag-resizable. Content is the ACTIVE surface — the Diff
    /// page (its options row + the lazy [`Changes`] viewer), an embedded
    /// terminal, or the surface picker when no tabs exist.
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            match self.resolved_right_active(cx) {
                RightSurface::Diff(id) if self.diffs.contains_key(&id) => {
                    let changes = self.diffs.get(&id).cloned().expect("checked");
                    // Idempotent — also covers a persisted-open pane on boot.
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                    // The diff options (scope dropdown, ref selector,
                    // fold-all) moved DOWN from the titlebar band — the
                    // surface tabs own that row now; the expand/close
                    // buttons stayed up there (user request).
                    let controls =
                        changes.update(cx, |changes, cx| changes.render_header_controls(cx));
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(px(36.0))
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(controls),
                        )
                        .child(div().flex_1().min_h_0().child(changes))
                        .into_any_element()
                }
                RightSurface::Terminal(tab) => {
                    let panel = self.right_terminal_panel(cx);
                    // Keep the embedded panel's own active tab aligned with
                    // the resolved surface (fallbacks can move it).
                    let resize_suspended = self.tween_active(self.right_tween);
                    panel.update(cx, |panel, cx| {
                        panel.set_resize_suspended(resize_suspended);
                        panel.select_tab_by_key(tab, cx);
                    });
                    panel.into_any_element()
                }
                RightSurface::Subagent(id) if self.subagent_tabs.contains_key(&id) => {
                    let transcript = self
                        .subagent_tabs
                        .get(&id)
                        .expect("checked")
                        .transcript
                        .clone();
                    // The pane hosts its own jump pill: the conversation
                    // overlay's is bound to the PRIMARY transcript, and this
                    // one anchors to the pane (no composer stack to clear).
                    let pill = transcript.read(cx).jump_button_shown().then(|| {
                        div()
                            .absolute()
                            .bottom(px(16.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(self.jump_pill(
                                "subagent-jump-to-bottom",
                                "subagent-jump-pill",
                                transcript.clone(),
                                cx,
                            ))
                    });
                    // Read-only surface: the transcript fills the pane — no
                    // composer, no status strip.
                    div()
                        .size_full()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(div().flex_1().min_h_0().child(transcript))
                        .children(pill)
                        .into_any_element()
                }
                _ => self.render_surface_picker(cx),
            }
        } else {
            gpui::Empty.into_any_element()
        };
        // Flush panel (user request — the inset card is gone): full window
        // height with a left hairline, glass-friendly like the terminal dock
        // (translucent over the frost; solid otherwise). The resize grabber
        // lives outside this clipped container, on the root layout's seam.
        let panel_bg = if theme.is_glass() {
            bg.opacity(0.4)
        } else {
            bg
        };
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            // In takeover the panel's left edge IS the sidebar seam, which
            // already carries the sidebar tone's right hairline — a second
            // border there doubled up (user report).
            .when(!self.right_pane_expanded, |el| {
                el.border_l_1().border_color(theme.border)
            })
            .bg(panel_bg)
            .overflow_hidden()
            // The titlebar is a glass overlay over the full-height content
            // row; the panel's own chrome starts below it.
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .child(content);
        let target = self.right_target(cx);
        self.right_pane_container(
            self.right_tween,
            target,
            div().h_full().relative().child(panel).into_any_element(),
        )
    }

    /// The right pane's empty state: a compact vertical list of surface rows
    /// (icon + label). The old two-card grid clipped in narrow panes and
    /// wasted short ones.
    fn render_surface_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let text = theme.text;
        let muted = theme.text_muted;
        let border = theme.border;
        let border_strong = theme.border_strong;
        let row = |id: &'static str, icon_path: &'static str, title: &'static str| {
            div()
                .id(id)
                .w_full()
                .h(px(44.0))
                .px(px(14.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(border)
                .bg(crate::theme::ink(0.02))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(move |s| s.bg(crate::theme::ink(0.05)).border_color(border_strong))
                .child(icon(icon_path).size(px(15.0)).flex_none().text_color(muted))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text)
                        .child(SharedString::from(title)),
                )
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(280.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        row("surface-card-terminal", icons::TERMINAL, "Terminal").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.add_terminal_surface(cx);
                            }),
                        ),
                    )
                    // Git only where there IS git — the pane itself no
                    // longer gates on it (terminals work anywhere).
                    .when(self.space_git_detected(cx), |el| {
                        el.child(row("surface-card-git", icons::GIT_BRANCH, "Git").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.add_diff_surface(cx);
                            }),
                        ))
                    }),
            )
            .into_any_element()
    }

    fn close_right_plus(&mut self, cx: &mut Context<Self>) {
        if self.right_plus.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.right_plus);
        }
        cx.notify();
    }

    /// The titlebar strip over the right pane: one chip per surface tab
    /// (icon · title · ✕) plus the `+` menu — the t3code RightPanelTabs bar,
    /// living in the top row; the diff options moved into the pane below.
    pub(crate) fn render_right_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        /// Fixed chip slot — the terminal drawer's drag mechanics (drop-index
        /// quantisation + slide offsets) assume uniform widths.
        const CHIP_W: f32 = 112.0;
        const CHIP_SLOT: f32 = CHIP_W + 4.0; // + the strip's own gap

        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the strip.
        if self.right_tab_drag.is_some() && !cx.has_active_drag() {
            self.right_tab_drag = None;
        }
        let rows = self.right_surface_rows(cx);
        let count = rows.len();
        let active = self.resolved_right_active(cx);
        let drag = self
            .right_tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        // Fade flags from the LAST frame's scroll state (invisible lag).
        // The EdgeFade scope below fades per-pixel on x for glyphs AND
        // quads/images (fork 5d1f83d) — washes dissolve across the band.
        const FADE_WIDTH: f32 = 36.0;
        let scrolled = -f32::from(self.right_tab_scroll.offset().x);
        let max_scroll = f32::from(self.right_tab_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        // The old session-tab strip's proven scroll shape: the flex row IS
        // the scroller (id + overflow_x_scroll + track_scroll), wrapped in a
        // relative min_w_0 region below; drop math runs in CONTENT
        // coordinates (viewport-relative x plus the scrolled-off width).
        let scroll_for_drag = self.right_tab_scroll.clone();
        let mut strip = div()
            .id("right-surface-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.right_tab_scroll)
            .on_drag_move::<RightTabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<RightTabDrag>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.panel_key != this.panel_key(cx) {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x)
                        - f32::from(event.bounds.left())
                        - f32::from(scroll_for_drag.offset().x);
                    let over = crate::terminal::panel::drop_index(rel_x, CHIP_SLOT, count);
                    this.update_right_tab_drag_over(from, over, cx);
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(move |this, payload: &RightTabDrag, _, cx| {
                if payload.panel_key != this.panel_key(cx) {
                    this.right_tab_drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .right_tab_drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from);
                this.right_tab_drag = None;
                this.reorder_right_tabs(payload.from, to, cx);
            }));
        for (ix, (surface, title)) in rows.into_iter().enumerate() {
            let is_active = surface == active;
            let icon_path = match surface {
                RightSurface::Diff(_) => icons::GIT_BRANCH,
                RightSurface::Subagent(_) => icons::BOT,
                _ => icons::TERMINAL,
            };
            // A live subagent tab swaps its icon for the mini working
            // spinner (the history fetch button's in-flight recipe) — the
            // doc's streaming tail entry IS the run's liveness, so the swap
            // settles by itself when the subagent finishes.
            let subagent_running = match surface {
                RightSurface::Subagent(id) => self.subagent_tabs.get(&id).is_some_and(|tab| {
                    self.state
                        .read(cx)
                        .sub_transcript(&tab.doc_id)
                        .last()
                        .is_some_and(|e| e.status == Some(zeron_doc::MessageStatus::Streaming))
                }),
                _ => false,
            };
            // t3 tab hover: the surface icon swaps IN PLACE for the close ✕
            // (same slot, no width jump) — the ✕ only shows while the tab is
            // hovered (user request).
            let group: SharedString = format!("right-surface-tab-{ix}").into();
            let ghost_title = title.clone();
            let chip = div()
                .id(("right-surface-tab", ix))
                .group(group.clone())
                .h(px(24.0))
                .w(px(CHIP_W))
                .flex_none()
                .pl(px(4.0))
                .pr(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .cursor_pointer()
                // The old session-tab strip's solved carve-out: NOT
                // `.occlude()` — a BlockMouse hitbox ends the hit test,
                // so the scroll container behind the tabs never saw
                // wheel events and an overflowing strip could not be
                // scrolled (tabs tile the whole region). ExceptScroll
                // keeps the titlebar drag-region carve-out and lets the
                // strip scroll.
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .when(is_active, |el| el.bg(crate::theme::wash(0.10)))
                .when(!is_active, |el| {
                    el.hover(|s| s.bg(crate::theme::wash(0.06)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_right_active(surface, cx);
                }))
                // Middle-click closes, like every tab strip.
                .on_mouse_down(
                    gpui::MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        this.close_right_surface(surface, window, cx);
                    }),
                )
                .on_drag(
                    RightTabDrag {
                        panel_key: self.panel_key(cx),
                        from: ix,
                        title: ghost_title,
                    },
                    |payload, _point, _, cx| {
                        let title = payload.title.clone();
                        cx.stop_propagation();
                        cx.new(|_| SurfaceTabGhost { title })
                    },
                )
                .child(
                    // Leading slot: icon normally, ✕ on tab hover — two
                    // stacked layers opacity-swapped by the group hover.
                    div()
                        .id(("right-surface-close", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .relative()
                        .hover(|s| s.bg(crate::theme::wash(0.12)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_right_surface(surface, window, cx);
                        }))
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .group_hover(group.clone(), |s| s.opacity(0.0))
                                .child(if subagent_running {
                                    loaders::mini_glyph_spinner(
                                        format!("subagent-tab-{ix}"),
                                        2.0,
                                        theme.glyph,
                                        cx.entity_id(),
                                        cx,
                                    )
                                    .into_any_element()
                                } else {
                                    icon(icon_path)
                                        .size(px(12.0))
                                        .text_color(if is_active {
                                            theme.text_muted
                                        } else {
                                            theme.text_muted.opacity(0.7)
                                        })
                                        .into_any_element()
                                }),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .opacity(0.0)
                                .group_hover(group.clone(), |s| s.opacity(1.0))
                                .child(
                                    icon(icons::CLOSE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(if is_active {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(title),
                );
            // Sliding transform while a sibling drags over (the terminal
            // drawer's exact recipe): animate 150ms between committed
            // offsets; the dragged tab leaves an invisible spacer — the
            // ghost carries it.
            let wrapped: AnyElement = match drag {
                Some((from, over, epoch, prev_over)) if ix != from => {
                    let target = crate::terminal::panel::slide_offset(ix, from, over) * CHIP_SLOT;
                    let start =
                        crate::terminal::panel::slide_offset(ix, from, prev_over) * CHIP_SLOT;
                    div()
                        .relative()
                        .child(chip.with_animation(
                            ("right-tab-slide", (ix as u64) | ((epoch as u64) << 32)),
                            TAB_SLIDE.animation(),
                            move |el, t| el.left(px(motion::lerp(start, target, t))),
                        ))
                        .into_any_element()
                }
                Some((from, ..)) if ix == from => div()
                    .w(px(CHIP_W))
                    .h(px(24.0))
                    .flex_none()
                    .into_any_element(),
                _ => chip.into_any_element(),
            };
            strip = strip.child(wrapped);
        }
        // The `+` — a small menu offering the two surfaces (t3 "Add panel
        // surface"); mirrors the picker cards.
        let plus_open = self.right_plus.get().is_some();
        let plus_fade = "right-surface-add-fade";
        let mut plus = div()
            .id("right-surface-add")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                plus_fade,
                crate::theme::wash(0.0),
                crate::theme::wash(0.11),
            ))
            .on_hover(motion::hover_listener(plus_fade))
            .block_mouse_except_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.right_plus.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.right_plus.take_press_was_open() {
                    this.close_right_plus(cx);
                } else {
                    this.right_plus.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        if plus_open {
            let closing = self.right_plus.closing_since();
            let menu = popover::popover_card(&theme)
                .w(px(168.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_right_plus(cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            popover::menu_row(&theme, false, "right-plus-terminal")
                                .id("right-plus-terminal-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_terminal_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::TERMINAL)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Terminal")),
                        )
                        .when(self.space_git_detected(cx), |menu| {
                            menu.child(
                                popover::menu_row(&theme, false, "right-plus-diff")
                                    .id("right-plus-diff-row")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.add_diff_surface(cx);
                                        this.close_right_plus(cx);
                                    }))
                                    .child(
                                        icon(icons::GIT_BRANCH)
                                            .size(px(13.0))
                                            .text_color(theme.text_muted),
                                    )
                                    // "Git", not "Git diff" — the surface hosts
                                    // history and per-commit views too (user
                                    // request; matches the picker card).
                                    .child(SharedString::from("Git")),
                            )
                        }),
                )
                .into_any_element();
            plus = plus.relative().child(popover::anchored_menu_below_gap(
                "right-plus-menu",
                menu,
                closing,
                10.0,
            ));
        }
        // The empty-state picker already offers every surface. Show a single
        // Chrome-style add-tab affordance only after at least one tab exists.
        strip = strip.when(count > 0, |strip| strip.child(plus));
        // Edge fades on whichever side hides tabs (flags computed above).
        // Glass: per-glyph EdgeFade scope over the chips' own opacity ramps;
        // opaque: painted gradients in the shell surface tone.
        let glass = theme.is_glass();
        let bar_bg = theme.surface;
        let region = div()
            .relative()
            .min_w_0()
            .size_full()
            .flex()
            .items_center()
            .child(strip)
            .when(fade_left && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });
        if glass {
            crate::edge_fade::edge_faded(FADE_WIDTH, false, false, region)
                .fade_left(fade_left)
                .fade_right(fade_right)
                .into_any_element()
        } else {
            region.into_any_element()
        }
    }

    /// Toggle the changes-panel takeover (the header's expand button, t3code
    /// parity): the panel grows to fill everything right of the sidebar,
    /// hiding the conversation column; toggling back restores the saved
    /// width. Rides the same width tween as open/close so the jump glides.
    fn toggle_right_pane_expand(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let from_main = conversation_width(self.viewport_width, sidebar_now, from);
        self.right_pane_expanded = !self.right_pane_expanded;
        let to = self.right_target(cx);
        let right_transition = WidthTween::new(from, to);
        self.right_tween = Some(right_transition);
        self.right_takeover_content_tween = Some(right_transition);
        self.main_takeover_tween = Some(WidthTween::new(
            from_main,
            conversation_width(self.viewport_width, sidebar_now, to),
        ));
        cx.notify();
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let keiki_status = self.state.read(cx).keiki_status;
        let keiki_error = self.state.read(cx).keiki_error.clone();
        let keiki_loading = matches!(keiki_status, crate::keiki::SessionStatus::Loading);
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (zeron Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(14.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card (Keiki Gate): centered card on the grid —
            // logo, "Sign in to Keiki", copy, full-width Keiki button.
            _ => div()
                .w(px(360.0))
                .px(px(32.0))
                .py(px(40.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_card)
                .shadow_lg()
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(
                    icon(icons::ZERON_LOGO)
                        .w(px(31.4))
                        .h(px(36.0))
                        .text_color(theme.text),
                )
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(crate::typography::ui_rems(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from("Sign in to Keiki")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens Keiki in your browser to finish signing in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("keiki-sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(crate::typography::ui_rems(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .when(!keiki_loading, |button| {
                            button
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.9))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_keiki_sign_in(cx)
                                }))
                        })
                        .child(SharedString::from(if keiki_loading {
                            "Opening Keiki…"
                        } else {
                            "Sign in to Keiki"
                        })),
                )
                .when_some(keiki_error, |card, error| {
                    card.child(
                        div()
                            .mt(px(10.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.danger)
                            .child(SharedString::from(error)),
                    )
                })
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (zeron App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (zeron styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (zeron window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // zeron window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

/// Right padding for titlebar content: past the native Windows caption
/// cluster, or past zeron's own Linux caption buttons (10px edge inset +
/// the button row) when the layout puts any on the right.
fn titlebar_right_padding(is_windows: bool, linux_right_captions: usize, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else if linux_right_captions > 0 {
        10.0 + caption_buttons_width(linux_right_captions)
    } else {
        0.0
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A Linux caption button in zeron's own cluster style (24px, rounded-6,
/// 16px linear icon). gpui's `WindowControlArea` hit-testing is inert on
/// Linux, so unlike the Windows cluster these carry explicit click handlers
/// (`minimize_window` / `zoom_window` / `remove_window`), the same calls
/// zed's Linux titlebar makes. `occlude` + `prevent_default` keep them out
/// of the drag strip's event surface (see [`window_control_button`]).
fn linux_caption_button(
    id: &'static str,
    icon_path: &'static str,
    close: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (muted, hover_bg, hover_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (theme.text_muted, red, gpui::white())
    } else {
        (theme.text_muted, theme.glass_hover(), theme.text)
    };
    div()
        .id(id)
        // gpui svgs don't inherit the div's text color — recolor the glyph
        // on hover through the group instead (zed's WindowControl idiom).
        .group("linux-caption-button")
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(
            icon(icon_path)
                .size(px(16.0))
                .text_color(muted)
                .group_hover("linux-caption-button", move |style| {
                    style.text_color(hover_fg)
                }),
        )
}

/// A titlebar history button (zeron window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (zeron __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // zeron __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport_width = f32::from(window.viewport_size().width);
        // Appearance actions persist independently of the shell. Mirror the
        // globals before any later debounced settings save can overwrite them.
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        avatars::flush_evicted(Some(window), cx);
        let theme = Theme::of(cx);
        // The shell tone (zeron `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Linux CSD: (re-)resolve which caption buttons we draw and on which
        // side — decorations can flip server↔client at runtime and the
        // desktop's button layout is user configuration.
        self.linux_captions = Self::resolve_linux_captions(window, cx);
        if cfg!(target_os = "linux") && self.button_layout_sub.is_none() {
            self.button_layout_sub =
                Some(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        if self.activation_sub.is_none() {
            self.activation_sub = Some(cx.observe_window_activation(
                window,
                |this: &mut Shell, window, cx| {
                    if !window.is_window_active() {
                        this.set_jump_hints(false, cx);
                    }
                },
            ));
        }

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat)
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(crate::typography::ui_rems(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (zeron __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            // New session works from anywhere — `open_new_session` routes back
            // to chat itself, so Settings is not a dead spot.
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(cx)))
            // Native Settings menu item and the platform convention (Cmd+, on
            // macOS, Ctrl+, elsewhere) always land on the default section.
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(SettingsSection::Appearance, cx)
            }))
            // Chat-scoped; `cycle_session` holds the guard
            // and says why.
            .on_action(cx.listener(|this, _: &NextSession, _, cx| this.cycle_session(true, cx)))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| this.cycle_session(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            // Chat-scoped like the panel toggles: Settings has no current
            // session to archive. Quiet under an open popover, like the other
            // session-nav shortcuts.
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if matches!(this.route, Route::Chat) && !this.overlay_owns_keyboard(cx) {
                    this.archive_selected_chat(cx)
                }
            }))
            // A jump routes back to chat itself, so Settings is not a dead
            // spot — the same call a click on that sidebar row makes. An open
            // picker/palette owns the keyboard: no jumping underneath it.
            .on_action(cx.listener(|this, jump: &JumpSession, _, cx| {
                if !this.overlay_owns_keyboard(cx) {
                    this.jump_to_session(jump.0, cx)
                }
            }))
            .on_modifiers_changed(
                cx.listener(|this, event, _, cx| this.on_modifiers_changed(event, cx)),
            )
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewKeikiAgent, _, cx| {
                this.open_keiki_agent_dialog(cx);
            }));

        let root = match &gate {
            GatePhase::Ready => {
                let window_active = window.is_window_active();
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                // Stamped for `right_target` — the expanded changes panel
                // sizes itself to the viewport.
                self.viewport_width = viewport;
                let main_target_width =
                    conversation_width(viewport, self.sidebar_target(), self.right_target(cx));
                let main_transition = self.active_tween_endpoints(self.main_takeover_tween);
                let main_content_width =
                    stable_panel_content_width(main_target_width, main_transition);
                let main_width = (main_content_width - 10.0).max(0.0);
                self.composer.update(cx, |composer, cx| {
                    composer.set_available_width(main_width, cx)
                });
                // Clearance excludes the terminal dock: the transcript
                // viewport ends at the dock's top (see the underlay in
                // `render_main`), so only the chrome above it overlaps.
                let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx);
                    t.set_bottom_clearance(stack_h, cx);
                });

                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (zeron __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                let right_open = on_chat && self.right_pane_open(cx);
                // Takeover mode derives its width from the viewport, so a
                // manual drag handle would fight the expanded target.
                let right_handle = (right_open
                    && !self.right_pane_expanded
                    && !self.tween_active(self.right_tween))
                .then(|| {
                    self.resize_handle(
                        "right-pane-resize",
                        || RightPaneResize,
                        |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                        cx,
                    )
                    // A forgiving transparent hit target centered on the
                    // seam; the panel's 1px border remains the visual divider.
                    .left(px(-6.0))
                });
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // Copied out (not held) — `render_title_bar` needs `cx` mutable.
                let border_color = Theme::of(cx).border;
                // No inset cards (user request): the conversation column sits
                // flush and unbordered, the transcript directly on the frost
                // glass; the changes pane is a flush left-bordered glass panel
                // (built inside `render_right_pane`).
                let main = if main_transition.is_some() {
                    div()
                        .h_full()
                        .w(px(main_content_width))
                        .flex_none()
                        .flex()
                        .child(main)
                        .into_any_element()
                } else {
                    main
                };
                let card: AnyElement = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .child(main)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (zeron
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.left(px(-6.0)));
                // Keep the right resize target outside the pane's
                // overflow-hidden width container. This mirrors the sidebar
                // seam and lets the target straddle both adjacent panes.
                let right_seam: AnyElement = if let Some(handle) = right_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                // The content row spans the FULL window height — the titlebar
                // overlays it (glass, no fill), so the transcript can scroll
                // under the header and fade out at its edge. Columns that
                // must NOT underlap (sidebar content, the changes panel,
                // settings) pad themselves down by the titlebar height.
                let page = div()
                    .size_full()
                    .relative()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(card)
                            .child(right_seam)
                            .child(right),
                    )
                    .child(div().absolute().top_0().left_0().right_0().child(title_bar))
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            phase @ GatePhase::Failed(_) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };
        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, false, cx.entity_id(), cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, true, cx.entity_id(), cx))
            }
            SplashPhase::Gone => root,
        };

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above the splash and every auth/org/error gate as well as
        // the full application. Gate pages also need a drag surface because
        // they do not render the unified tabs/settings titlebar — on Windows
        // the native `Drag` control area, on Linux the explicit
        // `start_window_move` strip (the control-area hit-test is inert
        // there); macOS drags gate windows natively.
        let root = if matches!(gate, GatePhase::Ready) || cfg!(target_os = "macos") {
            root
        } else {
            root.child(
                self.titlebar_drag_region(
                    "gate-titlebar-drag",
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(Theme::TITLEBAR_HEIGHT)),
                    cx,
                ),
            )
        };
        root.children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_identity_signed_out() {
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::SignedOut, None),
            (
                "Not signed in".into(),
                Some("Keiki".into()),
                "Keiki account".into()
            )
        );
    }

    #[test]
    fn account_identity_loading() {
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::Loading, None),
            ("Signing in…".into(), None, "Keiki account".into())
        );
    }

    #[test]
    fn account_identity_signed_in_with_name_and_org() {
        let session = KeikiSessionInfo {
            display_name: Some("Ada Lovelace".into()),
            email: Some("ada@example.com".into()),
            active_org_name: Some("Analytical Engines".into()),
            role: Some("owner".into()),
            ..KeikiSessionInfo::default()
        };
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::SignedIn, Some(&session)),
            (
                "Ada Lovelace".into(),
                Some("Analytical Engines".into()),
                "ada@example.com".into()
            )
        );
    }

    #[test]
    fn account_identity_signed_in_with_email_only() {
        let session = KeikiSessionInfo {
            display_name: None,
            email: Some("ada@example.com".into()),
            active_org_name: None,
            role: None,
            ..KeikiSessionInfo::default()
        };
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::SignedIn, Some(&session)),
            (
                "ada@example.com".into(),
                Some("ada@example.com".into()),
                "ada@example.com".into()
            )
        );
    }

    #[test]
    fn account_identity_signed_in_without_organization() {
        let session = KeikiSessionInfo {
            display_name: Some("Ada Lovelace".into()),
            email: Some("ada@example.com".into()),
            active_org_name: None,
            role: Some("owner".into()),
            ..KeikiSessionInfo::default()
        };
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::SignedIn, Some(&session)),
            (
                "Ada Lovelace".into(),
                Some("ada@example.com".into()),
                "ada@example.com".into()
            )
        );
    }

    #[test]
    fn account_identity_signed_in_before_session_info_arrives() {
        assert_eq!(
            account_identity(crate::keiki::SessionStatus::SignedIn, None),
            ("Keiki account".into(), None, "Keiki account".into())
        );
    }

    fn keiki_chat(id: &str) -> zeron_proto::Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "device",
            "archived": false,
            "createdAt": "2024-01-01T00:00:00Z",
        }))
        .expect("test chat")
    }

    #[test]
    fn prune_pinned_keiki_conversations_drops_stale_ids() {
        let mut pinned = vec![
            "keiki-conv:agent:stale".to_string(),
            "keiki-conv:agent:live".to_string(),
        ];
        let chats = vec![keiki_chat("keiki-conv:agent:live")];

        prune_pinned_keiki_conversations(&mut pinned, &chats);

        assert_eq!(pinned, vec!["keiki-conv:agent:live"]);
    }

    #[test]
    fn prune_pinned_keiki_conversations_keeps_live_ids() {
        let mut pinned = vec!["keiki-conv:agent:live".to_string()];
        let chats = vec![keiki_chat("keiki-conv:agent:live")];

        prune_pinned_keiki_conversations(&mut pinned, &chats);

        assert_eq!(pinned, vec!["keiki-conv:agent:live"]);
    }

    #[test]
    fn prune_pinned_keiki_conversations_skips_empty_keiki_snapshot() {
        let mut pinned = vec![
            "keiki-conv:agent:live".to_string(),
            "keiki-conv:other:also-live".to_string(),
        ];
        let chats = vec![keiki_chat("zeron-chat")];

        prune_pinned_keiki_conversations(&mut pinned, &chats);

        assert_eq!(
            pinned,
            vec![
                "keiki-conv:agent:live".to_string(),
                "keiki-conv:other:also-live".to_string()
            ]
        );
    }

    #[test]
    fn every_default_shortcut_binds_on_this_platform() {
        // `apply_keymap` silently falls back on an unparseable combo, so a
        // default gpui cannot parse would ship as a dead shortcut.
        for id in crate::settings::ShortcutId::ALL {
            let combo = platform_combo(id.default_combo());
            assert!(
                Keystroke::parse(&combo).is_ok(),
                "{} default {combo:?} does not parse",
                id.label()
            );
        }
    }

    #[test]
    fn right_pane_ceiling_preserves_the_chat_floor() {
        assert_eq!(right_pane_max_width(1200.0, 256.0), 644.0);
        assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
        // The chat floor wins over the right pane's preferred 360px minimum
        // when the whole window is unusually narrow.
        assert_eq!(right_pane_max_width(800.0, 256.0), 244.0);
        assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
    }

    #[test]
    fn right_pane_takeover_consumes_the_chat_column() {
        assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
        assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
    }

    #[test]
    fn right_pane_takeover_control_reverses_direction() {
        assert_eq!(tabs::right_pane_expand_icon(false), icons::EXPAND_ARROWS);
        assert_eq!(tabs::right_pane_expand_icon(true), icons::COLLAPSE_ARROWS);
    }

    #[test]
    fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
        assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
    }

    #[test]
    fn new_session_action_lives_in_the_titlebar_on_the_chat_route() {
        assert_eq!(titlebar_new_session_alpha(true), 1.0);
        assert_eq!(titlebar_new_session_alpha(false), 0.0);
    }

    #[test]
    fn right_panel_content_keeps_the_larger_width_only_during_transition() {
        assert_eq!(right_panel_content_width(520.0, None, None), 520.0);
        assert_eq!(
            right_panel_content_width(0.0, Some((520.0, 0.0)), None),
            520.0
        );
        assert_eq!(
            right_panel_content_width(760.0, Some((520.0, 760.0)), None),
            760.0
        );
        assert_eq!(
            right_panel_content_width(1064.0, Some((520.0, 1064.0)), Some(760.0)),
            760.0
        );

        let conversation = conversation_width(1320.0, 256.0, 520.0);
        let takeover = conversation_width(1320.0, 256.0, 1064.0);
        assert_eq!(conversation, 544.0);
        assert_eq!(takeover, 0.0);
        assert_eq!(
            stable_panel_content_width(takeover, Some((conversation, takeover))),
            conversation
        );
        assert_eq!(
            stable_panel_content_width(conversation, Some((takeover, conversation))),
            conversation
        );
    }

    #[test]
    fn titlebar_cluster_matches_zeron_window_controls() {
        // zeron window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
        assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
        assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
        assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
        assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
        assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
        assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
        assert_eq!(
            TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
            titlebar_cluster_start(false),
            "the rendered row padding and spacer must land on the declared cluster start"
        );
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 0, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
    }

    #[test]
    fn linux_caption_controls_reserve_titlebar_space() {
        // 24px buttons on the cluster's 2px rhythm.
        assert_eq!(caption_buttons_width(0), 0.0);
        assert_eq!(caption_buttons_width(1), 24.0);
        assert_eq!(caption_buttons_width(3), 76.0);
        // Right-side captions (the Linux default: minimize,maximize,close):
        // content pads past the 10px edge inset + the button row.
        assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
        // GNOME-vanilla ":close" — a single right button.
        assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
        // Left-side captions ("close:…" layouts) shift the app cluster right
        // by the button row + one 2px gap.
        assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
        assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
        assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
        // macOS ignores the Linux caption count entirely.
        assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..92; a 16px-padded header needs 84 more px to
        // put content at 92 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
        assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
        // Linux with a left-side close caption: everything shifts one slot.
        assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 0, 16.0),
            88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 0, 16.0),
            12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: zeron sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        // Everything closed until explicitly opened (user request — the
        // brief default-open popped the pane on every visited session).
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        assert_eq!(panels.get("a").right_active, RightSurface::Picker);
        // With no selected chat there is no session to close.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true,
                ..Default::default()
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
        // The right pane round-trips back closed.
        assert!(!panels.toggle_changes("a"));
        assert!(!panels.get("a").changes_open);
    }

    #[test]
    fn session_panels_update_tracks_right_surfaces() {
        let mut panels = SessionPanels::default();
        panels.update("a", |p| p.right_active = RightSurface::Diff(3));
        assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
        // Other chats keep the picker default.
        assert_eq!(panels.get("b").right_active, RightSurface::Picker);
        panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
        assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn sidebar_chat_height_tracks_visible_metadata() {
        assert_eq!(chat_row_height(false, false), 45.0);
        assert_eq!(chat_row_height(true, false), 61.0);
        assert_eq!(chat_row_height(false, true), 63.0);
        assert_eq!(chat_row_height(true, true), 63.0);
    }

    #[test]
    fn sidebar_harness_geometry_reflects_row_hierarchy() {
        assert_eq!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP, Theme::SPACE_SM);
        assert!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP < SIDEBAR_ARCHIVED_HARNESS_TITLE_GAP);
        assert!(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE < SIDEBAR_ARCHIVED_HARNESS_ICON_SIZE);
    }

    #[test]
    fn sidebar_height_change_is_not_a_reorder() {
        let open = keys(&[("first-group", 105.0), ("second-group", 240.0)]);
        let collapsed = keys(&[("first-group", 40.0), ("second-group", 240.0)]);
        assert!(!sidebar_key_order_changed(&open, &collapsed));

        let reordered = keys(&[("second-group", 240.0), ("first-group", 40.0)]);
        assert!(sidebar_key_order_changed(&collapsed, &reordered));
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Appearance));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Appearance))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — zeron's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched empty-state entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Appearance));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Appearance))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    #[test]
    fn sidebar_disclosure_motion_lands_exactly_on_its_target() {
        let mut tween = SidebarDisclosureMotion::new(1, 240.0, 0.0);
        tween.started = std::time::Instant::now() - motion::COLLAPSE.total().mul_f32(2.0);
        assert_eq!(tween.current(), 0.0);
        assert!(!tween.animating());
    }

    #[test]
    fn keiki_menu_is_scoped_to_the_selected_chat() {
        assert!(keiki_menu_is_selected(
            "keiki-conv:one",
            Some("keiki-conv:one")
        ));
        assert!(!keiki_menu_is_selected(
            "keiki-conv:one",
            Some("keiki-conv:two")
        ));
        assert!(!keiki_menu_is_selected("engine-chat", Some("engine-chat")));
    }

    #[test]
    fn empty_state_variant_matches_keiki_session_status() {
        assert_eq!(
            empty_state_variant(crate::keiki::SessionStatus::SignedOut),
            EmptyStateVariant::SignIn
        );
        assert_eq!(
            empty_state_variant(crate::keiki::SessionStatus::Error),
            EmptyStateVariant::SignIn
        );
        assert_eq!(
            empty_state_variant(crate::keiki::SessionStatus::Loading),
            EmptyStateVariant::Loading
        );
        assert_eq!(
            empty_state_variant(crate::keiki::SessionStatus::SignedIn),
            EmptyStateVariant::Ready
        );
    }

    #[test]
    fn empty_state_action_label_matches_each_variant() {
        assert_eq!(
            empty_state_action_label(EmptyStateVariant::SignIn),
            Some("Sign in to Keiki")
        );
        assert_eq!(
            empty_state_action_label(EmptyStateVariant::Loading),
            Some("Opening Keiki…")
        );
        assert_eq!(empty_state_action_label(EmptyStateVariant::Ready), None);
    }
}
