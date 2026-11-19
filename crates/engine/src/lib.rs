//! zeron-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
pub use zeron_proto::{EngineInfo, HarnessId, WorkspaceScope};
use zeron_rpc::{RpcError, RpcReply, RpcService, methods};

use zeron_sync::DocsStore;

pub mod agent_accounts;
pub mod copilot;
pub use copilot::CopilotCredentialHolder;

/// Which build this engine is: its git commit and when its executable was
/// built (the file's modification time, in seconds), as
/// `"<commit>.<seconds>"`. Read once, as the engine starts: a binary replaced
/// on disk while it runs leaves its id as it was, so a client can compare it
/// with `zeron-engine --build-id` of the binary there now.
pub fn build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let built = std::env::current_exe()
            .and_then(std::fs::metadata)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        format!("{}.{built}", env!("ZERON_GIT_COMMIT"))
    })
}
pub mod auth;
mod change_request_store;
pub mod change_requests;
mod chat2_checkpoint;
pub mod chat2_host;
mod chat_persistence;
pub mod commands;
pub mod commit_credit;
pub mod deferred;
pub mod desktop_notify;
pub mod device_stats;
pub mod diff_sync;
pub mod doc_host;
mod fs_watch;
mod git_tree;
pub mod heap;
pub(crate) mod home_index;
mod http_error;
pub mod instance_lock;
pub mod keep_running;
pub mod managed;
mod plugin_agents;
pub mod plugins;
pub mod profile;
mod private_file;
pub mod project_actions;
pub mod registry;
pub mod remote_terminals;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod self_schedule;
pub mod sessions;
pub mod source_control;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod tool_output;
mod transcript_history;
mod update_trial;
pub mod uploads;
pub mod voice;
pub mod workspace_files;
pub mod workspace_host;
pub mod workspace_search;
mod worktree_settings;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{
    Auth, AuthConfig, AuthState, AuthUser, OrgMembership, ScreenHint, SignInOptions};
pub use change_requests::{ChangeRequestCacheKey, CheckoutChangeRequests};
pub use diff_sync::{
    CheckoutDiffSync, DiffFileTextPair, DiffSidecar, DiffSnapshot, TreeSnapshot, TurnSnapshot,
    capture_commit_diff, capture_diff, capture_diff_against, capture_turn_diff,
    discard_working_tree, merge_base, read_diff_file_text, snapshot_tree, working_diff_base,
};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use profile::EngineProfile;
pub use project_actions::ProjectActionsStore;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use source_control::{
    BranchHeadContext, ChangeRequestError, ChangeRequestProvider, ChangeRequestResolution,
    ChangeRequestResolver, CheckoutChangeRequestLookup, CheckoutSourceContext, GitHubCli,
    GitRemote, parse_git_remote,
};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_files::WorkspaceFiles;
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};
pub use workspace_search::WorkspaceSearch;
pub use zeron_preview::Scope as PreviewScope;

