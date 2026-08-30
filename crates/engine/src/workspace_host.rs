//! WorkspaceHost owns local workspace snapshot persistence and the typed watch
//! channels that feed the entity RPC streams.
//!
//! Writer discipline (kept from the doc schema): this host writes its own device row,
//! its own session-status rows, and rows for chats it hosts; renames/archives are LWW
//! sets accepted from any device (the Mutate surface).
//!
//! `lastSeenAt` is written on boot and shutdown for the local device row.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use chrono::Utc;
use tokio::sync::watch;

use zeron_doc::DocsStore;
use zeron_doc::{DeletedSpace, REGISTRY_DOC_ID, RegistryDoc};
use zeron_proto::{Chat, ChatConfig, Device, Session, Space};

use crate::EngineError;

/// Debounce window for local snapshot saves after a change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;
#[derive(Debug, Clone)]
pub struct WorkspaceHostConfig {
    pub device_id: String,
    /// Human name for this device's registry row (hostname by default).
    pub device_name: String,
    /// `std::env::consts::OS`-style platform string.
    pub platform: String,
}

struct WorkspaceHostInner {
    store: Arc<DocsStore>,
    config: WorkspaceHostConfig,
    reg: Arc<Mutex<RegistryDoc>>,
    chats_tx: watch::Sender<Vec<Chat>>,
    devices_tx: watch::Sender<Vec<Device>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    spaces_tx: watch::Sender<Vec<Space>>,
    /// Bumped on every local registry change — drives republish and the snapshot
    /// debounce in `workspace_task`.
    changed_tx: watch::Sender<u64>,
    // Local-only workspace state ends here.
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct WorkspaceHost {
    inner: Arc<WorkspaceHostInner>,
}

impl WorkspaceHost {
    /// Load or initialize the local workspace snapshot and start persistence.
    pub fn open(store: Arc<DocsStore>, config: WorkspaceHostConfig) -> Result<Self, EngineError> {
        let mut doc = match store.load_snapshot(REGISTRY_DOC_ID)? {
            Some(bytes) => RegistryDoc::from_bytes(&bytes, &config.device_id)
                .map_err(|e| EngineError::Other(format!("registry snapshot load failed: {e}")))?,
            None => RegistryDoc::new(&config.device_id),
        };
        // Boot: upsert our own device row. A user-set name (RenameDevice is LWW from
        // any device) survives restarts. The old fallback sentinel is repaired with
        // the platform-resolved name because it was never a user-selected name.
        let now = Utc::now();
        let existing = doc
            .read_devices()?
            .into_iter()
            .find(|d| d.id == config.device_id);
        doc.upsert_device(&Device {
            id: config.device_id.clone(),
            name: device_name_on_boot(
                existing.as_ref().map(|device| device.name.as_str()),
                &config.device_name,
            ),
            platform: config.platform.clone(),
            last_seen_at: Some(now),
            // First registration stamps `createdAt`; restarts keep the original.
            created_at: existing.and_then(|d| d.created_at).or(Some(now)),
            // Every boot restamps the running binary version.
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        })?;

        let state = doc.read_all()?;
        let (chats_tx, _) = watch::channel(state.chats);
        let (devices_tx, _) = watch::channel(state.devices);
        let (sessions_tx, _) = watch::channel(state.sessions);
        let (spaces_tx, _) = watch::channel(state.spaces);
        let (changed_tx, changed_rx) = watch::channel(0u64);

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                reg: Arc::new(Mutex::new(doc)),
                chats_tx,
                devices_tx,
                sessions_tx,
                spaces_tx,
                changed_tx,
            }),
        };
        // Persist immediately so the registry snapshot exists even if the
        // process dies before the first debounced save.
        host.inner.save_snapshot();
        tokio::spawn(workspace_task(Arc::downgrade(&host.inner), changed_rx));
        Ok(host)
    }

    // ── registry access helpers ─────────────────────────────────────────────

    /// Run a mutation under the registry lock, then wake the publish/persist
    /// task and persist the local write.
    fn mutate<R>(&self, f: impl FnOnce(&mut RegistryDoc) -> R) -> R {
        let result = f(&mut lock(&self.inner.reg));
        self.inner.bump_changed();
        result
    }

    fn read<R>(&self, f: impl FnOnce(&RegistryDoc) -> R) -> R {
        f(&lock(&self.inner.reg))
    }

    /// The chat row as currently known (overlay view).
    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, EngineError> {
        Ok(self.read(|doc| doc.chat(chat_id))?)
    }

    /// The space row as currently known (overlay view).
    pub fn space(&self, space_id: &str) -> Result<Option<Space>, EngineError> {
        Ok(self.read(|doc| doc.space(space_id))?)
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, EngineError> {
        Ok(self.read(|doc| doc.read_chats())?)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, EngineError> {
        Ok(self.read(|doc| doc.read_devices())?)
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, EngineError> {
        Ok(self.read(|doc| doc.read_sessions())?)
    }

    // ── watches (WatchChats / WatchDevices / merged WatchSessions) ──────────

    pub fn watch_chats(&self) -> watch::Receiver<Vec<Chat>> {
        self.inner.chats_tx.subscribe()
    }

    pub fn watch_devices(&self) -> watch::Receiver<Vec<Device>> {
        self.inner.devices_tx.subscribe()
    }

    /// Raw locally persisted session-status rows.
    pub fn watch_session_rows(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn watch_spaces(&self) -> watch::Receiver<Vec<Space>> {
        self.inner.spaces_tx.subscribe()
    }

    /// WatchSessions source: locally persisted rows merged with this engine's
    /// live status watch (the local view is fresher for our own runs).
    pub fn merged_sessions_watch(
        &self,
        local: watch::Receiver<Vec<Session>>,
    ) -> watch::Receiver<Vec<Session>> {
        let mut rows = self.watch_session_rows();
        let mut local = local;
        let device_id = self.inner.config.device_id.clone();
        let (tx, rx) = watch::channel(merge_sessions(&device_id, &rows.borrow(), &local.borrow()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rows.changed() => if changed.is_err() { break },
                    changed = local.changed() => if changed.is_err() { break },
                }
                let merged = merge_sessions(
                    &device_id,
                    &rows.borrow_and_update(),
                    &local.borrow_and_update(),
                );
                if tx.send(merged).is_err() {
                    break; // no receivers left
                }
            }
        });
        rx
    }

    // ── chat ownership ──────────────────────────────────────────────────────

    /// Writer discipline: the chat's host is its row's `deviceId`. Unknown chats
    /// are claimable — the first run command claims them via [`Self::claim_chat`].
    pub fn is_host(&self, chat_id: &str) -> bool {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(Some(chat)) => chat.device_id == self.inner.config.device_id,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                true
            }
        }
    }

    /// Claim-on-first-command: create the chat row under OUR device id when a run
    /// command arrives for a chat with no row yet. No-op when the row exists.
    ///
    /// The claim is a PARTIAL row write (identity/cwd/space only): the command
    /// plane is nudged and outruns the registry channel, so the client's
    /// `createChat` for the same chat routinely arrives AFTER the claim with
    /// older clocks — fields the claim never wrote (`config`, `title`) must
    /// still land then.
    ///
    /// Spaces invariant: every chat belongs to a space, so the claim resolves an
    /// own-device space matching `cwd` — or auto-creates one (gitDetected false;
    /// SpacesSync corrects on its next pass). A cwd-less claim (e.g. note_message
    /// racing ahead of the run command) leaves `spaceId` unset; the row is
    /// invisible to the UI until a spaced claim/create lands.
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(());
        }
        let space_id = match cwd {
            Some(cwd) => Some(self.space_for_path(cwd)?),
            None => None,
        };
        self.mutate(|doc| doc.claim_chat(chat_id, cwd, space_id.as_deref(), Utc::now()));
        Ok(())
    }

    /// An own-device space whose path matches, else one at the path's parent
    /// checkout root, else a freshly created one at that root.
    ///
    /// A linked-worktree cwd resolves to the checkout root FIRST: claiming at
    /// the worktree path itself minted a phantom sidebar space named after the
    /// worktree folder ("clever-ember") next to the project's real space.
    fn space_for_path(&self, path: &str) -> Result<String, EngineError> {
        let device_id = &self.inner.config.device_id;
        let spaces = self.read(|doc| doc.read_spaces())?;
        if let Some(space) = spaces
            .iter()
            .find(|s| s.device_id == *device_id && s.path == path)
        {
            return Ok(space.id.clone());
        }
        let root = linked_worktree_root(std::path::Path::new(path));
        if let Some(root) = root.as_deref()
            && let Some(space) = spaces
                .iter()
                .find(|s| s.device_id == *device_id && s.path == root)
        {
            return Ok(space.id.clone());
        }
        let space = Space {
            id: crate::new_id(),
            device_id: device_id.clone(),
            path: root.unwrap_or_else(|| path.to_string()),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.mutate(|doc| doc.upsert_space(&space))?;
        Ok(space.id)
    }

    /// The chat's configured harness/model row, when present (RunRequest harness
    /// selection; callers fall back to the engine default).
    pub fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => chat.and_then(|c| c.config),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    // ── host-side row writes ────────────────────────────────────────────────

    /// Sidebar freshness on message persist: preview = first 120 chars of the last
    /// message's text. Claims the row first so a pre-workspace chat gains one.
    pub fn note_message(&self, chat_id: &str, text: &str) {
        let preview: String = text.chars().take(120).collect();
        let result = self.claim_chat(chat_id, None).and_then(|_| {
            self.mutate(|doc| doc.set_chat_last_message(chat_id, &preview, Utc::now()))
                .map_err(EngineError::from)
        });
        if let Err(err) = result {
            tracing::warn!(chat = %chat_id, error = %err, "registry last-message write failed");
        }
    }

    /// Resume continuity: stamp the chat row with the harness-native session id
    /// of its latest run and the cwd it was created under. An empty `session_id`
    /// tombstones the row ("do not resume" after a rejected resume). Best-effort:
    /// a missing chat row (claim happens on first command) just returns.
    pub fn set_chat_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        match self.mutate(|doc| doc.set_chat_harness_session(chat_id, session_id, cwd)) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry harness-session write failed");
            }
        }
    }

    /// The chat row's stored harness session `(session_id, cwd)`, if stamped.
    /// The empty-string tombstone passes through — callers must treat it as
    /// "explicitly no resume" (and must NOT fall back to older sources).
    pub fn chat_harness_session(&self, chat_id: &str) -> Option<(String, Option<String>)> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => {
                let chat = chat?;
                let id = chat.harness_session_id?;
                Some((id, chat.harness_session_cwd))
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.mutate(|doc| doc.upsert_session(session)) {
            tracing::warn!(chat = %session.chat_id, error = %err, "registry session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    /// Create a chat, usually *in a project*: the project fixes the host device
    /// and base cwd (`cwd` override = an isolated-worktree path). With no
    /// `space_id` the chat is project-less: `device_id` picks the host and the
    /// cwd defaults to `~` (expanded host-side when the run spawns).
    pub fn create_chat(
        &self,
        chat_id: &str,
        space_id: Option<&str>,
        device_id: Option<&str>,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        let space = match space_id {
            Some(space_id) => match self.read(|doc| doc.space(space_id))? {
                Some(space) => Some(space),
                None => return Err(EngineError::Other(format!("no such space: {space_id}"))),
            },
            None => None,
        };
        let host_device = match (&space, device_id) {
            (Some(space), _) => space.device_id.clone(),
            (None, Some(device_id)) => device_id.to_string(),
            (None, None) => {
                return Err(EngineError::Other(
                    "createChat needs a spaceId or a deviceId".into(),
                ));
            }
        };
        self.mutate(|doc| {
            doc.upsert_chat(&Chat {
                id: chat_id.to_string(),
                device_id: host_device.clone(),
                title: None,
                archived: false,
                cwd: Some(cwd.unwrap_or_else(|| {
                    space
                        .as_ref()
                        .map(|s| s.path.clone())
                        .unwrap_or_else(|| "~".to_string())
                })),
                branch: None,
                checkout_id: None,
                source_context: None,
                config,
                last_message_preview: None,
                last_message_at: None,
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
            })
        })?;
        Ok(())
    }

    // ── spaces (Mutate surface + owner stamps) ──────────────────────────────

    /// Create a space (any device). Idempotent by id; a live duplicate of the
    /// same `(deviceId, path)` is a no-op backstop (the UI reuses via
    /// WatchSpaces). `git_detected` is seeded from the picker's FolderEntry;
    /// the owning device's SpacesSync re-verifies.
    pub fn create_space(
        &self,
        space_id: &str,
        device_id: &str,
        path: &str,
        name: Option<String>,
        git_detected: bool,
    ) -> Result<(), EngineError> {
        let spaces = self.read(|doc| doc.read_spaces())?;
        if spaces
            .iter()
            .any(|s| s.id == space_id || (s.device_id == device_id && s.path == path))
        {
            return Ok(());
        }
        self.mutate(|doc| {
            doc.upsert_space(&Space {
                id: space_id.to_string(),
                device_id: device_id.to_string(),
                path: path.to_string(),
                name,
                git_detected,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            })
        })?;
        Ok(())
    }

    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_space(space_id, name))?)
    }

    /// Hard-delete a space and its chats (registry cascade — one atomic batch).
    /// The caller (rpc layer) tears down live runs / doc-host handles for the
    /// returned chat ids.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, EngineError> {
        Ok(self.mutate(|doc| doc.delete_space(space_id))?)
    }

    /// Seen marker with a monotonic guard in the local index.
    pub fn mark_chat_seen(
        &self,
        chat_id: &str,
        at: chrono::DateTime<Utc>,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_seen(chat_id, at))?)
    }

    /// Owner-only git stamp (SpacesSync). Refuses rows owned by another device.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
    ) -> Result<bool, EngineError> {
        match self.read(|doc| doc.space(space_id))? {
            Some(space) if space.device_id == self.inner.config.device_id => {
                Ok(self
                    .mutate(|doc| doc.set_space_git(space_id, detected, checkout_id, Utc::now()))?)
            }
            Some(space) => {
                tracing::warn!(
                    space = %space_id, owner = %space.device_id,
                    "refusing git stamp on space owned by another device"
                );
                Ok(false)
            }
            None => Ok(false),
        }
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, EngineError> {
        Ok(self.read(|doc| doc.read_spaces())?)
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_chat(chat_id, title))?)
    }

    /// Backdate a chat's activity timestamps (epoch ms). Returns false when
    /// the chat doesn't exist.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        last_message_at: Option<i64>,
        created_at: Option<i64>,
    ) -> Result<bool, EngineError> {
        let Some(mut chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        if let Some(ms) = last_message_at {
            chat.last_message_at = chrono::DateTime::<Utc>::from_timestamp_millis(ms);
        }
        if let Some(ms) = created_at
            && let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        {
            chat.created_at = at;
        }
        self.mutate(|doc| doc.upsert_chat(&chat))?;
        Ok(true)
    }

    /// Re-home a chat to another device (tooling/seeds; a future device
    /// migration flow will drive this). Returns false when the chat doesn't
    /// exist.
    pub fn set_chat_host(&self, chat_id: &str, device_id: &str) -> Result<bool, EngineError> {
        let Some(mut chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        chat.device_id = device_id.to_string();
        self.mutate(|doc| doc.upsert_chat(&chat))?;
        Ok(true)
    }

    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_archived(chat_id, archived))?)
    }

    /// LWW full-config replace on the chat row (zeron `SetChatConfig` — the
    /// composer's mid-session model/reasoning/options changes). Returns false
    /// when the chat doesn't exist.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_config(chat_id, config))?)
    }

    /// Tombstone: removes the chats (and session-status) row; the per-chat session
    /// doc remains untouched.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.delete_chat(chat_id))?)
    }

    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_device(device_id, name))?)
    }

    // ── git metadata (diff-sync host writes) ────────────────────────────────

    /// HEAD-watcher reconciliation: the branch checked out at the chat's cwd.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_branch(chat_id, branch))?)
    }

    pub fn set_chat_source_context(
        &self,
        chat_id: &str,
        context: &zeron_proto::ConversationSourceContext,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_source_context(chat_id, context))?)
    }

    /// Retarget a chat onto another folder (mid-session switch to an existing
    /// worktree). Resume is cwd-scoped — the next run there starts fresh.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_cwd(chat_id, cwd))?)
    }

    /// Canonical checkout identity for the chat's cwd (diff grouping key).
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_checkout(chat_id, checkout_id))?)
    }

    // ── persistence / teardown ──────────────────────────────────────────────

    /// Persist the snapshot now (shutdown path; bypasses the debounce).
    pub fn flush(&self) {
        self.inner.save_snapshot();
    }

    /// Shutdown: stamp our `lastSeenAt` (the only periodic-ish row write besides
    /// boot) and flush the snapshot.
    pub fn shutdown(&self) {
        let now = Utc::now();
        let device_id = self.inner.config.device_id.clone();
        if let Err(err) = self.mutate(|doc| doc.set_device_last_seen(&device_id, now)) {
            tracing::warn!(error = %err, "device lastSeenAt stamp failed");
        }
        self.inner.save_snapshot();
    }
}

