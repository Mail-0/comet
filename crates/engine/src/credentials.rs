//! GitHub-only credential broker for legacy Cloud checkouts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};
use serde::Deserialize;
use serde_json::{Value, json};

use zeron_proto::{GithubRepo, HarnessId, RunRequest, VaultProvider};

use crate::http_error::describe_http_error;
use crate::runner::{EdgeErrorBody, RunnerIdentity, write_private_atomic};

/// The one error every "no GitHub grant" path reports. The
/// `github_not_connected:` prefix is the machine-readable code (RPC errors
/// carry only a message); the rest is user-facing.
pub const GITHUB_NOT_CONNECTED: &str =
    "github_not_connected: GitHub isn't connected for this device — connect it in Settings → Cloud";

/// Refusals that mean this session's own GitHub login can't serve it (the
/// vault's messages): it stays on that login, so a run says so rather than
/// working on with no way to push.
const GITHUB_SESSION_REFUSALS: [&str; 3] = [
    "github_account_needs_reconnect:",
    "github_account_disconnected:",
    "github_account_no_access:",
];

/// The words for the user, when `reason` is such a refusal.
pub fn github_session_refusal(reason: &str) -> Option<String> {
    GITHUB_SESSION_REFUSALS.iter().find_map(|prefix| {
        let at = reason.find(prefix)?;
        Some(reason[at + prefix.len()..].trim().to_owned())
    })
}

/// A Cloud run of a provider with no login anywhere on the device.

/// A spawn uses a cached grant only while it has at least this much life left.
const GRANT_SLACK: Duration = Duration::from_secs(5 * 60);
/// The background refresher re-grants this far ahead of expiry.
const REFRESH_AHEAD: Duration = Duration::from_secs(10 * 60);
/// A "not connected" answer is reused this long before asking again.
const MISSING_TTL: Duration = Duration::from_secs(30);
/// A run re-asks the vault for a grant older than this, so switching the
/// account in Settings → Providers takes effect on the next turn (grants
/// otherwise live until shortly before they expire — hours).
const RUN_REVALIDATE: Duration = Duration::from_secs(30);
/// Background refresher cadence (wall-clock re-evaluation; sandboxes stop
/// and resume, which tokio's monotonic timers do not see).
const REFRESH_TICK: Duration = Duration::from_secs(60);
/// How often the refresher looks for a GitHub grant that isn't there yet.
const GITHUB_PROBE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Offline fallback only for grants that are still valid this much longer.
const OFFLINE_MIN_VALIDITY: Duration = Duration::from_secs(30);
const GITHUB_MAX_PAGES: usize = 10;
const HOSTS_MARKER: &str =
    "# Managed by Zeron Cloud (GitHub grant) — delete this line to manage gh yourself.";

/// `{data_dir}/cloud/git-credentials` — the git credential store.
pub fn git_credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cloud").join("git-credentials")
}

/// A vault grant (`POST /vault/{orgId}/grant` reply). `Debug` redacts secrets.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    #[serde(default)]
    pub provider: String,
    pub access_token: String,
    /// Unix ms.
    pub expires_at: i64,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    /// Claude: OAuth scopes of the access token.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Claude: `pro` / `max` / … as Claude Code records it.
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub generation: i64,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("provider", &self.provider)
            .field("expires_at", &self.expires_at)
            .field("account", &self.account)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Grant {
    fn remaining_ms(&self) -> i64 {
        self.expires_at - crate::now_ms()
    }
}

/// What a grant request resolved to.
#[derive(Debug, Clone)]
pub enum GrantOutcome {
    Granted(Grant),
    /// The vault holds no usable connection of this provider for this device.
    NotConnected(String),
    /// The vault could not be reached and no cached grant is still valid.
    Unavailable(String),
}

enum Cached {
    Granted { grant: Grant, at: Instant },
    Missing { at: Instant, reason: String },
}

/// Filesystem + endpoint knobs (env-resolved in production, explicit in tests).
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Engine data dir (`{data_dir}/cloud/…` holds the managed files).
    pub data_dir: PathBuf,
    /// The sandbox user's home: `git config --global` and `~/.config/gh`.
    pub home: PathBuf,
    /// GitHub REST base (`https://api.github.com`).
    pub github_api: String,
    /// The session repository as `owner/name`: `github` grants are minted
    /// for its owner's App installation. `None` = no GitHub grant at all.
    pub github_repo: Option<String>,
}

impl BrokerConfig {
    pub fn detect(data_dir: &Path) -> Self {
        let home = crate::repos::home_dir();
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            data_dir: data_dir.to_path_buf(),
            github_api: env("ZERON_GITHUB_API_URL")
                .unwrap_or_else(|| "https://api.github.com".into()),
            github_repo: crate::runner::session()
                .and_then(crate::runner::CloudSession::github_repo),
            home,
        }
    }
}