pub(crate) const LEGACY_UNKNOWN_DEVICE_NAME: &str = "unknown-device";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Token(#[from] zeron_rpc::TokenError),
    #[error("doc: {0}")]
    Doc(#[from] zeron_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] zeron_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] zeron_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.keiki`).
    pub data_dir: PathBuf,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub workspace_files: WorkspaceFiles,
    pub terminals: Terminals,
    /// Agents' shell commands on this device ([`commands`]).
    pub commands: commands::Commands,
    /// I/O routing for terminals on other devices (direct link or relay).
    pub remote_terminals: remote_terminals::RemoteTerminals,
    pub project_actions: ProjectActionsStore,
    pub previews: zeron_preview::PreviewService,
    pub change_requests: CheckoutChangeRequests,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub copilot_credentials: CopilotCredentialHolder,
    pub device_id: String,
    /// Which app windows are focused (desktop banners hold back while one is).
    pub viewport_focus: desktop_notify::ViewportFocus,
    /// Plugins under `{data}/plugins/` (the plugins contract).
    pub plugins: plugins::Plugins,
    workspace_scope: WorkspaceScope,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<zeron_rpc::LinkCache>>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<zeron_update::Updater>>,
    /// The updater's token-change wake forwarder — owned so shutdown can end it.
    updater_wake: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    /// This process's agents, recorded in the data dir so that whatever
    /// a killed engine leaves running is ended by the next one.
    _agent_ledger: zeron_harness::process::registry::Attachment,
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash in the local profile.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let profile = EngineProfile::local(data_dir)?;
        Self::assemble_with_profile(profile, registry, default_harness, edge)
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        _org_id: &str,
        _user_id: &str,
    ) -> Result<Self, EngineError> {
        let profile = EngineProfile::local(data_dir)?;
        Self::assemble_with_profile(profile, registry, default_harness, edge)
    }

    /// Assemble the engine against one resolved, immutable workspace profile.
    pub fn assemble_with_profile(
        profile: EngineProfile,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        if edge.is_some() {
            return Err(EngineError::Other("Remote workspaces are not supported".into()));
        }
        let data_dir = profile.device_root();
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        Self::assemble_with_profile_locked(profile, registry, default_harness, edge, lock)
    }

    /// Assemble against a pre-acquired [`InstanceLock`]. The headed app takes
    /// the lock before binding the IPC port so the listener owner and the
    /// data-dir owner cannot diverge when several viewports bootstrap at once.
    pub fn assemble_with_profile_locked(
        profile: EngineProfile,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        lock: InstanceLock,
    ) -> Result<Self, EngineError> {
        if edge.is_some() {
            return Err(EngineError::Other("Remote workspaces are not supported".into()));
        }
        let data_dir = profile.device_root();
        std::fs::create_dir_all(data_dir)?;
        update_trial::refuse_mid_update(data_dir)?;
        // Agents an engine on this data dir left running when it died (a
        // SIGKILL, a crash) are ended; ours are recorded for the next one
        // to do the same. Only with the data dir's lock held.
        let agent_ledger = zeron_harness::process::registry::attach(data_dir);
        zeron_harness::process::registry::sweep_in_background(data_dir);
        let device_id = load_or_create_device_id(data_dir)?;
        // This device's harness enablement (Settings → Providers) rides the
        // engine data dir — per-device, like the CLI installs it gates.
        let copilot_credentials = registry.copilot_credentials.clone();
        registry.load_prefs(data_dir);
        let store = Arc::new(DocsStore::open(profile.store_root())?);

        let journal = Arc::new(RunJournal::open(profile.store_root().join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(&device_id),
                platform: std::env::consts::OS.into(),
                org_id: profile.org_id().to_string(),
                user_id: profile.user_id().to_string(),
                edge: edge.clone(),
                local_only: matches!(profile.scope(), WorkspaceScope::Local),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.follow_chat_deletions(workspace.watch_chats());
        let plugins = plugins::Plugins::new(data_dir, &device_id);
        plugins.set_workspace(workspace.clone());
        // A service's `zeron.folders()`: where this device's chats and
        // projects are (the built-in Git service works in these alone).
        {
            let (workspace, device_id) = (workspace.clone(), device_id.clone());
            plugins.set_folders(move || {
                let chats = workspace.read_chats().unwrap_or_default();
                let spaces = workspace.read_spaces().unwrap_or_default();
                let mut folders: Vec<String> = chats
                    .into_iter()
                    .filter(|c| c.device_id == device_id)
                    .filter_map(|c| c.cwd)
                    .chain(spaces.into_iter().filter(|s| s.device_id == device_id).map(|s| s.path),
                    )
                    .filter(|f| !f.is_empty())
                    .collect();
                folders.sort();
                folders.dedup();
                folders
            });
        }
        // A plugin service's `zeron.notify`: as `PostNotification` posts.
        let viewport_focus = desktop_notify::ViewportFocus::default();
        {
            let (data_dir, focus) = (data_dir.to_path_buf(), viewport_focus.clone());
            plugins.set_notifier(move |source, title, body, session| {
                let title: String = title.lines().next().unwrap_or_default().trim().chars().take(120).collect();
                desktop_notify::post_for(&data_dir, &focus, source, &title, body, session).to_owned()
            });
        }
        sessions.set_plugins(plugins.clone());
        sessions.set_doc_host(doc_host.clone());
        // A new version on trial may yet be undone, and recovering clears the
        // journals' marks of what to resume, which a rollback doesn't restore.
        // It waits for the commit (`update_trial`), with all other new work.
        if zeron_update::transaction::in_trial() {
            doc_host.hold_work();
        } else {
            match sessions.recover_stale() {
                Ok(0) => {}
                Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
                Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
            }
            doc_host.boot_recovery_finished();
        }
        doc_host.spawn_transcript_salvage(profile.store_root().join("journals"));
        let repos = Repos::new(data_dir, &device_id);
        sessions.set_scratch_root(repos.scratch_root());
        doc_host.set_repos(repos.clone());
        let change_requests = CheckoutChangeRequests::start(repos.clone(), &device_id);
        let workspace_files = WorkspaceFiles::new(repos.clone(), workspace.clone(), device_id.clone());
        let terminals = Terminals::new();
        sessions.track_terminal_activity(terminals.clone(), workspace.clone());
        let commands = commands::Commands::default();
        // An agent's background commands end with its chat.
        commands.follow_chats(workspace.watch_chats());
        let project_actions = ProjectActionsStore::open(profile.store_root())?;
        doc_host.set_project_action_runtime(project_actions.clone(), terminals.clone());
        let previews = zeron_preview::PreviewService::new(
            profile.store_root().join("previews.json"),
            device_id.clone(),
            local_device_name(&device_id),
        )
        .map_err(|e| EngineError::Other(e.to_string()))?;
        previews.serve_streams(
            terminals::link::SERVICE_PREFIX,
            Arc::new(terminals::link::Service::new(terminals.clone())),
        );
        let uploads = Uploads::from_root_with_fallback(
            profile.uploads_root(),
            None,
        );
        // Queued-attachment support: the doc host resolves `pending://` refs
        // against this store and pushes staged bytes to remote hosts.
        doc_host.set_uploads(uploads.clone());
        let agent_accounts_config = AgentAccountsConfig::detect(data_dir);
        // Logins started from another device publish their callback port to
        // the P2P service, which serves it to that device alone.
        let agent_accounts =
            AgentAccounts::with_callback_routes(agent_accounts_config, previews.callback_routes());
        // Plugins' agent ops (usage, accounts, models: plugins/agents) reach
        // the engine as Settings and the composer do, and hear what changes.
        plugins.agents().set_host(Arc::new(plugin_agents::EngineAgentHost {
            accounts: agent_accounts.clone(),
            registry: registry.clone(),
            data_dir: repos.data_dir().to_path_buf(),
            workspace: workspace.clone(),
            sessions: sessions.clone(),
            doc_host: doc_host.clone(),
            default_harness,
        }));
        plugins.follow_agents(
            agent_accounts.watch_snapshots(),
            sessions.watch_sessions(),
            workspace.watch_chats(),
        );
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        // Turn starts snapshot the checkout tree — the "Latest turn" diff base.
        let turn_diff = diff_sync.clone();
        sessions.set_turn_listener(Arc::new(move |chat_id, cwd| {
            turn_diff.note_turn_start(chat_id, cwd);
        }));
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        plugins.start();
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            workspace_files,
            terminals,
            commands,
            remote_terminals: Default::default(),
            project_actions,
            previews,
            change_requests,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            copilot_credentials,
            device_id,
            plugins,
            workspace_scope: profile.scope(),
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            updater: std::sync::Mutex::new(None),
            updater_wake: std::sync::Mutex::new(None),
            viewport_focus,
            _agent_ledger: agent_ledger,
            _instance_lock: lock,
        })
    }

    pub fn workspace_scope(&self) -> WorkspaceScope {
        self.workspace_scope
    }

    /// When an update may restart this engine: never under a live run, an open
    /// terminal or a running command. On a Cloud machine, a message waiting counts too: nothing is
    /// running yet, and a restart would land just as its turn starts.
    pub fn update_quiescence(&self, cloud: bool) -> zeron_update::QuiescentCheck {
        let sessions = self.sessions.clone();
        let terminals = self.terminals.clone();
        let commands = self.commands.clone();
        let doc_host = cloud.then(|| self.doc_host.clone());
        Arc::new(move || {
            !sessions.any_active()
                && !terminals.any_open()
                && commands.running() == 0
                && !doc_host.as_ref().is_some_and(DocHost::has_waiting_commands)
        })
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        auth.attach_copilot_credentials(self.copilot_credentials.clone());
        self.plugins.attach_auth(auth.clone());
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached account, or a lazily-created Keiki account for in-process embeds.
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let auth = Auth::new(AuthConfig::new(keiki_api::api_url(), self.doc_host.store().path().to_path_buf()));
            auth.attach_copilot_credentials(self.copilot_credentials.clone());
            auth
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing,
    /// [`Self::dial_device`], and the doc host's queued-attachment transfers.
    pub fn set_links(&self, links: Arc<zeron_rpc::LinkCache>) {
        self.doc_host.set_links(links.clone());
        self.plugins.set_links(links.clone());
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<zeron_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater_wake(&self, handle: tokio::task::JoinHandle<()>) {
        *self
            .updater_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    pub fn set_updater(&self, updater: zeron_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<zeron_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<zeron_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> zeron_rpc::HostRelay {
        let auth = self.auth();
        let config =
            zeron_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));

        let doc_host = self.doc_host.clone();
        let on_nudge: zeron_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            match doc_host.enqueue_wakeup(&chat_id) {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!(chat = %chat_id, %err, "nudge: durable admission failed; withholding ACK");
                    false
                }
            }
        });
        let service: Arc<dyn RpcService> = self.rpc_service();

        zeron_rpc::HostRelay::spawn(config, service, on_nudge)
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.workspace_files.clone(),
            self.terminals.clone(),
            self.project_actions.clone(),
            self.change_requests.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
            self.workspace_scope,
        )
        .with_auth(self.auth())
        .with_previews(self.previews.clone())
        .with_remote_terminals(self.remote_terminals.clone())
        .with_commands(self.commands.clone())
        .with_copilot_credentials(self.copilot_credentials.clone())
        .with_viewport_focus(self.viewport_focus.clone())
        .with_plugins(self.plugins.clone());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }


        Arc::new(rpc)
    }

    /// Revoke every account-scoped transport before any slower graceful
    /// draining. Connected sockets remain authorized by their handshake, so
    /// clearing credentials alone is not a security boundary.
    pub fn disconnect_edge(&self) {
        self.previews.stop();
        if let Some(links) = self.links() {
            links.disconnect_all();
        }
        self.doc_host.disconnect_edge();
        self.workspace.disconnect_edge();
    }

    pub async fn shutdown(&self) {
        self.plugins.shutdown();
        self.previews.shutdown().await;
        // A run interruption transitions its chat to Idle, and Idle normally
        // releases the next queued row. Freeze first so quitting never starts
        // recovered work while the engine is being torn down.
        self.doc_host.pause_all_queues();
        // Decided here, from the auth state at the stop, so every way an
        // engine goes down (headless, background, in-process) agrees.
        let auth = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if auth.is_some_and(|auth| forgoes_revival(self.workspace_scope, &auth.state())) {
            self.sessions.forgo_revival();
        }
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.commands.shutdown().await;
        self.sessions.stop_activity_projection().await;
        self.agent_accounts.shutdown();
        self.change_requests.shutdown();
        // Cancel + await every worker that can reach Edge before flushing: a
        // replaced synced runtime must not keep polling releases or draining
        // the attachment outbox under the old identity after Local boots.
        let wake = self
            .updater_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(wake) = wake {
            wake.abort();
            let _ = wake.await;
        }
        let updater = self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(updater) = updater {
            updater.shutdown().await;
        }
        self.diff_sync.shutdown().await;
        self.workspace_files.shutdown().await;
        self.spaces_sync.shutdown().await;
        self.doc_host.shutdown_workers().await;
        self.doc_host.flush_all();
        self.workspace.shutdown();
        // Last: the WAL into the database file, so a stopped engine leaves one
        // self-contained file (a Cloud machine's disk is snapshotted next).
        self.doc_host.checkpoint_store();
        // Break the sessions ⇄ doc-host retain cycle so the replaced graph can
        // actually be freed once the last handle drops.
        self.sessions.clear_doc_host();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

/// A fully assembled identity-scoped engine plus the relay handle whose lifetime
/// keeps this device reachable. Used by both the headless server and the headed
/// in-process engine so their production authentication paths cannot diverge.
pub struct EngineRuntime {
    core: EngineCore,
    host_relay: std::sync::Mutex<Option<zeron_rpc::HostRelay>>,
    /// Desktop banners for this engine's sessions (desktop engines only).
    notifier: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// IPC-only lifecycle control owned by `zeron headless`. The regular
/// [`EngineRpc`] deliberately does not expose this method, so a viewport
/// attached to another headed process cannot shut down that process's engine.
struct HeadlessRpc {
    inner: Arc<dyn RpcService>,
    doc_host: DocHost,
    stop_tx: tokio::sync::mpsc::UnboundedSender<()>,
    data_dir: PathBuf,
}

#[async_trait]
impl RpcService for HeadlessRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == methods::QUIESCE_FOR_SLEEP {
            // Local IPC only (never relayed): a Cloud sleep's last check.
            return RpcReply::value(&match self.doc_host.quiesce_for_sleep().await {
                Ok(()) => {
                    // Asleep it can't make room: leave it fitting its size.
                    // Bounded, and never in the sleep's way.
                    // The provider snapshots the disk from outside once the
                    // engine stops: the docs written so far go into the store,
                    // and the store's WAL into its file, now. The stop does it
                    // again, and the stop command flushes the disk after.
                    let doc_host = self.doc_host.clone();
                    let checkpointed = tokio::task::spawn_blocking(move || {
                        doc_host.flush_all();
                        doc_host.checkpoint_store()
                    })
                    .await
                    .unwrap_or(false);
                    serde_json::json!({ "quiesced": true, "checkpointed": checkpointed })
                }
                Err(busy) => serde_json::json!({ "quiesced": false, "busy": busy }),
            });
        }
        if method != methods::STOP_ENGINE {
            return self.inner.handle(method, params).await;
        }

        let stop_tx = self.stop_tx.clone();
        // Let the unary success frame reach the client before `Engine::run`
        // aborts the IPC server and drains the runtime.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = stop_tx.send(());
        });
        RpcReply::value(&serde_json::json!({ "ok": true }))
    }
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub fn workspace_scope(&self) -> WorkspaceScope {
        self.core.workspace_scope()
    }

    pub fn disconnect_edge(&self) {
        // Revoke remote reachability before graceful draining. Sessions may
        // need time to settle; no authenticated relay RPC may enter during
        // that window after sign-out.
        self.host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.core.disconnect_edge();
    }

    pub async fn shutdown(&self) {
        self.stop_notifier();
        // While the edge is still reachable: a relaying chat's newest changes may
        // be only here, and a device opening it while this one is off would get
        // an older checkpoint.
        self.core
            .doc_host
            .checkpoint_relayed_chats_before_stop(std::time::Duration::from_secs(5))
            .await;
        self.disconnect_edge();
        self.core.shutdown().await;
    }

    fn stop_notifier(&self) {
        if let Some(task) = self
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

impl Drop for EngineRuntime {
    fn drop(&mut self) {
        self.stop_notifier();
        self.host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    pub async fn build_auth(config: &EngineConfig) -> Auth {
        Auth::new(AuthConfig::new(keiki_api::api_url(), config.data_dir.clone()))
    }

    pub fn initial_workspace_scope(_auth: &Auth) -> WorkspaceScope {
        WorkspaceScope::Local
    }

    pub fn resolve_profile(
        config: &EngineConfig,
        _auth: &Auth,
        scope: WorkspaceScope,
    ) -> Result<Option<EngineProfile>, EngineError> {
        if scope != WorkspaceScope::Local {
            return Err(EngineError::Other("Only the local workspace is supported".into()));
        }
        EngineProfile::local(&config.data_dir).map(Some)
    }


    /// Resolve the one-shot identity served before profile stores are available.
    pub fn engine_info(
        config: &EngineConfig,
        workspace_scope: WorkspaceScope,
    ) -> Result<EngineInfo, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        Ok(EngineInfo {
            device_id: load_or_create_device_id(&config.data_dir)?,
            workspace_scope,
            cursor_sdk_version: None,
            capabilities: {
                let mut capabilities = zeron_proto::capabilities::current();
                if desktop_notify::enabled() {
                    capabilities
                        .push(zeron_proto::capabilities::DESKTOP_NOTIFICATIONS_V1.to_string());
                }
                capabilities
            },
            build: Some(crate::build_id().to_owned()),
        })
    }

    /// Open one already-resolved profile. Synced profiles always keep their
    /// Edge supervisors alive; temporary token or network failures are runtime
    /// states, not a reason to permanently assemble an offline engine.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
    ) -> anyhow::Result<EngineRuntime> {
        Self::assemble_runtime_inner(config, auth, profile, None).await
    }

    /// Like [`Self::assemble_runtime`], but against an [`InstanceLock`] the
    /// caller already holds on the profile's device root (headed bootstrap
    /// acquires it before binding IPC).
    pub async fn assemble_runtime_with_lock(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
        lock: InstanceLock,
    ) -> anyhow::Result<EngineRuntime> {
        Self::assemble_runtime_inner(config, auth, profile, Some(lock)).await
    }

    async fn assemble_runtime_inner(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
        lock: Option<InstanceLock>,
    ) -> anyhow::Result<EngineRuntime> {
        let _ = auth.access_token().await;
        let registry = Arc::new(default_registry());
        auth.attach_copilot_credentials(registry.copilot_credentials.clone());
        let core = match lock {
            Some(lock) => EngineCore::assemble_with_profile_locked(
                profile,
                registry,
                config.default_harness,
                None,
                lock,
            )?,
            None => EngineCore::assemble_with_profile(
                profile,
                registry,
                config.default_harness,
                None,
            )?,
        };
        core.set_auth(auth.clone());
        core.registry.set_commit_credit(Arc::new(commit_credit::CommitCredit::open(&config.data_dir, false)));
        // The chats as last published (the list every view watches), not a
        // fresh parse of every chat row on each scan.
        let preview_chats = core.workspace.watch_chats();
        let preview_device = core.device_id.clone();
        let preview_terminals = core.terminals.clone();
        let preview_sessions = core.sessions.clone();
        let projects = Arc::new(move || zeron_preview::Scope {
            chats: preview_chats
                .borrow()
                .iter()
                .filter(|chat| chat.device_id == preview_device)
                .filter_map(|chat| {
                    Some((chat.id.clone(), std::path::PathBuf::from(chat.cwd.clone()?)))
                })
                .collect(),
            terminals: preview_terminals.chats(),
            reserved_ports: Default::default(),
            live: preview_sessions.active_count() > 0 || preview_terminals.any_open(),
        });
        // Discovery parks while nothing Zeron started is alive. A run
        // changing state (a turn starting) or a terminal opening is what can
        // start a server: wake it then. It keeps its cadence from there for
        // as long as the run, the terminal, or anything they started lives.
        // The chats it serves changing (one arriving here with its folder,
        // as the registry loads after a restart) also wakes it: a server
        // one of them left running belongs to it from then on.
        {
            let previews = core.previews.clone();
            let mut sessions = core.sessions.watch_sessions();
            let mut terminals = core.terminals.watch_opened();
            let mut chats = core.workspace.watch_chats();
            let device = core.device_id.clone();
            let folders = move |chats: &[zeron_proto::Chat]| -> Vec<(String, Option<String>)> {
                chats
                    .iter()
                    .filter(|chat| chat.device_id == device)
                    .map(|chat| (chat.id.clone(), chat.cwd.clone()))
                    .collect()
            };
            let mut served = folders(&chats.borrow_and_update());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        changed = sessions.changed() => if changed.is_err() { break },
                        changed = terminals.changed() => if changed.is_err() { break },
                        changed = chats.changed() => {
                            if changed.is_err() { break }
                            let now = folders(&chats.borrow_and_update());
                            if now == served { continue }
                            served = now;
                        }
                    }
                    previews.rescan();
                }
            });
        }
        core.previews.start(projects, None).await;
        zeron_update::channel::set_data_dir(&config.data_dir);
        if let Some(feed) = zeron_update::release_feed_url() {
            let updater = zeron_update::Updater::spawn(feed, Some(core.update_quiescence(false)));
            updater.follow_wakes(zeron_sync::wake::subscribe());
            core.set_updater(updater);
        }
        // Whatever restarts this engine into another version (an update, a
        // newer install, a channel switch's update), the store it leaves is
        // one every version reads.
        // (Held weakly: the hook is process-wide and outlives this graph;
        // a strong hold kept a shut-down engine's whole graph alive.)
        let version_change_docs = core.doc_host.downgrade();
        let version_change_registry = core.workspace.downgrade();
        zeron_update::set_before_restart(move || {
            // Only this process's live engine restarts it: with its graph
            // gone there is nothing left to prepare.
            let (Some(version_change_docs), Some(version_change_registry)) =
                (version_change_docs.upgrade(), version_change_registry.upgrade(),
            )
            else {
                return;
            };
            // No more deltas from here (a debounced registry save is whole
            // too), the registry saved whole (older builds don't read row
            // deltas), then every chat saved whole and the WAL folded in.
            version_change_docs.store().stop_snapshot_deltas();
            version_change_registry.flush();
            version_change_docs.prepare_for_version_change();
        });
        let notifier = desktop_notify::enabled()
            .then(|| desktop_notify::spawn(&core, config.data_dir.clone()));
        Ok(EngineRuntime {
            core,
            host_relay: std::sync::Mutex::new(None),
            notifier: std::sync::Mutex::new(notifier),
        })
    }

    /// Run until ctrl-c: auth (dev or WorkOS), sessions engine + doc host + command
    /// executor, IPC server, and — when edge+auth are ready — the device-room host
    /// relay + peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let mut config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        update_trial::boot(&config.data_dir)?;
        let auth = Self::build_auth(&config).await;
        let mut auth_state = auth.watch_state();
        let workspace_scope = Self::initial_workspace_scope(&auth);
        let mut profile = Self::resolve_profile(&config, &auth, workspace_scope)?;
        let _refresh_loop = auth.spawn_refresh_loop();

        // A captured cloud session without an organization must finish onboarding
        // before its profile can open. A clean signed-out install is local and never
        // enters the terminal sign-in flow.
        if workspace_scope == WorkspaceScope::Synced && profile.is_none() {
            terminal_sign_in(&auth).await?;
            profile = Self::resolve_profile(&config, &auth, workspace_scope)?;
        }
        let profile = profile
            .ok_or_else(|| EngineError::Other("synced workspace profile is not ready".into()))?;

        let runtime = Self::assemble_runtime(&config, auth, profile).await?;
        // The desktop app or `zeron update` may install a newer binary under
        // a running service; restart into it once no run or terminal is live.
        if let Some(updater) = runtime.core().updater() {
            updater.restart_when_superseded();
        }

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel();
        let service: Arc<dyn RpcService> = Arc::new(HeadlessRpc {
            inner: runtime.core().rpc_service(),
            doc_host: runtime.core().doc_host.clone(),
            stop_tx,
            data_dir: config.data_dir.clone(),
        });
        let server = serve_ipc(config.ipc_port, &config.data_dir, service).await?;
        // Only a port this process actually serves goes to agents: the
        // injected MCP server must dial back into THIS engine. (Its bearer
        // first: a run revived on boot waits on the port, then reads both.)
        runtime
            .core()
            .sessions
            .set_ipc_token_file(zeron_rpc::ipc_auth::agent_token_path(&config.data_dir));
        // Agents' Zeron tools come from this engine itself (before the port:
        // a run revived on boot waits on the port, then reads both).
        if let Err(err) = runtime
            .core()
            .sessions
            .serve_mcp_endpoint(runtime.core().rpc_service())
            .await
        {
            tracing::warn!(error = %err, "agent MCP endpoint not served; agents get `zeron mcp`");
        }
        runtime.core().sessions.set_ipc_port(config.ipc_port);
        update_trial::prove_and_commit(runtime.core(), &config);
        let corruption_watch = watch_store_corruption(
            runtime.core().doc_host.store().path().to_path_buf(),
            false,
        );

        tokio::select! {
            result = shutdown_signal() => {
                // Stopped from outside mid-trial (a sleep, a restart): not the
                // release's fault, and the record should say so.
                zeron_update::transaction::note_trial_interrupted();
                result?
            }
            requested = stop_rx.recv() => {
                if requested.is_some() {
                    tracing::info!("headless shutdown requested over IPC");
                }
            }
            _ = wait_for_signed_out(&mut auth_state), if workspace_scope == WorkspaceScope::Synced => {
                // Edge transports observe the same auth signal and close at
                // once. Leave a brief reply window for a SignOut RPC before
                // the localhost server itself is aborted.
                runtime.disconnect_edge();
                tracing::info!("headless authentication revoked; stopping synced runtime");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        tracing::info!("shutting down");
        corruption_watch.abort();
        server.abort();
        // No token refresh mid-exchange when the next engine takes over the
        // session file (a refresh token is single-use).
        runtime
            .core()
            .auth()
            .quiesce_refresh(std::time::Duration::from_secs(10))
            .await;
        runtime.shutdown().await;
        Ok(())
    }
}