impl WorkspaceHostInner {
    fn bump_changed(&self) {
        self.changed_tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn publish(&self) {
        match lock(&self.reg).read_all() {
            Ok(state) => {
                // send_replace, NOT send: `watch::Sender::send` drops the value when
                // no receiver exists yet, so a stream subscribed later would start
                // from a stale snapshot (found the hard way by the e2e smoke).
                self.chats_tx.send_replace(state.chats);
                self.devices_tx.send_replace(state.devices);
                self.sessions_tx.send_replace(state.sessions);
                self.spaces_tx.send_replace(state.spaces);
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry read failed");
            }
        }
    }

    fn save_snapshot(&self) {
        let bytes = lock(&self.reg).to_bytes();
        match bytes {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot(REGISTRY_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "registry snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry snapshot export failed");
            }
        }
    }
}

/// The parent checkout root of a linked git worktree: `<path>/.git` is a FILE
/// containing `gitdir: <root>/.git/worktrees/<name>`. `None` for a primary
/// checkout (`.git` is a directory), a non-repo folder, or any other layout
/// (bare-repo worktrees have no `<root>` working copy to attribute to). Pure
/// fs reads — no git subprocess; this runs on the synchronous claim path.
pub(crate) fn linked_worktree_root(path: &std::path::Path) -> Option<String> {
    let gitfile = path.join(".git");
    if !std::fs::metadata(&gitfile).ok()?.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&gitfile).ok()?;
    let target = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let mut target = std::path::PathBuf::from(target);
    if target.is_relative() {
        // Rare (`worktree.useRelativePaths`); canonicalize resolves the
        // `../..` hops against the real filesystem.
        target = std::fs::canonicalize(path.join(target)).ok()?;
    }
    let worktrees = target.parent()?;
    let dot_git = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || dot_git.file_name()? != ".git" {
        return None;
    }
    Some(dot_git.parent()?.to_string_lossy().into_owned())
}