#[derive(Default)]
struct FileState {
    git_helper_configured: bool,
    last_github_probe: Option<Instant>,
}

struct Inner {
    config: BrokerConfig,
    identity: Arc<RunnerIdentity>,
    tokens: Arc<dyn zeron_rpc::TokenSource>,
    http: reqwest::Client,
    cache: Mutex<HashMap<VaultProvider, Cached>>,
    /// One grant request in flight per device: the vault's replay fence
    /// rejects a `ts` that isn't above the last accepted one, so concurrent
    /// signed requests could race each other into `stale`.
    grant_gate: tokio::sync::Mutex<()>,
    /// Serializes writes of the managed files.
    files: tokio::sync::Mutex<FileState>,
    /// The user's git identity, looked up once per GitHub login.
    git_identity: tokio::sync::Mutex<Option<(String, GitIdentity)>>,
    refresher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The runner's credential broker. Cheap to clone.
#[derive(Clone)]
pub struct CredentialBroker {
    inner: Arc<Inner>,
}

impl CredentialBroker {
    pub fn new(
        config: BrokerConfig,
        identity: Arc<RunnerIdentity>,
        tokens: Arc<dyn zeron_rpc::TokenSource>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                identity,
                tokens,
                http: crate::runner::http_client(),
                cache: Mutex::new(HashMap::new()),
                grant_gate: tokio::sync::Mutex::new(()),
                files: tokio::sync::Mutex::new(FileState::default()),
                git_identity: tokio::sync::Mutex::new(None),
                refresher: Mutex::new(None),
            }),
        }
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.inner.config
    }

    /// Start the background refresher (idempotent): primes the GitHub grant
    /// so terminals can `git push` right after boot, then keeps the GitHub
    /// files and any cached Codex grant fresh ahead of expiry.
    pub fn start(&self) {
        let mut slot = lock(&self.inner.refresher);
        if slot.is_some() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        *slot = Some(tokio::spawn(async move {
            let mut first = true;
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let broker = CredentialBroker { inner };
                if std::mem::take(&mut first) {
                    tokio::join!(broker.refresh_due(), broker.prime_for_first_run());
                } else {
                    broker.refresh_due().await;
                }
                drop(broker);
                tokio::time::sleep(REFRESH_TICK).await;
            }
        }));
    }

    /// What the first run after a boot asks for: fetched now, beside the
    /// engine's boot, so a machine woken by a message doesn't make that run
    /// wait on one round trip after another. A grant it finds cached within
    /// [`RUN_REVALIDATE`] is used as it is.
    async fn prime_for_first_run(&self) {
        if let Some(grant) = self.github_grant().await {
            let _ = self.git_identity(&grant).await;
        }
    }

    pub fn shutdown(&self) {
        if let Some(task) = lock(&self.inner.refresher).take() {
            task.abort();
        }
    }

    // -- grants -------------------------------------------------------------

    /// A grant valid for at least 5 more minutes (cached or fresh).
    pub async fn grant(&self, provider: VaultProvider) -> GrantOutcome {
        if provider != VaultProvider::Github {
            return GrantOutcome::NotConnected("Only GitHub grants are supported".into());
        }
        self.grant_within(provider, GRANT_SLACK, None).await
    }

    /// [`Self::grant`] for a run about to start: a cached grant older than
    /// [`RUN_REVALIDATE`] is checked with the vault first, so the run uses
    /// the provider's active account as it is now.
    async fn grant_for_run(&self, provider: VaultProvider) -> GrantOutcome {
        self.grant_within(provider, GRANT_SLACK, Some(RUN_REVALIDATE))
            .await
    }

    async fn grant_within(
        &self,
        provider: VaultProvider,
        min_validity: Duration,
        max_age: Option<Duration>,
    ) -> GrantOutcome {
        if let Some(hit) = self.cached(provider, min_validity, max_age) {
            return hit;
        }
        let _gate = self.inner.grant_gate.lock().await;
        if let Some(hit) = self.cached(provider, min_validity, max_age) {
            return hit;
        }
        match self.fetch(provider).await {
            GrantOutcome::Granted(grant) => {
                tracing::info!(
                    provider = provider.as_str(),
                    generation = grant.generation,
                    expires_in_s = grant.remaining_ms() / 1000,
                    "credentials: vault grant"
                );
                lock(&self.inner.cache).insert(
                    provider,
                    Cached::Granted {
                        grant: grant.clone(),
                        at: Instant::now(),
                    },
                );
                GrantOutcome::Granted(grant)
            }
            GrantOutcome::NotConnected(reason) => {
                tracing::info!(provider = provider.as_str(), %reason, "credentials: provider not connected");
                lock(&self.inner.cache).insert(
                    provider,
                    Cached::Missing {
                        at: Instant::now(),
                        reason: reason.clone(),
                    },
                );
                GrantOutcome::NotConnected(reason)
            }
            GrantOutcome::Unavailable(reason) => {
                // Offline fallback: a cached grant that is still valid keeps
                // working while the vault is unreachable.
                if let Some(Cached::Granted { grant, .. }) = lock(&self.inner.cache).get(&provider)
                    && grant.remaining_ms() > OFFLINE_MIN_VALIDITY.as_millis() as i64
                {
                    tracing::warn!(provider = provider.as_str(), %reason,
                        "credentials: vault unreachable; using the cached grant");
                    return GrantOutcome::Granted(grant.clone());
                }
                tracing::warn!(provider = provider.as_str(), %reason, "credentials: vault unreachable");
                GrantOutcome::Unavailable(reason)
            }
        }
    }

    fn cached(
        &self,
        provider: VaultProvider,
        min_validity: Duration,
        max_age: Option<Duration>,
    ) -> Option<GrantOutcome> {
        match lock(&self.inner.cache).get(&provider)? {
            Cached::Granted { grant, at }
                if grant.remaining_ms() > min_validity.as_millis() as i64
                    && max_age.is_none_or(|max| at.elapsed() < max) =>
            {
                Some(GrantOutcome::Granted(grant.clone()))
            }
            Cached::Missing { at, reason } if at.elapsed() < MISSING_TTL => {
                Some(GrantOutcome::NotConnected(reason.clone()))
            }
            _ => None,
        }
    }

    fn cached_grant(&self, provider: VaultProvider) -> Option<Grant> {
        match lock(&self.inner.cache).get(&provider)? {
            Cached::Granted { grant, .. } => Some(grant.clone()),
            Cached::Missing { .. } => None,
        }
    }

    async fn fetch(&self, provider: VaultProvider) -> GrantOutcome {
        // A GitHub grant is for the session repository's owner: without one
        // there is nothing to ask for.
        let repo = match provider {
            VaultProvider::Github => match self.inner.config.github_repo.as_deref() {
                Some(repo) => Some(repo),
                None => {
                    return GrantOutcome::NotConnected(
                        "this Cloud device has no GitHub repository".into(),
                    );
                }
            },
            _ => None,
        };
        let bearer = match self.inner.tokens.token().await {
            Ok(token) => token,
            Err(err) => return GrantOutcome::Unavailable(format!("no runner token: {err}")),
        };
        let identity = &self.inner.identity;
        let url = format!("{}/vault/{}/grant", identity.edge_url(), identity.org_id());
        for attempt in 0..2 {
            let body = identity.grant_request(provider.as_str(), repo);
            let res = match self
                .inner
                .http
                .post(&url)
                .bearer_auth(&bearer)
                .json(&body)
                .send()
                .await
            {
                Ok(res) => res,
                Err(err) => return GrantOutcome::Unavailable(describe_http_error(err)),
            };
            let status = res.status().as_u16();
            if res.status().is_success() {
                return match res.json::<Grant>().await {
                    Ok(grant) if !grant.access_token.is_empty() => GrantOutcome::Granted(grant),
                    Ok(_) => GrantOutcome::Unavailable("the vault returned an empty grant".into()),
                    Err(err) => GrantOutcome::Unavailable(format!(
                        "malformed grant: {}",
                        describe_http_error(err)
                    )),
                };
            }
            let body = EdgeErrorBody::read(res).await;
            // Replay fence (per device, across providers): our clock lagged
            // the last accepted ts (e.g. a restart). One retry, fresh ts.
            if body.error == "stale" && attempt == 0 {
                continue;
            }
            return classify_grant_failure(status, &body);
        }
        GrantOutcome::Unavailable("vault grant timestamp refused".into())
    }

    // -- run environment ----------------------------------------------------

    /// Fill `request.env` for a spawn of `harness` on this Cloud device.
    /// `Err` when the provider has no login here at all: the run fails at
    /// once, saying where to sign in, instead of the CLI retrying without
    /// credentials.
    pub async fn prepare(
        &self,
        _harness: HarnessId,
        request: &mut RunRequest,
    ) -> Result<(), String> {
        // GitHub: the credential store and hosts.yml (refreshed in place),
        // never the run's environment (fixed at spawn, outlived by the token).
        // The user's identity does go in the environment: it outranks any
        // git config, so an agent that sets one of its own (the App's bot,
        // which its token belongs to) still commits as the user. Asked again
        // before a run, so a login disconnected since the last one stops it.
        match self.grant_for_run(VaultProvider::Github).await {
            GrantOutcome::Granted(grant) => {
                self.sync_github_files(&grant).await;
                if let Some(identity) = self.git_identity(&grant).await {
                    for (key, value) in identity.env() {
                        request.env.insert(key.into(), value);
                    }
                }
            }
            GrantOutcome::NotConnected(reason) => {
                self.clear_github_files().await;
                if let Some(message) = github_session_refusal(&reason) {
                    return Err(message);
                }
            }
            GrantOutcome::Unavailable(_) => {}
        }
        Ok(())
    }

    /// Which credential a run of `harness` would use now, in [`Self::prepare`]'s
    /// order: the vault login, the sandbox's own, the vault API key. Keyed by
    /// the grant's generation, so another account (or a refreshed token) reads
    /// as different. `None` while the vault can't be reached: unknown is not
    /// a change.
    pub async fn credential(&self, _harness: HarnessId) -> Option<String> {
        None
    }

    /// The GitHub token (files synced), or `None` when not connected/unreachable.
    pub async fn github_token(&self) -> Option<String> {
        self.github_grant().await.map(|grant| grant.access_token)
    }

    /// The GitHub grant, its files synced (or cleared when not connected).
    async fn github_grant(&self) -> Option<Grant> {
        match self.grant(VaultProvider::Github).await {
            GrantOutcome::Granted(grant) => {
                self.sync_github_files(&grant).await;
                Some(grant)
            }
            GrantOutcome::NotConnected(_) => {
                self.clear_github_files().await;
                None
            }
            GrantOutcome::Unavailable(_) => None,
        }
    }

    /// Make sure git can authenticate to github.com before a clone (no-op
    /// without a grant: public repositories still clone). `Err` carries why
    /// there is no grant (e.g. the App isn't installed on the owner), for a
    /// clone that then fails.
    pub async fn ensure_git_credentials(&self) -> Result<(), String> {
        match self.grant(VaultProvider::Github).await {
            GrantOutcome::Granted(grant) => {
                self.sync_github_files(&grant).await;
                Ok(())
            }
            GrantOutcome::NotConnected(reason) => {
                self.clear_github_files().await;
                Err(github_session_refusal(&reason).unwrap_or(reason))
            }
            GrantOutcome::Unavailable(reason) => Err(reason),
        }
    }

    // -- GitHub files -------------------------------------------------------

    async fn sync_github_files(&self, grant: &Grant) {
        let mut files = self.inner.files.lock().await;
        let config = &self.inner.config;
        let store = git_credentials_path(&config.data_dir);
        let line = format!("https://x-access-token:{}@github.com\n", grant.access_token);
        // Rewritten when it differs (a new token, or git's own `erase` after
        // a rejected push emptied the store).
        if std::fs::read_to_string(&store).ok().as_deref() != Some(line.as_str())
            && let Err(err) = store
                .parent()
                .map_or(Ok(()), private_dir)
                .map_err(crate::EngineError::from)
                .and_then(|()| write_private_atomic(&store, line.as_bytes()))
        {
            tracing::warn!(error = %err, "credentials: failed to write git credentials");
        }
        if !files.git_helper_configured {
            files.git_helper_configured = configure_git_helper(&config.home, &store).await;
        }
        write_gh_hosts(&config.home, grant);
    }

    /// The user's git identity from the grant's GitHub login: their name and
    /// GitHub noreply address (`{id}+{login}@users.noreply.github.com`,
    /// which ties a commit to their account whatever their email settings).
    /// Looked up once per login; `None` until GitHub answers. Also stamped as
    /// the machine's global git identity for the user's own terminals, unless
    /// they set their own.
    async fn git_identity(&self, grant: &Grant) -> Option<GitIdentity> {
        let login = grant
            .account
            .as_deref()
            .map(str::trim)
            .filter(|login| is_github_login(login))?;
        let mut cached = self.inner.git_identity.lock().await;
        if let Some((known, identity)) = cached.as_ref()
            && known == login
        {
            return Some(identity.clone());
        }
        let api = self.inner.config.github_api.trim_end_matches('/');
        let user = match github_get(
            &self.inner.http,
            &format!("{api}/users/{login}"),
            &grant.access_token,
        )
        .await
        {
            Ok(user) => user,
            Err(err) => {
                let error = match err {
                    GithubError::Unauthorized => "unauthorized".to_string(),
                    GithubError::Other(error) => error,
                };
                tracing::warn!(%error, login, "credentials: GitHub user lookup failed");
                return None;
            }
        };
        let identity = GitIdentity::from_user(login, &user)?;
        configure_git_identity(&self.inner.config.home, &identity).await;
        *cached = Some((login.to_string(), identity.clone()));
        Some(identity)
    }

    async fn clear_github_files(&self) {
        let _files = self.inner.files.lock().await;
        let store = git_credentials_path(&self.inner.config.data_dir);
        if store.exists() {
            let _ = std::fs::remove_file(&store);
            tracing::info!("credentials: removed git credentials (GitHub not connected)");
        }
        let hosts = gh_hosts_path(&self.inner.config.home);
        if is_managed_hosts(&hosts) {
            let _ = std::fs::remove_file(&hosts);
        }
    }

    /// One background pass (the refresher runs it every minute): GitHub
    /// (probed every few minutes when absent, re-granted ~10 min before
    /// expiry) plus any cached Claude / Codex grant nearing expiry.
    pub async fn refresh_due(&self) {
        let github_due = match self.cached_grant(VaultProvider::Github) {
            Some(grant) => grant.remaining_ms() <= REFRESH_AHEAD.as_millis() as i64,
            None => {
                let mut files = self.inner.files.lock().await;
                let due = files
                    .last_github_probe
                    .is_none_or(|at| at.elapsed() >= GITHUB_PROBE_EVERY);
                if due {
                    files.last_github_probe = Some(Instant::now());
                }
                due
            }
        };
        if github_due {
            match self
                .grant_within(VaultProvider::Github, REFRESH_AHEAD, None)
                .await
            {
                GrantOutcome::Granted(grant) => self.sync_github_files(&grant).await,
                GrantOutcome::NotConnected(_) => self.clear_github_files().await,
                GrantOutcome::Unavailable(_) => {}
            }
        }
    }

    // -- GitHub repositories ------------------------------------------------

    /// `ListGithubRepos`: repositories the device's GitHub grant (an App
    /// installation token) can see — the installation's repositories —
    /// filtered by `query` (case-insensitive substring of `owner/name`),
    /// newest push first.
    pub async fn list_github_repos(&self, query: Option<&str>) -> Result<Vec<GithubRepo>, String> {
        let token = match self.grant(VaultProvider::Github).await {
            GrantOutcome::Granted(grant) => {
                self.sync_github_files(&grant).await;
                grant.access_token
            }
            GrantOutcome::NotConnected(_) => return Err(GITHUB_NOT_CONNECTED.into()),
            GrantOutcome::Unavailable(reason) => {
                return Err(format!("Couldn't reach the credential vault: {reason}"));
            }
        };
        let repos =
            match fetch_github_repos(&self.inner.http, &self.inner.config.github_api, &token).await
            {
                Ok(repos) => repos,
                Err(GithubError::Unauthorized) => {
                    // The grant is dead on GitHub's side: drop it so the next
                    // call re-grants instead of reusing it until expiry.
                    lock(&self.inner.cache).remove(&VaultProvider::Github);
                    return Err(
                    "GitHub rejected this device's token — reconnect GitHub in Settings → Cloud"
                        .into(),
                );
                }
                Err(GithubError::Other(message)) => return Err(message),
            };
        Ok(filter_and_sort_repos(repos, query))
    }
}

