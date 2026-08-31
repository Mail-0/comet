//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle).
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering and folder-browser navigation) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use zeron_proto::{
    ChatConfig, FolderListing, HarnessId, ReasoningLevel, RepoRef, SandboxLevel, Space,
};
use zeron_rpc::methods;

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the git
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs (the t3code env-mode).
    pub checkout: CheckoutKind,
}

/// Where a new session runs (t3code's env-mode: `local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (zeron mints a `zeron/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Byte length of `name`'s prefix matching `query`, compared char-for-char
/// case-insensitively; `None` when `query` isn't a prefix of `name`. The
/// length indexes into `name` (not `query`) so the completion suffix keeps
/// the folder's real casing: `("Documents", "doc") → Some(3)` → `"uments"`.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder `names` (slash-descend):
/// exact match first — case-SENSITIVE before case-insensitive, so `GitHub/`
/// picks a `GitHub` sibling over `github` — then a unique case-insensitive
/// prefix. Ambiguity resolves to `None`: the slash stays in the query.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

/// Interpret a palette query as a typed path jump: absolute (`/disk2/projects`)
/// or home-relative (`~`, `~/github`). Returns the absolute path to browse,
/// trailing slash trimmed. `home` is the device's resolved home — `None`
/// until the first listing lands, when `~` can't expand yet. A query like
/// `~foo` is a folder name, not a path.
pub fn typed_path_target(query: &str, home: Option<&str>) -> Option<String> {
    let query = query.trim();
    if let Some(rest) = query.strip_prefix('~') {
        let home = home?.trim_end_matches('/');
        if rest.is_empty() {
            return Some(home.to_string());
        }
        let rest = rest.strip_prefix('/')?.trim_end_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if query.starts_with('/') {
        let trimmed = query.trim_end_matches('/');
        return Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
    }
    None
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&zeron_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Sentinel for "no keyboard-highlighted row" (`active`): matches no index,
/// and `usize::MAX as isize == -1` — `menu_step` treats it like `None`, so
/// the first Down lands on row 0.
const NO_ACTIVE_ROW: usize = usize::MAX;

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    /// New-session canvas only: which project the session mints into.
    Space,
    /// New-session canvas only: the device project-less sessions run on (a
    /// project pick implies its own host and overrides this).
    Device,
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    /// Sticky last-used picks (zeron `zeron.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<String>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<String>,
    open: popover::Popup<PickerKind>,
    refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    refs_space: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    /// One-shot mute for the next Edited event's highlight reset — armed by
    /// [`Self::toggle`]'s programmatic clear (see the subscription).
    search_reset_muted: bool,
    focus: FocusHandle,
    refs_task: Option<Task<()>>,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// Last mid-session switch failure (shown in the ref popover).
    switch_error: Option<String>,
    _search_events: Subscription,
    _state_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                // Typing in a filter resets the highlight to the top of the
                // fresh results. `set_text` emits Edited on programmatic
                // clears too, and this subscription runs AFTER `toggle`
                // returns — an unmuted reset clobbers the just-anchored
                // selected row back to 0, leaving the top row wearing a
                // second highlight next to the selection (user report;
                // `toggle` arms the mute right before its clear).
                if !std::mem::take(&mut this.search_reset_muted) {
                    if this.open_kind() == Some(PickerKind::Branch) {
                        this.active = 0;
                    }
                }
                cx.notify();
            }
            ComposerInputEvent::Submitted => this.on_search_submit(cx),
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.model_options.clear();
                this.switch_error = None;
            }
            // A space switch invalidates the branch draft + cache — the folder
            // (and possibly the device) changed under them.
            let space = state.read(cx).selected_space.clone();
            if space != this.space_owner {
                this.space_owner = space;
                this.config.branch = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_space = None;
            }
            cx.notify();
        });
        // Dev/testing knob: `ZERON_OPEN_PICKER=repo|branch` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let boot_open = match std::env::var("ZERON_OPEN_PICKER").ok().as_deref() {
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            Some("device") => Some(PickerKind::Device),
            _ => None,
        };
        let mut open = popover::Popup::default();
        if let Some(kind) = boot_open {
            open.open(kind);
        }
        // Sticky target picks are loaded synchronously for the first frame.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        // Restore the last device/project picks (the canvas's "defaults to
        // last selected" rule). Vanished rows heal in `apply_spaces`. A
        // remembered "Don't work in a project" opt-out is deliberately NOT
        // restored: the menu row is gone, so a stale saved opt-out would
        // strand the canvas in a state the picker can no longer express.
        {
            let device = defaults.device.clone();
            let project = defaults.project.clone();
            state.update(cx, |s, _| {
                if s.selected_device.is_none() {
                    s.selected_device = device;
                }
                if s.selected_space.is_none() {
                    s.selected_space = project;
                }
            });
        }
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        Self {
            state,
            space_owner,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            search,
            search_reset_muted: false,
            focus: cx.focus_handle(),
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            _search_events: search_events,
            _state_observe: state_observe,
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Harness is locked once the chat exists (feature-inventory §1.7).
    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// Resolve the harness carried by the selected chat or new-chat default.
    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        self.config
            .harness
            .or_else(|| {
                self.state
                    .read(cx)
                    .selected_chat_row()
                    .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
            })
            .or(Some(HarnessId::Copilot))
    }

    /// Resolve the model carried by the draft or selected chat.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(model) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.as_ref())
            .and_then(|config| config.model.as_deref())
        {
            return Some(model);
        }
        Some("copilot")
    }

    fn effective_reasoning(&self, _cx: &App) -> Option<ReasoningLevel> {
        None
    }

    /// The explicit option picks persisted on the selected chat or draft.
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The resolved harness's steering mode.
    pub fn resolved_steering_mode(&self, cx: &App) -> Option<zeron_proto::SteeringMode> {
        (self.effective_harness(cx) == Some(HarnessId::Copilot))
            .then_some(zeron_proto::SteeringMode::TurnBoundary)
    }

    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self.effective_model_id(cx).map(str::to_string),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    // ---- open/close ----

    /// The picker that's open AND interactive — `None` while one animates out.
    fn open_kind(&self) -> Option<PickerKind> {
        self.open.as_open().copied()
    }

    /// Whether any picker popover is open (shell-side: session-nav shortcuts
    /// go quiet underneath an open popover instead of yanking the session out
    /// from under it).
    pub fn is_open(&self) -> bool {
        self.open.as_open().is_some()
    }

    /// The picker to render: open or mid-exit.
    fn mounted_kind(&self) -> Option<PickerKind> {
        self.open.get().copied()
    }

    /// Begin the exit animation (shared by every close path).
    fn animate_close(&mut self, cx: &mut Context<Self>) {
        if self.open.begin_close() {
            popover::reap_popup(cx, |pickers: &mut Self| &mut pickers.open);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.animate_close(cx);
        cx.notify();
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        // A press that found this picker open closes it — the card's
        // `on_mouse_down_out` already began the close on that same press,
        // so by click time the popup reads as closed and a plain toggle
        // would reopen it. A press while a DIFFERENT picker is open doesn't
        // count (see note_trigger_press_matching): that click switches.
        let pressed_open = self.open.take_press_was_open();
        if self.open_kind() == Some(kind) || pressed_open {
            self.animate_close(cx);
            cx.notify();
            return;
        }
        self.open.open(kind);
        // Clearing stale text emits Edited AFTER this function returns —
        // mute that one event so its reset can't clobber the highlight
        // anchored below (the no-op clear is also skipped for the same
        // reason).
        self.search_reset_muted = !self.search.read(cx).text().is_empty();
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report).
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::Space => self.selected_space_index(cx),
            PickerKind::Device => self.selected_device_index(cx),
        };
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Branch => {
                self.switch_error = None; // stale mid-session failures don't linger
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search agents…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Device => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search devices…", cx);
                });
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            // Projects and devices are already synced state — nothing to load.
            PickerKind::Space | PickerKind::Device => {}
        }
        cx.notify();
    }

    fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return;
        }
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if pickers.open_kind() == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Refs are fixed at creation: an existing session can never move
        // (wing's rule — the footer renders read-only labels there, so this
        // is a belt-and-braces guard).
        if self.state.read(cx).selected_chat_row().is_some() {
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree") — the
            // t3code `reuseExistingWorktree` path.
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (full t3code `switchRef` — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.animate_close(cx);
        cx.notify();
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder
    /// (relay-forwarded for remote spaces). Success records the pick and
    /// refreshes tags; failure keeps the popover open with git's message.
    fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.config.branch = Some(ref_name);
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current plain ref picked:
            // drop the pick (we don't checkout the main folder) — the current
            // branch takes over.
            self.config.branch = None;
        }
        self.config.checkout = kind;
        self.animate_close(cx);
        cx.notify();
    }

    fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution (the t3code env-mode semantics) ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// the session's branch on an existing chat, the draft pick on a new one,
    /// else the current branch. Capped to the displayed window.
    fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel` /
    /// `resolveCurrentWorkspaceLabel`).
    fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => {
                if self.selected_ref_worktree().is_some() {
                    "Current worktree"
                } else {
                    "Current checkout"
                }
            }
        }
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    // ---- the space picker (new-session canvas) ----

    /// The picker's project rows: scoped to the canvas's device — the device
    /// switcher narrows the list, projects on other devices don't show
    /// (pick the device first, then its project). Unscoped only while the
    /// device is still unknown (pre-probe boot).
    fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        state
            .spaces_sorted()
            .into_iter()
            .filter(|s| match device.as_deref() {
                Some(d) => s.device_id == d,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// [`Self::scoped_space_rows`] matching the search query, ranked
    /// (`popover::filter_indices`).
    fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let spaces = self.scoped_space_rows(cx);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    /// Row index of the currently selected space (un-searched open) — within
    /// the scoped order [`filtered_space_rows`] lists on an empty query.
    /// [`NO_ACTIVE_ROW`] when nothing is selected (the no-project canvas must
    /// not open with row 0 wearing a phantom highlight — user report).
    fn selected_space_index(&self, cx: &App) -> usize {
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| self.scoped_space_rows(cx).iter().position(|s| s.id == id))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// Re-home the canvas onto another project. The state observer resets the
    /// branch draft and ref cache for the new project.
    fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_device(device_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the device/project picks — the "last selected" defaults the
    /// next boot's canvas restores.
    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.device = state
                .selected_device
                .clone()
                .or_else(|| state.local_device_id.clone());
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir {
            if let Err(err) = self.defaults.save(dir) {
                tracing::warn!(error = %err, "composer-defaults save failed");
            }
        }
    }

    /// Devices in picker order: this device first, then by name.
    fn device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let mut devices: Vec<zeron_proto::Device> = state.devices.clone();
        devices.sort_by_key(|d| {
            (
                local.as_deref() != Some(d.id.as_str()),
                d.name.to_lowercase(),
                d.id.clone(),
            )
        });
        devices
    }

    /// [`Self::device_rows`] filtered by the search box (same ranked
    /// substring match as the project rows).
    fn filtered_device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.device_rows(cx);
        let names: Vec<String> = rows.iter().map(|d| d.name.clone()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    fn selected_device_index(&self, cx: &App) -> usize {
        let effective = self.state.read(cx).effective_device_id();
        self.device_rows(cx)
            .iter()
            .position(|d| Some(d.id.as_str()) == effective.as_deref())
            .unwrap_or(0)
    }

    /// The device popover: search + one row per device (name, muted "offline"
    /// tag, check on the canvas's effective device).
    fn render_device_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows = self.filtered_device_rows(cx);
        let (effective, local, online): (Option<String>, Option<String>, Vec<bool>) = {
            let state = self.state.read(cx);
            (
                state.effective_device_id(),
                state.local_device_id.clone(),
                rows.iter()
                    .map(|d| state.device_online(&d.id, now))
                    .collect(),
            )
        };
        let active = self.active;
        let body: AnyElement =
            if rows.is_empty() {
                div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No devices match."))
                    .into_any_element()
            } else {
                div()
                    .id("device-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .children(rows.into_iter().zip(online).enumerate().map(
                        |(ix, (device, online))| {
                            let is_local = local.as_deref() == Some(device.id.as_str());
                            let label: SharedString = device.name.clone().into();
                            let is_selected = effective.as_deref() == Some(device.id.as_str());
                            let pick_id = device.id.clone();
                            popover::menu_row_nav(
                                &theme,
                                is_selected,
                                ix == active,
                                format!("device-row-{ix}"),
                            )
                            .id(("device-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_device(pick_id.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            // The local device wears a muted right-aligned "You"
                            // instead of a "(this device)" suffix in the name.
                            .when(is_local, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from("You")),
                                )
                            })
                            // Disconnected glyph, not the word (user request).
                            .when(!online, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::WIFI_OFF)
                                        .size(px(12.0))
                                        .flex_none()
                                        .text_color(theme.warning.opacity(0.8)),
                                )
                            })
                        },
                    ))
                    .into_any_element()
            };
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .into_any_element()
    }

    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No agent selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        let session_branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.branch.clone());
        let selected = session_branch.or_else(|| self.config.branch.clone());
        let active = self.active;
        let switching = self.switching.clone();
        let body: AnyElement = match &self.refs {
            Loadable::Loading | Loadable::Idle => {
                popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
            }
            Loadable::Error(message) => {
                self.retry_row("branch-retry", message, PickerKind::Branch, &theme, cx)
            }
            Loadable::Ready(_) if rows.is_empty() => div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No refs found."))
                .into_any_element(),
            Loadable::Ready(_) => div()
                .id("branch-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(
                    rows.into_iter()
                        .take(MAX_REF_ROWS)
                        .enumerate()
                        .map(|(ix, row)| {
                            let is_selected = selected.as_deref() == Some(row.name.as_str());
                            let tag = if row.current {
                                Some("current")
                            } else if row.worktree_path.is_some() {
                                Some("worktree")
                            } else {
                                None
                            };
                            let is_switching = switching.as_deref() == Some(row.name.as_str());
                            let row_name = row.name.clone();
                            popover::menu_row_nav(
                                &theme,
                                is_selected,
                                ix == active,
                                format!("branch-row-{ix}"),
                            )
                            .id(("branch-row", ix))
                            .when(switching.is_some(), |element| element.opacity(0.55))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_ref(row.clone(), cx);
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(row_name)),
                            )
                            .when(is_switching, |element| {
                                element.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted.opacity(0.6))
                                        .child(SharedString::from("switching…")),
                                )
                            })
                            .when_some(tag, |element, tag| {
                                element.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from(tag)),
                                )
                            })
                        }),
                )
                .into_any_element(),
        };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let options = [
            (
                CheckoutKind::Local,
                if has_worktree {
                    "Current worktree"
                } else {
                    "Current checkout"
                },
                if has_worktree {
                    crate::icons::FOLDER_WITH_FILES
                } else {
                    crate::icons::FOLDER
                },
            ),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        popover::menu_row_nav(
                            &theme,
                            current == kind,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The project popover: search + one row per project on the picked device
    /// (check on the current pick), then a "New project…" action row. Rows
    /// are device-scoped, so no per-row `@ device` tag — the device chip next
    /// door names the host.
    fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        let active = self.active;
        let body: AnyElement = if rows.is_empty() {
            // Distinguish "the filter ate everything" from "this device has
            // no projects yet" — the scoped list makes the latter common.
            let empty: &str = if self.search.read(cx).text().is_empty() {
                "No agents on this device."
            } else {
                "No agents match."
            };
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(empty.to_string()))
                .into_any_element()
        } else {
            div()
                .id("space-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(ix, space)| {
                    let label: SharedString = space.display_name().to_string().into();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let pick_id = space.id.clone();
                    popover::menu_row_nav(
                        &theme,
                        is_selected,
                        ix == active,
                        format!("space-row-{ix}"),
                    )
                    .id(("space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_space(pick_id.clone(), cx);
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                }))
                .into_any_element()
        };
        // Action row under a hairline: mint a project.
        let new_project = popover::menu_row_nav(&theme, false, false, "project-new".to_string())
            .id("project-new")
            .on_click(cx.listener(|this, _, window, cx| {
                this.close(cx);
                window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
            }))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from("New agent…")),
            );
        let new_keiki = self
            .state
            .read(cx)
            .keiki_status
            .eq(&crate::keiki::SessionStatus::SignedIn)
            .then(|| {
                popover::menu_row_nav(&theme, false, false, "keiki-agent-new".to_string())
                    .id("keiki-agent-new")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close(cx);
                        window.dispatch_action(Box::new(crate::shell::NewKeikiAgent), cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::PLUS)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from("New Keiki agent…")),
                    )
            });
        div()
            .flex()
            .flex_col()
            // Same 2px rhythm as the list's own row gap — the action rows
            // sat flush while list rows breathed (user report).
            .gap(px(2.0))
            .child(self.search_box(&theme))
            .child(body)
            .child(
                // Full-bleed through the card's 4px inset — a divider
                // stopping short of the edges read as a mistake.
                div()
                    .my(px(2.0))
                    .mx(px(-4.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.border.opacity(0.6)),
            )
            .child(new_project)
            .when_some(new_keiki, |el, row| {
                el.child(
                    div()
                        .my(px(2.0))
                        .mx(px(-4.0))
                        .h(px(1.0))
                        .flex_none()
                        .bg(theme.border.opacity(0.6)),
                )
                .child(row)
            })
            .into_any_element()
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open_kind() == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
        if self.open_kind() == Some(PickerKind::Space)
            && let Some(space) = self.filtered_space_rows(cx).into_iter().nth(self.active)
        {
            self.pick_space(space.id, cx);
        }
        if self.open_kind() == Some(PickerKind::Device)
            && let Some(device) = self.filtered_device_rows(cx).into_iter().nth(self.active)
        {
            self.pick_device(device.id, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // The frame stays mounted (and possibly focused) through the exit
        // animation — keys must not drive a dying popover.
        if !self.open.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        match key {
            MenuKey::Escape => {
                self.animate_close(cx);
                cx.notify();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open_kind() {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    Some(PickerKind::Space) => self.filtered_space_rows(cx).len(),
                    Some(PickerKind::Device) => self.filtered_device_rows(cx).len(),
                    None => 0,
                };
                let current = (self.active != NO_ACTIVE_ROW).then_some(self.active);
                self.active = popover::menu_step(current, count, delta).unwrap_or(0);
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open_kind() == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    // ---- render ----

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            // Four of these share one row now (device, project, checkout,
            // ref): cap each early and let them SHRINK (`min_w_0`) — without
            // it the clusters overflowed into each other and the labels
            // painted overlapped (user report).
            .max_w(px(160.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    /// The new-session target row — device + project selector chips rendered
    /// ABOVE the composer pill, left-aligned like the checkout toolbar (the
    /// composer footer carries only checkout + ref, and sessions show their
    /// target in the titlebar instead).
    pub fn render_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            Some(PickerKind::Device) => {
                let content = self.render_device_popover(cx);
                Some((PickerKind::Device, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let (device_label, project_label, offline) = {
            let state = self.state.read(cx);
            let device_id = state.effective_device_id();
            let device_label: SharedString = device_id
                .as_deref()
                .and_then(|id| state.device_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| "This device".to_string())
                .into();
            let offline = device_id
                .as_deref()
                .is_some_and(|id| !state.device_online(id, chrono::Utc::now()));
            let project_label: SharedString = state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No agent".to_string())
                .into();
            (device_label, project_label, offline)
        };
        let device_chip = self
            .footer_chip(
                PickerKind::Device,
                "picker-device",
                crate::icons::MONITOR,
                device_label,
                &theme,
                cx,
            )
            .when(offline, |el| el.text_color(theme.warning.opacity(0.8)));
        let project_chip = self.footer_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::FOLDER,
            project_label,
            &theme,
            cx,
        );
        // Same left-edge geometry as the checkout toolbar under the pill
        // (`render_footer`'s row): full-width, 10px inset, chips hugging the
        // left. The row sits just above the composer pill, so the menus open
        // UPWARD.
        div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(10.0))
            .child(attach_overlay(
                device_chip,
                &mut overlay,
                PickerKind::Device,
                "device-popover",
                closing,
            ))
            .child(attach_overlay(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .into_any_element()
    }

    /// The composer footer row: checkout-kind + ref, LEFT-aligned, only when
    /// the picked (or session's) project has git. Device + project moved to
    /// the row above the pill ([`Self::render_target_selectors`]); sessions
    /// name their target in the titlebar.
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty locked state.
        let (space, session, change_request) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            let change_request = session
                .as_ref()
                .and_then(|chat| state.change_request_for_chat(chat).cloned());
            (space, session, change_request)
        };
        let row = || {
            // Symmetric: the container's 8px gap sits above the toolbar;
            // bleeding 8 of the container's 16px bottom padding (mb -8)
            // leaves 8 below — equal air on both sides of the row.
            // `w_full` is load-bearing: without it the canvas layout sizes
            // the row to CONTENT, and the left cluster's flex_1 (basis 0)
            // collapsed to zero width — both clusters painted from the same
            // origin, chips overlapping (user report).
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .mb(px(-8.0))
        };

        if let Some(chat) = &session {
            // Sessions never move: read-only checkout-kind + ref labels,
            // LEFT-aligned, only when the session's project has git. The
            // target (project @ device) lives in the titlebar now.
            let Some(space) = space.as_ref().filter(|s| s.git_detected) else {
                return None;
            };
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let (icon_path, label) = if is_worktree {
                (crate::icons::FOLDER_WITH_FILES, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            // Mirrors the draft chips: checkout hugs the left edge, ref the
            // right.
            let left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(label),
                    &theme,
                ));
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_w_0()
                .when_some(change_request, |el, summary| {
                    el.child(crate::change_requests::pull_request_badge(
                        "composer-pull-request".into(),
                        summary,
                        crate::change_requests::ChangeRequestBadgeSurface::Composer,
                        &theme,
                    ))
                })
                .child(Self::footer_label(
                    crate::icons::GIT_BRANCH,
                    chat.branch
                        .clone()
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from("No ref")),
                    &theme,
                ));
            return Some(row().child(left).child(right).into_any_element());
        }

        // New-session draft: checkout + ref only, LEFT-aligned (device +
        // project live in the row above the pill now).
        let git = space.as_ref().is_some_and(|s| s.git_detected);
        if !git {
            return None;
        }
        // Refs feed the draft labels — eager + idempotent.
        self.ensure_refs(false, cx);
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            // Space/Device popovers mount on the target row above the pill
            // (`render_target_selectors`), not here.
            _ => None,
        };

        let ref_label = self.ref_label();
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        // Checkout on the left edge, ref on the right — the row's
        // justify_between splits them (user request).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
                closing,
            ));
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay_end(
                ref_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
                closing,
            ));
        Some(row().child(left).child(right).into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            // zeron caps its tallest picker at min(640px, 75vh).
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        // Projects/devices load nothing; no retry surface exists.
                        PickerKind::Space | PickerKind::Device => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::Copilot | HarnessId::Mock | HarnessId::Unknown(_) => {
            (crate::icons::MONITOR, None)
        }
    }
}

fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay
        .as_ref()
        .is_some_and(|(open_kind, _)| *open_kind == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element, closing));
    }
    chip
}

fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay
        .as_ref()
        .is_some_and(|(open_kind, _)| *open_kind == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element, closing));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::FolderEntry;

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn completion_prefix_lengths() {
        // Case-insensitive; the length indexes into the NAME's bytes.
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("zeron", "zeron"), Some(5));
        assert_eq!(completion_prefix_len("zeron", ""), Some(0));
        assert_eq!(completion_prefix_len("zeron", "dev"), None);
        // Longer than the name → not a prefix.
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        // Multibyte names slice on a char boundary.
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        // Exact casing beats the earlier case-insensitive sibling…
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        // …but with no exact-cased hit, case-insensitive exact still lands.
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
        // Unique prefix descends; an ambiguous one keeps the slash honest.
        assert_eq!(segment_target(&names, "work"), Some(2));
        assert_eq!(segment_target(&names, "g"), None);
        assert_eq!(segment_target(&names, "x"), None);
    }

    #[test]
    fn typed_path_target_expands_absolute_and_home_paths() {
        let home = Some("/home/wing");
        assert_eq!(typed_path_target("/disk2/", home), Some("/disk2".into()));
        assert_eq!(
            typed_path_target("/disk2/projects", home),
            Some("/disk2/projects".into())
        );
        assert_eq!(typed_path_target("/", home), Some("/".into()));
        assert_eq!(typed_path_target("~", home), Some("/home/wing".into()));
        assert_eq!(typed_path_target("~/", home), Some("/home/wing".into()));
        assert_eq!(
            typed_path_target("~/github/", home),
            Some("/home/wing/github".into())
        );
        // `~x` is a folder name; relative queries are searches, not paths.
        assert_eq!(typed_path_target("~x", home), None);
        assert_eq!(typed_path_target("src", home), None);
        // `~` can't expand before the device's home is known.
        assert_eq!(typed_path_target("~/github", None), None);
        assert_eq!(typed_path_target("/disk2", None), Some("/disk2".into()));
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "zeron".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "zeron");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::Copilot);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::Copilot);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }
}