/// A docs store that reports corruption while running can't be trusted with
/// another write, and every chat it holds is stuck behind it. Its next open
/// checks it in full and rebuilds it ([`zeron_sync::DocsStore::open`]). A
/// Cloud machine restarts at once for that (its unit starts it again in a
/// second): nobody is there to restart it, and a waiting message would wait for
/// good. Elsewhere the next start does it.
fn watch_store_corruption(store: PathBuf, restart: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if zeron_sync::corruption_seen() {
                zeron_sync::request_store_check(&store);
                if restart {
                    tracing::error!("the docs store is damaged; restarting to rebuild it");
                    std::process::exit(STORE_DAMAGED_EXIT);
                }
                tracing::error!("the docs store is damaged; it's rebuilt on the next start");
                return;
            }
        }
    })
}

/// A Cloud engine restarting to rebuild a damaged store.
const STORE_DAMAGED_EXIT: i32 = 70;

/// Whether a stopping engine's cut-off turns stay ended rather than revive on
/// the next boot ([`EngineCore::shutdown`] asks): a synced engine whose
/// account is signed out by the time it stops, whichever stop reached it
/// first — the auth watcher, or the app's StopEngine right after its SignOut.
/// Sign-out is the user ending the session, like a Stop. A local engine is
/// signed out as a matter of course.
pub(crate) fn forgoes_revival(scope: WorkspaceScope, auth: &AuthState) -> bool {
    scope == WorkspaceScope::Synced && matches!(auth, AuthState::SignedOut)
}