/// Vault failure → outcome. Only transient failures (502 `upstream`, 503
/// `unavailable`, 408/429, and the runner bearer itself being refused) keep a
/// cached grant alive; 403 (`device_unknown`/`device_revoked`/
/// `not_authorized`/`forbidden`/`disabled`), 409 `needs_reconnect`, a bad
/// signature, and either kill switch (`disabled`, even as a 503) drop it.
fn classify_grant_failure(status: u16, body: &EdgeErrorBody) -> GrantOutcome {
    let detail = body.describe();
    let error = body.error.as_str();
    if error == "disabled" || error == "bad_signature" {
        return GrantOutcome::NotConnected(detail);
    }
    if status >= 500 || matches!(status, 401 | 408 | 429) {
        return GrantOutcome::Unavailable(format!("vault grant failed ({status}): {detail}"));
    }
    GrantOutcome::NotConnected(detail)
}

#[async_trait::async_trait]
impl crate::registry::RunEnvironment for CredentialBroker {
    async fn prepare(
        &self,
        harness: HarnessId,
        _chat_id: Option<&str>,
        request: &mut RunRequest,
    ) -> Result<(), String> {
        CredentialBroker::prepare(self, harness, request).await
    }

    async fn credential(&self, harness: HarnessId, _chat_id: Option<&str>) -> Option<String> {
        CredentialBroker::credential(self, harness).await
    }
}

