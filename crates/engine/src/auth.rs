//! Keiki OAuth account and credential lifecycle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use keiki_api::{AuthorizationFlow, Client, SessionResponse, StoredCredentials, TokenSet};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zeron_rpc::TokenError;

use crate::{CopilotCredentialHolder, EngineError};

async fn request<T>(
    future: impl std::future::Future<Output = Result<T, keiki_api::Error>>,
) -> Result<T, keiki_api::Error> {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| keiki_api::Error::Local("Keiki request timed out".into()))?
}

const SCOPE: &str = "manage offline_access";
const KEYRING_SERVICE: &str = "keiki://oauth";
const KEYRING_ACCOUNT: &str = "OAuth";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub api_url: String,
    pub data_dir: PathBuf,
}

impl AuthConfig {
    pub fn new(api_url: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_url: api_url.into(),
            data_dir: data_dir.into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedCredentials {
    #[serde(flatten)]
    credentials: StoredCredentials,
    #[serde(default)]
    issuer: Option<String>,
}

trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<SavedCredentials>, String>;
    fn save(&self, credentials: &SavedCredentials) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

struct KeyringStore;

impl KeyringStore {
    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|_| "OS keyring is unavailable".into())
    }
}

impl CredentialStore for KeyringStore {
    fn load(&self) -> Result<Option<SavedCredentials>, String> {
        match self.entry()?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| "Invalid Keiki keyring entry".into()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err("Could not read Keiki credentials from the OS keyring".into()),
        }
    }

    fn save(&self, credentials: &SavedCredentials) -> Result<(), String> {
        let value =
            serde_json::to_string(credentials).map_err(|_| "Could not encode Keiki credentials")?;
        self.entry()?
            .set_password(&value)
            .map_err(|_| "Could not save Keiki credentials in the OS keyring".into())
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err("Could not remove Keiki credentials from the OS keyring".into()),
        }
    }
}

struct Access {
    tokens: TokenSet,
    received: SystemTime,
    lifetime: Duration,
}

impl Access {
    fn fresh(&self) -> bool {
        !self.tokens.should_refresh()
            && self
                .received
                .elapsed()
                .unwrap_or(Duration::MAX)
                .saturating_add(Duration::from_secs(60))
                < self.lifetime
    }

    fn expires_at(&self) -> Option<i64> {
        self.received
            .checked_add(self.lifetime)?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()
    }
}

#[derive(Default)]
struct Session {
    generation: u64,
    saved: Option<SavedCredentials>,
    access: Option<Access>,
    pending: Option<AuthorizationFlow>,
    holders: Vec<CopilotCredentialHolder>,
    stopped: bool,
}

struct AuthInner {
    client: Client,
    store: Arc<dyn CredentialStore>,
    session: Mutex<Session>,
    refresh: tokio::sync::Mutex<()>,
    state: watch::Sender<AuthState>,
    changes: watch::Sender<u64>,
}

#[derive(Clone)]
pub struct Auth {
    inner: Arc<AuthInner>,
}

impl Auth {
    pub fn new(config: AuthConfig) -> Self {
        Self::with_store(config, Arc::new(KeyringStore))
    }

    fn with_store(config: AuthConfig, store: Arc<dyn CredentialStore>) -> Self {
        let client = Client::new(config.api_url);
        let saved = match store.load() {
            Ok(saved) => saved.filter(|saved| {
                saved
                    .issuer
                    .as_deref()
                    .unwrap_or(keiki_api::DEFAULT_API_URL)
                    .trim_end_matches('/')
                    == client.base_url()
            }),
            Err(error) => {
                tracing::warn!(%error, "Keiki credentials could not be restored");
                None
            }
        };
        Self {
            inner: Arc::new(AuthInner {
                client,
                store,
                session: Mutex::new(Session {
                    saved,
                    ..Default::default()
                }),
                refresh: tokio::sync::Mutex::new(()),
                state: watch::channel(AuthState::SignedOut).0,
                changes: watch::channel(0).0,
            }),
        }
    }

    pub async fn detect(config: AuthConfig) -> Self {
        Self::new(config)
    }

    pub fn watch_state(&self) -> watch::Receiver<AuthState> {
        self.inner.state.subscribe()
    }
    pub fn state(&self) -> AuthState {
        self.inner.state.borrow().clone()
    }
    pub fn user_id(&self) -> Option<String> {
        self.state().user().map(|user| user.id.clone())
    }
    pub fn api_url(&self) -> &str {
        self.inner.client.base_url()
    }