async fn wait_for_signed_out(state: &mut tokio::sync::watch::Receiver<AuthState>) {
    loop {
        if matches!(&*state.borrow(), AuthState::SignedOut) {
            return;
        }
        if state.changed().await.is_err() {
            return;
        }
    }
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
///
/// Every call mints a fresh bearer at `{data_dir}/ipc-token` (0600) that each
/// handshake must present ([`zeron_rpc::ipc_auth`]); viewports and the CLI
/// read it at dial time. Agents' `zeron mcp` servers get the agent-scoped
/// one (`{data_dir}/ipc-token-agent`), which reaches only their tools'
/// methods ([`zeron_rpc::agent_allowed`]). The caller owns `data_dir` (it holds the
/// [`InstanceLock`]), so rotating the file before binding cannot strand
/// another engine's clients — and a client that sees the port open always
/// finds the new token already published.
pub async fn serve_ipc(
    port: u16,
    data_dir: &Path,
    service: std::sync::Arc<dyn zeron_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let tokens = zeron_rpc::ipc_auth::issue_tokens(data_dir)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(zeron_rpc::serve_ws_listener_scoped(
        listener, service, tokens,
    )))
}

/// Block until the WorkOS session is signed in AND org-scoped. On a TTY, print the
/// headless (paste-code) sign-in URL, read the pasted `state.code` from stdin, and
/// run workspace onboarding (create / auto-join / numbered picker). Off a TTY this
/// errors immediately — a daemon under systemd/launchd must load the session that
/// `zeron login` persisted, never wait on a prompt nobody can see.
pub async fn terminal_sign_in(auth: &Auth) -> Result<(), EngineError> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    let mut org_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let state = state_rx.borrow().clone();
        match state {
            AuthState::SignedIn { user, org_id } => {
                tracing::info!(email = %user.email, org = org_id.as_deref().unwrap_or("<none>"),
                    "auth: session ready");
                break;
            }
            AuthState::NeedsOrganization { user } => {
                if !interactive {
                    // No reader tasks have been spawned on this path (both spawns
                    // are TTY-gated), so an early return leaks nothing.
                    return Err(EngineError::Other(format!(
                        "signed in as {} but no workspace is selected — run `zeron login` on this machine to pick one",
                        user.email
                    )));
                }
                if org_reader.is_none() {
                    // Workspace onboarding on the TTY (old zeron's
                    // `backend login` flow): create if none, auto-join a
                    // single membership, numbered picker otherwise.
                    println!("Signed in as {}.", user.email);
                    org_reader = Some(tokio::spawn(run_org_onboarding(auth.clone())));
                }
            }
            AuthState::SignedOut => {
                if !interactive {
                    return Err(EngineError::Other(
                        "not signed in — run `zeron login` on this machine first".into(),
                    ));
                }
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in().await?;
                    println!("Sign in to Keiki:\n\n  {url}\n");
                    println!("Complete sign-in in the browser. If needed, paste its final callback URL here.");
                    let auth = auth.clone();
                    stdin_reader = Some(tokio::spawn(async move {
                        loop {
                            let Some(line) = read_stdin_line().await else {
                                return;
                            };
                            let pasted = line.trim();
                            if pasted.is_empty() {
                                continue;
                            }
                            match auth.complete_sign_in(pasted).await {
                                Ok(()) => return,
                                Err(err) => println!("Sign-in failed: {err}"),
                            }
                        }
                    }));
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    if let Some(reader) = org_reader {
        reader.abort();
    }
    Ok(())
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// TTY workspace onboarding for an org-less session (ports old zeron's
/// `backend login` flow): no memberships → prompt a name and create; exactly
/// one → auto-join; several → numbered picker. Success flips the auth state to
/// `SignedIn`, which ends [`wait_for_sign_in`]'s wait (and aborts this task).
async fn run_org_onboarding(auth: Auth) {
    let orgs = match auth.list_orgs().await {
        Ok(orgs) => orgs,
        Err(err) => {
            println!(
                "Could not list workspaces ({err}) — create or select one from the Zeron UI to continue."
            );
            return;
        }
    };
    match orgs.len() {
        0 => {
            println!("No Keiki organizations yet. Create one at {} and sign in again.", auth.api_url());
        }
        1 => {
            let only = &orgs[0];
            println!("Joining workspace \"{}\"…", only.name);
            if let Err(err) = auth.select_org(&only.organization_id).await {
                println!("Joining workspace failed: {err}");
            }
        }
        _ => {
            println!("\nYour workspaces:");
            for (index, org) in orgs.iter().enumerate() {
                println!("  {}. {}", index + 1, org.name);
            }
            println!("Pick a workspace [1-{}]:", orgs.len());
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let choice = line
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|index| orgs.get(index));
                let Some(org) = choice else {
                    println!("Pick a workspace [1-{}]:", orgs.len());
                    continue;
                };
                match auth.select_org(&org.organization_id).await {
                    Ok(()) => return,
                    Err(err) => println!("Joining workspace failed: {err}"),
                }
            }
        }
    }
}