// ---------------------------------------------------------------------------
// Codex auth.json
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Claude Code credentials
// ---------------------------------------------------------------------------

/// Scrub every credential file a broker manages — a copied (forked /
/// template) runner identity is being discarded. User files, repositories,
/// and a native Claude login (real refresh token) are left alone.
pub fn discard_managed_credentials(config: &BrokerConfig) {
    if std::fs::remove_file(git_credentials_path(&config.data_dir)).is_ok() {
        tracing::info!("credentials: discarded previous git credentials");
    }
    let hosts = gh_hosts_path(&config.home);
    if is_managed_hosts(&hosts) {
        let _ = std::fs::remove_file(&hosts);
    }
}

// ---------------------------------------------------------------------------
// git + gh
// ---------------------------------------------------------------------------

/// `git config --global credential.https://github.com.helper "store --file=…"`
/// (only when it differs). `HOME` is pinned so the "global" config is the
/// broker's home. Returns whether the helper is in place.
/// Wire the store as git's helper for github.com. An empty helper comes
/// first: it resets the helper list, so a helper from the system config
/// (Apple git's `osxkeychain`, a distro's libsecret or cache) never also
/// stores — or prompts to store — the short-lived App token.
async fn configure_git_helper(home: &Path, store: &Path) -> bool {
    let key = "credential.https://github.com.helper";
    let value = format!("store --file=\"{}\"", store.display());
    let git = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    };
    let wanted = format!("\n{value}\n");
    if let Ok(out) = git(&["config", "--global", "--get-all", key])
        .output()
        .await
        && out.status.success()
        && String::from_utf8_lossy(&out.stdout) == wanted
    {
        return true;
    }
    let run = |args: Vec<String>| async move {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(&args).output().await
    };
    let steps = [
        vec![
            "config".into(),
            "--global".into(),
            "--replace-all".into(),
            key.into(),
            String::new(),
        ],
        vec![
            "config".into(),
            "--global".into(),
            "--add".into(),
            key.into(),
            value.clone(),
        ],
    ];
    for step in steps {
        match run(step).await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "credentials: git config failed");
                return false;
            }
            Err(err) => {
                tracing::warn!(error = %err, "credentials: git is not runnable");
                return false;
            }
        }
    }
    tracing::info!("credentials: git credential helper configured for github.com");
    true
}