    pub fn attach_copilot_credentials(&self, holder: CopilotCredentialHolder) {
        let mut session = lock(&self.inner.session);
        if session.holders.iter().any(|existing| existing.same_holder(&holder)) {
            return;
        }
        if let Some(access) = &session.access {
            holder.set(zeron_copilot::CopilotCredentials {
                base_url: self.api_url().into(),
                access_token: access.tokens.access_token().into(),
            });
        } else {
            holder.clear();
        }
        session.holders.push(holder);
    }

    fn clear_session(&self, generation: Option<u64>) -> Result<(), EngineError> {
        let mut session = lock(&self.inner.session);
        if generation.is_some_and(|generation| generation != session.generation) {
            return Ok(());
        }
        self.clear_locked(&mut session)
    }

    fn clear_locked(&self, session: &mut Session) -> Result<(), EngineError> {
        session.generation = session.generation.wrapping_add(1);
        session.saved = None;
        session.access = None;
        session.pending = None;
        for holder in &session.holders {
            holder.clear();
        }
        self.inner.state.send_replace(AuthState::SignedOut);
        self.inner
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
        self.inner.store.clear().map_err(EngineError::Other)
    }

    async fn account(&self, token: &str, generation: u64) -> Result<SessionResponse, EngineError> {
        match request(self.inner.client.session(token)).await {
            Ok(account)
                if account
                    .user
                    .email
                    .as_deref()
                    .is_some_and(|email| !email.trim().is_empty()) =>
            {
                Ok(account)
            }
            Ok(_) => {
                self.clear_session(Some(generation))?;
                Err(EngineError::Other(
                    "Keiki session has no email; sign in again".into(),
                ))
            }
            Err(error) => {
                if error.is_authentication_failure() {
                    self.clear_session(Some(generation))?;
                }
                Err(EngineError::Other(error.to_string()))
            }
        }
    }

    fn save_refresh(
        &self,
        tokens: &TokenSet,
        client_id: String,
        generation: u64,
    ) -> Result<(), EngineError> {
        let saved = SavedCredentials {
            credentials: tokens.stored_credentials(client_id),
            issuer: Some(self.api_url().into()),
        };
        let mut session = lock(&self.inner.session);
        if generation != session.generation {
            return Err(EngineError::Other("Keiki sign-in was cancelled".into()));
        }
        session.saved = Some(saved.clone());
        self.inner.store.save(&saved).map_err(EngineError::Other)
    }

    fn publish(
        &self,
        tokens: TokenSet,
        client_id: String,
        account: SessionResponse,
        generation: u64,
    ) -> Result<(), EngineError> {
        let mut session = lock(&self.inner.session);
        if session.generation != generation {
            return Err(EngineError::Other("Keiki sign-in was cancelled".into()));
        }
        let saved = SavedCredentials {
            credentials: tokens.stored_credentials(client_id),
            issuer: Some(self.api_url().into()),
        };
        self.inner.store.save(&saved).map_err(EngineError::Other)?;
        let email = account
            .user
            .email
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let user = AuthUser {
            id: email.clone(),
            email,
            name: account.user.name,
        };
        let state = match account.user.active_org_id.or(account.user.org_id) {
            Some(org) => AuthState::SignedIn {
                user,
                org_id: Some(org),
            },
            None => AuthState::NeedsOrganization { user },
        };
        for holder in &session.holders {
            holder.set(zeron_copilot::CopilotCredentials {
                base_url: self.api_url().into(),
                access_token: tokens.access_token().into(),
            });
        }
        let lifetime = tokens.expires_in();
        session.access = Some(Access {
            tokens,
            received: SystemTime::now(),
            lifetime,
        });
        session.saved = Some(saved);
        self.inner.state.send_replace(state);
        self.inner
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(())
    }

    pub async fn access_token(&self) -> Result<String, TokenError> {
        let _refresh = self.inner.refresh.lock().await;
        let (generation, saved) = {
            let session = lock(&self.inner.session);
            if session.stopped {
                return Err(TokenError::SignedOut);
            }
            if let Some(access) = session.access.as_ref().filter(|access| access.fresh()) {
                return Ok(access.tokens.access_token().to_owned());
            }
            (
                session.generation,
                session.saved.clone().ok_or(TokenError::SignedOut)?,
            )
        };
        let tokens = match request(self.inner.client.refresh_token(&saved.credentials)).await {
            Ok(tokens) => tokens,
            Err(error) => {
                if error.is_invalid_refresh_token() || error.is_authentication_failure() {
                    let _ = self.clear_session(Some(generation));
                    return Err(TokenError::SignedOut);
                }
                return Err(TokenError::TemporarilyUnavailable(error.to_string()));
            }
        };
        self.save_refresh(&tokens, saved.credentials.client_id.clone(), generation)
            .map_err(|error| TokenError::TemporarilyUnavailable(error.to_string()))?;
        let account = self
            .account(tokens.access_token(), generation)
            .await
            .map_err(|error| {
                if lock(&self.inner.session).saved.is_none() {
                    TokenError::SignedOut
                } else {
                    TokenError::TemporarilyUnavailable(error.to_string())
                }
            })?;
        self.publish(tokens, saved.credentials.client_id, account, generation)
            .map_err(|error| TokenError::TemporarilyUnavailable(error.to_string()))?;
        lock(&self.inner.session)
            .access
            .as_ref()
            .map(|access| access.tokens.access_token().to_owned())
            .ok_or(TokenError::SignedOut)
    }