/// Best-effort human name for this device's registry row.
fn local_device_name(device_id: &str) -> String {
    select_local_device_name(
        [
            std::env::var("ZERON_DEVICE_NAME").ok(),
            native_friendly_device_name(),
            std::env::var("HOSTNAME").ok(),
            gethostname::gethostname().into_string().ok(),
            std::fs::read_to_string("/etc/hostname").ok(),
        ],
        device_id,
        std::env::consts::OS,
    )
}

fn select_local_device_name(
    candidates: impl IntoIterator<Item = Option<String>>,
    device_id: &str,
    platform: &str,
) -> String {
    candidates
        .into_iter()
        .flatten()
        .map(|name| name.trim().to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let platform = match platform {
                "macos" => "macOS",
                "windows" => "Windows",
                "linux" => "Linux",
                _ => "Local",
            };
            let short_id: String = device_id.chars().take(8).collect();
            format!("{platform} device {short_id}")
        })
}

#[cfg(target_os = "macos")]
fn native_friendly_device_name() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn native_friendly_device_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    return std::env::var("COMPUTERNAME").ok();

    #[cfg(not(target_os = "windows"))]
    None
}

#[cfg(all(test, windows))]
mod identity_lock_retry_tests {
    use super::*;

    /// A concurrent holder of the lock file (share_mode(0)) fails the open
    /// with ERROR_SHARING_VIOLATION, which Rust reports as
    /// ErrorKind::Uncategorized, not PermissionDenied. acquire must retry
    /// through that error until the holder releases; before the fix the
    /// retry loop never matched it and startup failed outright.
    #[test]
    fn acquire_retries_through_sharing_violations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-id.lock");