fn gh_hosts_path(home: &Path) -> PathBuf {
    home.join(".config").join("gh").join("hosts.yml")
}

fn is_managed_hosts(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|raw| raw.lines().next() == Some(HOSTS_MARKER))
}

/// `~/.config/gh/hosts.yml` for interactive terminals — written only when
/// absent or already Zeron-managed (a user's own `gh auth login` wins).
fn write_gh_hosts(home: &Path, grant: &Grant) {
    let path = gh_hosts_path(home);
    if path.exists() && !is_managed_hosts(&path) {
        return;
    }
    let user = grant
        .account
        .clone()
        .filter(|login| !login.trim().is_empty() && !login.contains(['\n', ':', '"']))
        .unwrap_or_else(|| "x-access-token".into());
    let token = &grant.access_token;
    let body = format!(
        "{HOSTS_MARKER}\ngithub.com:\n    users:\n        {user}:\n            oauth_token: {token}\n    git_protocol: https\n    oauth_token: {token}\n    user: {user}\n"
    );
    if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return;
    }
    if let Some(dir) = path.parent()
        && let Err(err) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(error = %err, "credentials: cannot create the gh config dir");
        return;
    }
    if let Err(err) = write_private_atomic(&path, body.as_bytes()) {
        tracing::warn!(error = %err, "credentials: failed to write gh hosts.yml");
    }
}