    pub async fn access_token_with_expiry(&self) -> Result<(String, Option<i64>), TokenError> {
        let token = self.access_token().await?;
        let session = lock(&self.inner.session);
        let access = session.access.as_ref().ok_or(TokenError::SignedOut)?;
        if access.tokens.access_token() != token {
            return Err(TokenError::SignedOut);
        }
        Ok((token, access.expires_at()))
    }

    pub async fn start_sign_in(&self) -> Result<String, EngineError> {
        let generation = {
            let mut session = lock(&self.inner.session);
            session.generation = session.generation.wrapping_add(1);
            session.pending = None;
            session.generation
        };
        let (listener, redirect) = keiki_api::bind_loopback_listener()
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        request(self.inner.client.discover_oauth(SCOPE))
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let client_id = request(self.inner.client.register_client(&redirect))
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let flow = AuthorizationFlow::new(client_id, redirect.clone());
        let url = self
            .inner
            .client
            .authorization_url(&flow, SCOPE)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        {
            let mut session = lock(&self.inner.session);
            if generation != session.generation {
                return Err(EngineError::Other("Keiki sign-in was cancelled".into()));
            }
            session.pending = Some(flow);
        }
        let auth = self.clone();
        tokio::spawn(async move {
            match keiki_api::wait_for_loopback_callback(listener, redirect).await {
                Ok(callback) => {
                    if let Err(error) = auth.complete_generation(&callback, generation).await {
                        tracing::warn!(%error, "Keiki sign-in failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "Keiki callback failed");
                }
            }
        });
        Ok(url.to_string())
    }

    pub async fn start_sign_in_with(&self, _options: SignInOptions) -> Result<String, EngineError> {
        self.start_sign_in().await
    }
    pub async fn start_headless_sign_in(&self) -> Result<String, EngineError> {
        self.start_sign_in().await
    }

    async fn complete_generation(
        &self,
        callback: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        let _refresh = self.inner.refresh.lock().await;
        let flow = {
            let session = lock(&self.inner.session);
            if session.generation != generation {
                return Err(EngineError::Other("Keiki sign-in was cancelled".into()));
            }
            session
                .pending
                .clone()
                .ok_or_else(|| EngineError::Other("No Keiki sign-in is pending".into()))?
        };
        let code = flow
            .authorization_code(callback)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let tokens = request(self.inner.client.exchange_code(&flow, &code))
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let saved = flow.stored_credentials(&tokens);
        self.save_refresh(&tokens, saved.client_id.clone(), generation)?;
        let account = self.account(tokens.access_token(), generation).await?;
        self.publish(tokens, saved.client_id, account, generation)?;
        let mut session = lock(&self.inner.session);
        if session.generation == generation {
            session.pending = None;
        }
        Ok(())
    }

    pub async fn complete_sign_in(&self, callback: &str) -> Result<(), EngineError> {
        let generation = lock(&self.inner.session).generation;
        self.complete_generation(callback, generation).await
    }

    pub async fn sign_out(&self) -> Result<(), EngineError> {
        let (saved, cleared) = {
            let mut session = lock(&self.inner.session);
            let saved = session.saved.clone();
            let cleared = self.clear_locked(&mut session);
            (saved, cleared)
        };
        if let Some(saved) = saved {
            if let Err(error) = request(
                self.inner
                    .client
                    .revoke_token(&saved.credentials.refresh_token),
            )
            .await
            {
                tracing::warn!(%error, "Keiki token could not be revoked; local session is cleared");
            }
        }
        cleared
    }

    pub async fn list_orgs(&self) -> Result<Vec<OrgMembership>, EngineError> {
        let token = self
            .access_token()
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let _refresh = self.inner.refresh.lock().await;
        let generation = {
            let session = lock(&self.inner.session);
            if session
                .access
                .as_ref()
                .is_none_or(|access| access.tokens.access_token() != token)
            {
                return Err(EngineError::Other(
                    "Keiki session changed; retry the request".into(),
                ));
            }
            session.generation
        };
        Ok(self
            .account(&token, generation)
            .await?
            .user
            .orgs
            .into_iter()
            .map(|org| OrgMembership {
                id: org.id.clone(),
                organization_id: org.id,
                name: org.name.unwrap_or_else(|| "Organization".into()),
            })
            .collect())
    }

    pub async fn select_org(&self, organization_id: &str) -> Result<(), EngineError> {
        let token = self
            .access_token()
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let _refresh = self.inner.refresh.lock().await;
        let generation = {
            let session = lock(&self.inner.session);
            if session
                .access
                .as_ref()
                .is_none_or(|access| access.tokens.access_token() != token)
            {
                return Err(EngineError::Other(
                    "Keiki session changed; retry the request".into(),
                ));
            }
            session.generation
        };
        match request(self.inner.client.switch_org(&token, organization_id)).await {
            Ok(result) if result.ok && result.active_org_id == organization_id => {}
            Ok(_) => {
                return Err(EngineError::Other(
                    "Keiki refused the organization switch".into(),
                ));
            }
            Err(error) => {
                if error.is_authentication_failure() {
                    self.clear_session(Some(generation))?;
                }
                return Err(EngineError::Other(error.to_string()));
            }
        }
        let account = self.account(&token, generation).await?;
        let mut session = lock(&self.inner.session);
        if generation != session.generation {
            return Err(EngineError::Other("Keiki session changed".into()));
        }
        let email = account.user.email.unwrap().trim().to_lowercase();
        let user = AuthUser {
            id: email.clone(),
            email,
            name: account.user.name,
        };
        let state = match account.user.active_org_id.or(account.user.org_id) {
            Some(org) => AuthState::SignedIn {
                user,
                org_id: Some(org),
            },
            None => AuthState::NeedsOrganization { user },
        };
        self.inner.state.send_replace(state);
        session.generation = session.generation.wrapping_add(1);
        self.inner
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(())
    }

    pub fn retry_refresh(&self) {
        self.inner
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub fn spawn_refresh_loop(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                if lock(&inner.session).stopped {
                    break;
                }
                let _ = Auth { inner }.access_token().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        })
    }

    pub async fn quiesce_refresh(&self, limit: Duration) {
        lock(&self.inner.session).stopped = true;
        let _ = tokio::time::timeout(limit, self.inner.refresh.lock()).await;
    }
}

#[async_trait::async_trait]
impl zeron_rpc::TokenSource for Auth {
    async fn token(&self) -> Result<String, TokenError> {
        Err(TokenError::SignedOut)
    }
    fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        Some(self.inner.changes.subscribe())
    }
}