        // Hold the file exclusively for 50ms, then release. The retry loop
        // has a 200 x 5ms budget, so the timing is comfortable.
        {
            use std::os::windows::fs::OpenOptionsExt;
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&path)
                .unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                drop(holder);
            });
        }

        let lock = DeviceIdentityLock::acquire(dir.path());
        assert!(
            lock.is_ok(),
            "acquire did not retry through the sharing violation"
        );
    }
}

#[cfg(test)]
mod device_name_tests {
    use super::select_local_device_name;

    fn name(candidates: &[Option<&str>], device_id: &str, platform: &str) -> String {
        select_local_device_name(
            candidates
                .iter()
                .map(|candidate| candidate.map(str::to_string)),
            device_id,
            platform,
        )
    }

    #[test]
    fn explicit_override_wins_and_is_trimmed() {
        assert_eq!(
            name(
                &[Some("  Studio Mac  "), Some("system-host")],
                "17bc0aa2-rest",
                "macos"
            ),
            "Studio Mac"
        );
    }

    #[test]
    fn native_friendly_name_wins_over_hostnames() {
        assert_eq!(
            name(
                &[
                    None,
                    Some("MacBook Pro de Jose"),
                    None,
                    Some("MacBook-Pro.local"),
                ],
                "17bc0aa2-rest",
                "macos"
            ),
            "MacBook Pro de Jose"
        );
    }