/// Who a Cloud machine's commits are by: the signed-in GitHub user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIdentity {
    pub name: String,
    pub email: String,
}

impl GitIdentity {
    /// From `GET /users/{login}`: the profile name (else the login) and the
    /// noreply address keyed by the account id.
    fn from_user(login: &str, user: &Value) -> Option<Self> {
        let id = user["id"].as_u64()?;
        let login = user["login"]
            .as_str()
            .filter(|l| is_github_login(l))
            .unwrap_or(login);
        let name = user["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty() && !name.contains(['\n', '<', '>']))
            .unwrap_or(login);
        Some(Self {
            name: name.to_string(),
            email: format!("{id}+{login}@users.noreply.github.com"),
        })
    }

    fn env(&self) -> [(&'static str, String); 4] {
        [
            ("GIT_AUTHOR_NAME", self.name.clone()),
            ("GIT_AUTHOR_EMAIL", self.email.clone()),
            ("GIT_COMMITTER_NAME", self.name.clone()),
            ("GIT_COMMITTER_EMAIL", self.email.clone()),
        ]
    }
}

fn is_github_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The machine's global git identity, for the user's own terminals: set
/// when there is none, or when it's a bot's (an agent that configured the
/// App it pushes as). An identity the user chose stays.
async fn configure_git_identity(home: &Path, identity: &GitIdentity) {
    // `git config --global` writes $XDG_CONFIG_HOME/git/config when that file
    // exists. An XDG_CONFIG_HOME outside `home` belongs to another home (this
    // process's own, when `home` is a sandbox or a test directory), so `home`'s is
    // used instead; otherwise the identity lands in that other user's config.
    // When `home` is this process's own, its XDG_CONFIG_HOME is the user's
    // wherever it lives: replacing it would hide an identity kept there, and
    // ~/.gitconfig (which wins over it) would get the machine's.
    let own_home = std::env::var_os("HOME").is_some_and(|own| Path::new(&own) == home);
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|dir| own_home || dir.starts_with(home))
        .unwrap_or_else(|| home.join(".config"));
    let git = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        cmd
    };
    let current = match git(&["config", "--global", "user.email"]).output().await {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => return,
    };
    if !current.is_empty() && !current.contains("[bot]") {
        return;
    }
    for (key, value) in [
        ("user.name", &identity.name),
        ("user.email", &identity.email),
    ] {
        if let Err(err) = git(&["config", "--global", key, value]).output().await {
            tracing::warn!(error = %err, "credentials: setting the git identity failed");
            return;
        }
    }
}