#[async_trait::async_trait]
impl zeron_preview::signaling::TokenSource for Auth {
    async fn token(&self) -> anyhow::Result<String> {
        anyhow::bail!("Keiki does not use edge signaling")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMembership {
    pub id: String,
    pub organization_id: String,
    pub name: String,
}

/// AuthStatus stream payload (`SignedOut | NeedsOrganization{user} |
/// SignedIn{user, orgId?}`). Serializes as the canonical [`zeron_proto::AuthState`]
/// wire shape (`{"state": "signedIn", …}`) so every client parses one form.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    NeedsOrganization {
        user: AuthUser,
    },
    SignedIn {
        user: AuthUser,
        org_id: Option<String>,
    },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, AuthState::SignedIn { .. })
    }

    pub fn org_id(&self) -> Option<&str> {
        match self {
            AuthState::SignedIn { org_id, .. } => org_id.as_deref(),
            _ => None,
        }
    }

    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    /// The proto wire twin — the one shape the engine emits over AuthStatus.
    pub fn to_proto(&self) -> zeron_proto::AuthState {
        let profile = |user: &AuthUser| zeron_proto::UserProfile {
            id: user.id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        };
        match self {
            AuthState::SignedOut => zeron_proto::AuthState::SignedOut,
            AuthState::NeedsOrganization { user } => zeron_proto::AuthState::NeedsOrganization {
                user: profile(user),
            },
            AuthState::SignedIn { user, org_id } => zeron_proto::AuthState::SignedIn {
                user: profile(user),
                org_id: org_id.clone(),
            },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenHint {
    #[serde(rename = "sign-in")]
    SignIn,
    #[serde(rename = "sign-up")]
    SignUp,
}

/// How a headed sign-in opens (`SignIn {screenHint?, fresh?}`). The default
/// is the hosted page's own default, exactly as before these existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInOptions {
    #[serde(default)]
    pub screen_hint: Option<ScreenHint>,
    /// Ask afresh (`max_age=0`): the hosted page doesn't go on as the
    /// account the browser is still signed into.
    #[serde(default)]
    pub fresh: bool,
}

#[cfg(test)]
mod tests;