    #[test]
    fn windows_computer_name_is_used_when_present() {
        assert_eq!(
            name(
                &[None, Some("DESKTOP-123"), Some("shell-host")],
                "17bc0aa2-rest",
                "windows"
            ),
            "DESKTOP-123"
        );
    }

    #[test]
    fn blank_candidates_are_ignored() {
        assert_eq!(
            name(
                &[Some("  "), None, Some("\n"), Some("linux-box")],
                "17bc0aa2-rest",
                "linux"
            ),
            "linux-box"
        );
    }

    #[test]
    fn final_fallback_is_platform_specific_and_distinct() {
        assert_eq!(
            name(&[None, Some(" ")], "17bc0aa2-rest", "linux"),
            "Linux device 17bc0aa2"
        );
    }
}

/// Trimmed env var or the given default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    // EngineInfo is resolved before the lifetime InstanceLock is acquired, so
    // identity creation and legacy repair need their own short critical section.
    // The OS releases this lock after a crash; unlike a create_new lockfile it
    // cannot strand an installation permanently.
    let _identity_lock = DeviceIdentityLock::acquire(data_dir)?;
    let path = data_dir.join("device-id");
    let recovering_empty = match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
        // Older releases used truncate+write. A crash between those operations
        // left a zero-byte file that is safe to replace with a fresh identity.
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };

    let id = new_id();
    let temp_path = data_dir.join(format!(
        ".device-id.tmp-{}-{}",
        std::process::id(),
        new_id()
    ));
    let write_result = (|| -> Result<(), EngineError> {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(id.as_bytes())?;
        temp.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    // Fresh installs use create-if-absent. Legacy empty files need an atomic
    // same-directory replacement on Unix; the Windows fallback runs under the
    // identity lock and remains recoverable if interrupted.
    let publish_result = if recovering_empty {
        match std::fs::read_to_string(&path) {
            Ok(id) if !id.trim().is_empty() => {
                let _ = std::fs::remove_file(&temp_path);
                return Ok(id.trim().to_string());
            }
            Ok(_) => replace_empty_device_id(&temp_path, &path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::hard_link(&temp_path, &path)
            }
            Err(err) => Err(err),
        }
    } else {
        std::fs::hard_link(&temp_path, &path)
    };
    let _ = std::fs::remove_file(&temp_path);
    match publish_result {
        Ok(()) => Ok(id),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let winner = std::fs::read_to_string(&path)?;
            if winner.trim().is_empty() {
                Err(EngineError::Other(format!(
                    "invalid device identity {}: file is empty",
                    path.display()
                )))
            } else {
                Ok(winner.trim().to_string())
            }
        }
        Err(err) => Err(err.into()),
    }
}