/// `dir` created and made owner-only (0700 on Unix).
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GitHub REST
// ---------------------------------------------------------------------------

enum GithubError {
    Unauthorized,
    Other(String),
}

async fn github_get(http: &reqwest::Client, url: &str, token: &str) -> Result<Value, GithubError> {
    let res = http
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|err| {
            GithubError::Other(format!(
                "GitHub is unreachable: {}",
                describe_http_error(err)
            ))
        })?;
    let status = res.status().as_u16();
    if status == 401 {
        return Err(GithubError::Unauthorized);
    }
    if !res.status().is_success() {
        return Err(GithubError::Other(format!(
            "GitHub request failed ({status})"
        )));
    }
    res.json::<Value>().await.map_err(|err| {
        GithubError::Other(format!(
            "malformed GitHub reply: {}",
            describe_http_error(err)
        ))
    })
}

/// Paginate `{url}&page=N` (100 per page) collecting `items(reply)`.
async fn github_pages(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    items: impl Fn(&Value) -> Vec<Value>,
) -> Result<Vec<Value>, GithubError> {
    let mut out = Vec::new();
    for page in 1..=GITHUB_MAX_PAGES {
        let reply = github_get(http, &format!("{url}&page={page}"), token).await?;
        let batch = items(&reply);
        let full = batch.len() >= 100;
        out.extend(batch);
        if !full {
            break;
        }
    }
    Ok(out)
}