/// Local live statuses win for this device's chats; every other device's rows come
/// from the registry. Sorted by chat id (stable stream output).
fn merge_sessions(device_id: &str, rows: &[Session], local: &[Session]) -> Vec<Session> {
    let mut merged: std::collections::HashMap<String, Session> = rows
        .iter()
        .filter(|s| s.device_id != device_id)
        .map(|s| (s.chat_id.clone(), s.clone()))
        .collect();
    for session in local {
        merged.insert(session.chat_id.clone(), session.clone());
    }
    let mut list: Vec<Session> = merged.into_values().collect();
    list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
    list
}

async fn workspace_task(weak: Weak<WorkspaceHostInner>, mut changed_rx: watch::Receiver<u64>) {
    let mut save_deadline = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() { break; }
                let Some(inner) = weak.upgrade() else { break };
                inner.publish();
                if save_deadline.is_none() { save_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS)); }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(inner) = weak.upgrade() else { break };
                inner.save_snapshot();
            }
        }
    }
}

fn device_name_on_boot(existing_name: Option<&str>, detected_name: &str) -> String {
    existing_name
        .filter(|name| {
            let name = name.trim();
            !name.is_empty() && name != crate::LEGACY_UNKNOWN_DEVICE_NAME
        })
        .unwrap_or(detected_name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{device_name_on_boot, linked_worktree_root};

    #[test]
    fn boot_repairs_the_legacy_unknown_device_sentinel() {
        assert_eq!(
            device_name_on_boot(Some("unknown-device"), "MacBook Pro"),
            "MacBook Pro"
        );
    }

    #[test]
    fn boot_preserves_a_user_selected_device_name() {
        assert_eq!(
            device_name_on_boot(Some("Work laptop"), "MacBook Pro"),
            "Work laptop"
        );
    }

    #[test]
    fn linked_worktree_resolves_to_the_checkout_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let wt = dir.path().join("clever-ember");
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("clever-ember")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                root.join(".git/worktrees/clever-ember").display()
            ),
        )
        .unwrap();
        assert_eq!(
            linked_worktree_root(&wt).as_deref(),
            Some(root.to_str().unwrap())
        );
    }

    #[test]
    fn primary_checkouts_and_plain_folders_resolve_to_none() {
        let dir = tempfile::tempdir().unwrap();
        // Primary checkout: `.git` is a directory.
        let primary = dir.path().join("primary");
        std::fs::create_dir_all(primary.join(".git")).unwrap();
        assert_eq!(linked_worktree_root(&primary), None);
        // Not a repo at all.
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(linked_worktree_root(&plain), None);
        // A `.git` file pointing somewhere that is not `<root>/.git/worktrees/<name>`.
        let odd = dir.path().join("odd");
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(odd.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        assert_eq!(linked_worktree_root(&odd), None);
    }
}