struct DeviceIdentityLock {
    _file: std::fs::File,
}

impl DeviceIdentityLock {
    fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("device-id.lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
            let mut retries = 200;
            let file = loop {
                match options.open(&path) {
                    Ok(file) => break file,
                    // share_mode(0) means a concurrent holder fails the open
                    // with ERROR_SHARING_VIOLATION (raw os error 32), which
                    // Rust maps to ErrorKind::Uncategorized, not
                    // PermissionDenied. Retry through both.
                    Err(err)
                        if (err.raw_os_error()
                            == Some(
                                windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32,
                            )
                            || err.kind() == std::io::ErrorKind::PermissionDenied)
                            && retries > 0 =>
                    {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(err) => return Err(err.into()),
                }
            };
            return Ok(Self { _file: file });
        }

        #[cfg(not(windows))]
        let file = options.open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    return Err(err.into());
                }
            }
        }

        #[cfg(not(windows))]
        Ok(Self { _file: file })
    }
}

fn replace_empty_device_id(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(temp_path, path)
    }
    #[cfg(not(unix))]
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        std::fs::hard_link(temp_path, path)
    }
}

#[cfg(test)]
mod revival_tests {
    use super::*;

    #[test]
    fn only_a_signed_out_synced_engine_forgoes_revival() {
        assert!(forgoes_revival(
            WorkspaceScope::Synced,
            &AuthState::SignedOut
        ));
        let user = AuthUser {
            id: "u".into(),
            email: "u@example.com".into(),
            name: None,
        };
        assert!(!forgoes_revival(
            WorkspaceScope::Synced,
            &AuthState::SignedIn {
                user: user.clone(),
                org_id: Some("o".into()),
            }
        ));
        assert!(!forgoes_revival(
            WorkspaceScope::Synced,
            &AuthState::NeedsOrganization { user }
        ));
        for scope in [WorkspaceScope::Local, WorkspaceScope::Development] {
            assert!(!forgoes_revival(scope, &AuthState::SignedOut));
        }
    }
}