/// An installation token sees exactly its installation's repositories.
async fn fetch_github_repos(
    http: &reqwest::Client,
    api: &str,
    token: &str,
) -> Result<Vec<GithubRepo>, GithubError> {
    let api = api.trim_end_matches('/');
    let raw = github_pages(
        http,
        &format!("{api}/installation/repositories?per_page=100"),
        token,
        |reply| {
            reply["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        },
    )
    .await?;
    Ok(raw.iter().filter_map(parse_github_repo).collect())
}

fn parse_github_repo(value: &Value) -> Option<GithubRepo> {
    let full_name = value["full_name"].as_str()?.to_owned();
    Some(GithubRepo {
        clone_url: value["clone_url"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://github.com/{full_name}.git")),
        default_branch: value["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_owned(),
        private: value["private"].as_bool().unwrap_or(false),
        description: value["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .map(str::to_owned),
        pushed_at: value["pushed_at"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis())
            .unwrap_or(0),
        full_name,
        ..Default::default()
    })
}

/// Dedupe by `owner/name`, keep `query` matches (case-insensitive substring),
/// newest push first.
pub fn filter_and_sort_repos(repos: Vec<GithubRepo>, query: Option<&str>) -> Vec<GithubRepo> {
    let needle = query
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<GithubRepo> = repos
        .into_iter()
        .filter(|repo| seen.insert(repo.full_name.to_lowercase()))
        .filter(|repo| {
            needle
                .as_deref()
                .is_none_or(|needle| repo.full_name.to_lowercase().contains(needle))
        })
        .collect();
    out.sort_by(|a, b| {
        b.pushed_at
            .cmp(&a.pushed_at)
            .then_with(|| a.full_name.cmp(&b.full_name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::{GitIdentity, configure_git_identity};

    #[tokio::test]
    async fn the_machine_identity_never_overrides_the_users_own() {
        let home = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            // XDG_CONFIG_HOME too: with the developer's set, `--global` is their real config.
            let out = std::process::Command::new("git")
                .args(args)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let user = GitIdentity {
            name: "Octo Cat".into(),
            email: "583231+octo@users.noreply.github.com".into(),
        };
        // A bot identity an agent configured is replaced.
        git(&[
            "config",
            "--global",
            "user.email",
            "346184242+zeronsh[bot]@users.noreply.github.com",
        ]);
        configure_git_identity(home.path(), &user).await;
        assert_eq!(git(&["config", "--global", "user.email"]), user.email);
        // One the user chose stays.
        git(&["config", "--global", "user.email", "me@example.com"]);
        configure_git_identity(home.path(), &user).await;
        assert_eq!(git(&["config", "--global", "user.email"]), "me@example.com");
    }

    #[test]
    fn identity_comes_from_the_github_profile() {
        let user = serde_json::json!({"login": "octo", "id": 7, "name": "  "});
        let identity = GitIdentity::from_user("octo", &user).unwrap();
        assert_eq!(
            identity.name, "octo",
            "a blank name falls back to the login"
        );
        assert_eq!(identity.email, "7+octo@users.noreply.github.com");
        assert!(GitIdentity::from_user("octo", &serde_json::json!({"login": "octo"})).is_none());
    }
    use super::*;

    fn grant(access_token: &str) -> Grant {
        Grant {
            provider: "github".into(),
            access_token: access_token.into(),
            expires_at: crate::now_ms() + 3_600_000,
            account_id: Some("acct_1".into()),
            id_token: Some("h.p.s".into()),
            account: Some("u@example.com".into()),
            scopes: None,
            subscription_type: None,
            generation: 3,
        }
    }

    #[test]
    fn vault_failures_classify_per_the_contract() {
        let body = |error: &str| EdgeErrorBody {
            error: error.into(),
            message: String::new(),
        };
        let not_connected = |status, error: &str| {
            matches!(
                classify_grant_failure(status, &body(error)),
                GrantOutcome::NotConnected(_)
            )
        };
        let unavailable = |status, error: &str| {
            matches!(
                classify_grant_failure(status, &body(error)),
                GrantOutcome::Unavailable(_)
            )
        };
        for error in [
            "device_unknown",
            "device_revoked",
            "not_authorized",
            "forbidden",
            "disabled",
        ] {
            assert!(not_connected(403, error), "{error}");
        }
        assert!(not_connected(409, "needs_reconnect"));
        assert!(not_connected(404, "not_found"));
        assert!(not_connected(401, "bad_signature"));
        // The global kill switch drops cached grants too.
        assert!(not_connected(503, "disabled"));
        assert!(unavailable(502, "upstream"));
        assert!(unavailable(503, "unavailable"));
        assert!(unavailable(429, ""));
        assert!(unavailable(401, "stale"));
        assert!(unavailable(401, "unauthorized"));
    }

    #[test]
    fn grant_debug_never_prints_secrets() {
        let printed = format!("{:?}", grant("super-secret-access"));
        assert!(!printed.contains("super-secret-access"));
        assert!(!printed.contains("h.p.s"));
    }

    #[test]
    fn repos_dedupe_filter_and_sort_newest_first() {
        let repo = |name: &str, pushed_at: i64| GithubRepo {
            full_name: name.into(),
            clone_url: format!("https://github.com/{name}.git"),
            default_branch: "main".into(),
            private: false,
            description: None,
            pushed_at,
            ..Default::default()
        };
        let repos = vec![
            repo("acme/old", 1),
            repo("acme/New-App", 30),
            repo("other/thing", 20),
            repo("acme/new-app", 30),
        ];
        let all = filter_and_sort_repos(repos.clone(), None);
        assert_eq!(
            all.iter().map(|r| r.full_name.as_str()).collect::<Vec<_>>(),
            ["acme/New-App", "other/thing", "acme/old"]
        );
        let filtered = filter_and_sort_repos(repos, Some(" ACME/ "));
        assert_eq!(
            filtered
                .iter()
                .map(|r| r.full_name.as_str())
                .collect::<Vec<_>>(),
            ["acme/New-App", "acme/old"]
        );
    }

    #[test]
    fn github_repo_parsing_tolerates_missing_fields() {
        let parsed = parse_github_repo(&json!({
            "full_name": "acme/app",
            "private": true,
            "description": "",
            "pushed_at": "2026-09-01T00:00:00Z",
        }))
        .unwrap();
        assert_eq!(parsed.clone_url, "https://github.com/acme/app.git");
        assert_eq!(parsed.default_branch, "main");
        assert!(parsed.private);
        assert_eq!(parsed.description, None);
        assert!(parsed.pushed_at > 0);
        assert!(parse_github_repo(&json!({"name": "x"})).is_none());
    }
}
