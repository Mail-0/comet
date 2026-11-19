//! Copilot and test harness registration and device preferences.
//!
//! Also owns the device's harness ENABLEMENT (Settings → Providers): which harnesses
//! this device's composer offers, persisted in `{data_dir}/harness-prefs.json`.
//! Per-device because CLI installs are — a viewer retargets the settings page at
//! another device and edits THAT device's set over the forwarded RPCs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use zeron_harness::{Harness, HarnessError, mock::MockHarness};
use zeron_proto::{AccessMode, AgentEvent, DoneStatus, HarnessId, ReasoningLevel, SteeringMode};

/// What `ListHarnesses` reports per harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// Whether the agent's CLI is present on the listing device (the settings
    /// enable-gate). Defaults true so catalogs from engines predating the
    /// field never read as uninstallable.
    #[serde(default = "default_installed")]
    pub installed: bool,
    /// Explicit CLI installation is available on this listing device.
    #[serde(default)]
    pub can_install: bool,
    /// Whether the listing device offers this harness (Settings → Providers).
    /// `None` — the catalog came from an engine predating the setting — means
    /// "unknown": consumers fall back to detection (see [`descriptor_enabled`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The access modes this harness honors, least to most trusting. Empty
    /// — a catalog from an engine predating modes — offers no choice: those
    /// engines run every chat with full access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_modes: Vec<AccessMode>,
    /// Whether this harness honors plan mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_plan: bool,
    /// An agent added from the ACP registry: its one-ink SVG icon, for the
    /// pickers (built-in agents have marks of their own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl HarnessDescriptor {
    /// Whether this harness can accept a prompt inside the turn that is
    /// currently running. Turn-boundary steering is still useful to the
    /// automatic queue drain, but it is not the non-interrupting "Steer"
    /// action exposed on an individual queued row.
    pub fn steers_mid_turn(&self) -> bool {
        self.supports_steering && self.steering_mode == SteeringMode::StepBoundary
    }
}

fn default_installed() -> bool {
    true
}

/// Whether detection alone may switch a harness on. The mock always "resolves"
/// but is a test rig, and counting it would let the last real agent be turned
/// off (the composer needs something it can actually run); the dev rig opts it
/// in through `COMET_HARNESS=mock`, which bypasses enablement in the pickers.
fn auto_enabled(id: HarnessId) -> bool {
    id != HarnessId::Mock
}

/// A descriptor's effective enabled flag. `None` — a catalog from an engine
/// predating the setting — falls back to detection, the same rule new devices
/// start from (see [`HarnessRegistry::enabled_set`]).
pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor
        .enabled
        .unwrap_or_else(|| descriptor.installed && auto_enabled(descriptor.id))
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
        installed: harness.installed(),
        can_install: false,
        enabled: None,
        access_modes: harness.access_modes().to_vec(),
        supports_plan: harness.supports_plan(),
        icon: None,
    }
}

/// The persisted shape of `harness-prefs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HarnessPrefsFile {
    /// The user's explicit opt-OUTS. Enablement otherwise follows detection,
    /// so the file only records "no" — an agent installed later turns itself
    /// on without a trip to Settings.
    disabled: Vec<HarnessId>,
    titles: TitleSettings,
    /// Harnesses the user turned native subagents off for: their runs
    /// delegate through side chats only.
    native_subagents_off: Vec<HarnessId>,
    /// The allow-list written back when enablement was a fixed default set.
    /// Read once, folded into `disabled`, and never written again.
    #[serde(skip_serializing)]
    enabled: Option<Vec<HarnessId>>,
}

/// Per-device automatic session title preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TitleSettings {
    /// None follows the session harness, using a supported installed fallback.
    pub harness: Option<HarnessId>,
    /// None selects the cheapest model offered by the selected harness.
    pub model: Option<String>,
}

/// Per-device native subagent preferences (Settings → Providers).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NativeSubagentSettings {
    /// Registered harnesses whose native subagents can be turned off.
    pub switchable: Vec<HarnessId>,
    /// The ones turned off: their runs delegate through side chats only.
    pub off: Vec<HarnessId>,
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;
type InstalledProbe = Box<dyn Fn() -> bool + Send + Sync>;

/// The harnesses a Cloud device (`ZERON_DEVICE_PLATFORM=cloud`) registers —
/// the typed twin of [`zeron_proto::CLOUD_HARNESSES`]. Everything else is not
/// merely disabled but absent: never probed (`--version`, install detection,
/// model discovery, update checks). The rule is generic to the `cloud`
/// platform; the motivating example is a hosted sandbox whose preinstalled
/// CLIs are lazy installer shims (Boat's, at the time of writing) that
/// download and install the real agent when merely probed.
pub const CLOUD_HARNESS_IDS: &[HarnessId] = &[HarnessId::Copilot];

/// How long a run waits for an expected [`RunEnvironment`]: far longer than the
/// engine's assembly, on any disk.
const RUN_ENVIRONMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Host-side environment for agent spawns (the Cloud device's credential
/// broker). Called right before a request reaches the harness; whatever it
/// puts in [`zeron_proto::RunRequest::env`] is never persisted.
#[async_trait::async_trait]
pub trait RunEnvironment: Send + Sync {
    /// `Err` = the run can't start: no credential for `harness` (the
    /// message says where to sign in). `chat_id` is the chat the run
    /// belongs to (`None` for runs of no chat, such as title generation).
    async fn prepare(
        &self,
        harness: HarnessId,
        chat_id: Option<&str>,
        request: &mut zeron_proto::RunRequest,
    ) -> Result<(), String>;

    /// Which credential a run of `harness` for `chat_id` would use now (an
    /// opaque fingerprint), `None` when unknown. A parked session started
    /// with a different one is replaced before its next turn, so switching
    /// the account applies from the next turn.
    async fn credential(&self, _harness: HarnessId, _chat_id: Option<&str>) -> Option<String> {
        None
    }

    /// A run of `harness` that started with `credential` reported the
    /// subscription's usage (see [`zeron_proto::AgentEvent::PlanUsage`]).
    async fn record_usage(
        &self,
        _harness: HarnessId,
        _credential: &str,
        _windows: Vec<zeron_proto::AgentUsageWindow>,
        _plan_label: Option<String>,
    ) {
    }
}

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        /// Re-run on every `descriptors()` call — a CLI installed mid-session
        /// shows up on the next settings/picker open, no restart needed.
        installed: InstalledProbe,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
    /// This device's enabled set; `None` inner value = the default set.
    prefs: Mutex<HarnessPrefsFile>,
    pub(crate) copilot_credentials: crate::CopilotCredentialHolder,
    /// Where the prefs persist; `None` (tests, bare registries) skips writes.
    prefs_path: Mutex<Option<PathBuf>>,
    /// Fair per-harness execution gates. Runs and title generation hold a
    /// shared lease; an accepted update queues an exclusive lease. Tokio's
    /// write-preferring FIFO policy prevents a stream of new runs from
    /// starving an update that is already waiting.
    gates: Mutex<HashMap<HarnessId, Arc<tokio::sync::RwLock<()>>>>,
    /// Spawn-time environment provider (Cloud runner only).
    run_environment: Mutex<Option<Arc<dyn RunEnvironment>>>,
    /// False from [`Self::expect_run_environment`] until it's installed.
    run_environment_ready: tokio::sync::watch::Sender<bool>,
    /// Spawn-time environment of the agent accounts zeron keeps itself
    /// (Muse's per-account roots); after the Cloud broker, never over it.
    account_environment: Mutex<Option<Arc<dyn RunEnvironment>>>,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub async fn discover_models(
        &self,
        id: HarnessId,
    ) -> Result<Vec<zeron_proto::Model>, HarnessError> {
        let lease = Arc::new(self.execution_lease(id).await);
        self.discover_models_with_lease(id, lease).await
    }

    /// A caller such as titling already holds a lease. Reacquiring after an
    /// update queues would deadlock it against its own existing reader.
    pub(crate) async fn discover_models_with_lease(
        &self,
        id: HarnessId,
        lease: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<Vec<zeron_proto::Model>, HarnessError> {
        let harness = self.resolve(id)?;
        tokio::spawn(async move {
            // An RPC cancellation must not drop the gate before the probe's
            // own deadline and child cleanup finish.
            let _lease = lease;
            harness.models().await
        })
        .await
        .map_err(|error| HarnessError::Protocol(format!("model discovery task failed: {error}")))?
    }

    pub async fn discover_commands(
        &self,
        id: HarnessId,
        cwd: &Path,
    ) -> Result<Vec<zeron_proto::SlashCommand>, HarnessError> {
        let cwd = cwd.to_owned();
        let lease = self.execution_lease(id).await;
        let harness = self.resolve(id)?;
        tokio::spawn(async move {
            let _lease = lease;
            harness.commands_for(&cwd).await
        })
        .await
        .map_err(|error| {
            HarnessError::Protocol(format!("command discovery task failed: {error}"))
        })?
    }

    pub async fn discover_skills(
        &self,
        id: HarnessId,
        cwd: &Path,
    ) -> Result<Option<Vec<zeron_proto::invocation::Skill>>, HarnessError> {
        let cwd = cwd.to_owned();
        let lease = self.execution_lease(id).await;
        let harness = self.resolve(id)?;
        tokio::spawn(async move {
            // Retain the read lease through the adapter's deadline and cleanup,
            // even when the requesting RPC is dropped.
            let _lease = lease;
            harness.skills(&cwd).await
        })
        .await
        .map_err(|error| HarnessError::Protocol(format!("skill discovery task failed: {error}")))?
    }

    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            prefs: Mutex::new(HarnessPrefsFile::default()),
            copilot_credentials: Default::default(),
            prefs_path: Mutex::new(None),
            gates: Mutex::new(HashMap::new()),
            run_environment: Mutex::new(None),
            run_environment_ready: tokio::sync::watch::channel(true).0,
            account_environment: Mutex::new(None),
        }
    }

    /// Install the spawn-time environment provider (the Cloud runner's
    /// credential broker). Laptops never set one.
    pub fn set_run_environment(&self, environment: Arc<dyn RunEnvironment>) {
        *self
            .run_environment
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(environment);
        self.run_environment_ready.send_replace(true);
    }

    /// One is on its way: runs wait for it rather than start without it. On a
    /// Cloud machine, boot recovery dispatches a killed turn while the engine
    /// is still assembling, ahead of the credential broker. Started then, the
    /// agent has no login and none of its earlier conversations, and it fails.
    pub fn expect_run_environment(&self) {
        self.run_environment_ready.send_replace(false);
    }

    /// Install the agent accounts' environment provider (every device).
    pub fn set_account_environment(&self, environment: Arc<dyn RunEnvironment>) {
        *self
            .account_environment
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(environment);
    }

    fn environments(&self) -> Vec<Arc<dyn RunEnvironment>> {
        [&self.run_environment, &self.account_environment]
            .into_iter()
            .filter_map(|slot| slot.lock().unwrap_or_else(PoisonError::into_inner).clone())
            .collect()
    }

    /// Fill `request.env` for a spawn of `harness` (no-op without a provider).
    pub async fn prepare_run(
        &self,
        harness: HarnessId,
        chat_id: Option<&str>,
        request: &mut zeron_proto::RunRequest,
    ) -> Result<(), String> {
        let mut ready = self.run_environment_ready.subscribe();
        if tokio::time::timeout(RUN_ENVIRONMENT_WAIT, ready.wait_for(|ready| *ready))
            .await
            .is_err()
        {
            return Err("This machine's agent logins aren't ready yet. Send again in a moment.".into(),
            );
        }
        for environment in self.environments() {
            environment.prepare(harness, chat_id, request).await?;
        }
        Ok(())
    }

    /// [`RunEnvironment::credential`] (`None` without a provider).
    pub async fn run_credential(
        &self,
        harness: HarnessId,
        chat_id: Option<&str>,
    ) -> Option<String> {
        for environment in self.environments() {
            if let Some(credential) = environment.credential(harness, chat_id).await {
                return Some(credential);
            }
        }
        None
    }

    /// Hand a run's reported subscription usage to every provider (the
    /// account it belongs to records it).
    pub async fn record_plan_usage(
        &self,
        harness: HarnessId,
        credential: &str,
        windows: Vec<zeron_proto::AgentUsageWindow>,
        plan_label: Option<String>,
    ) {
        for environment in self.environments() {
            environment
                .record_usage(harness, credential, windows.clone(), plan_label.clone())
                .await;
        }
    }

    /// Whether `id` exists on this device at all (a Cloud device registers
    /// only [`CLOUD_HARNESS_IDS`]).
    pub fn is_registered(&self, id: HarnessId) -> bool {
        self.slots().contains_key(&id)
    }

    /// Drop every slot `keep` rejects. Lazy slots are dropped unresolved, so
    /// a removed harness is never probed.
    fn retain(&self, keep: impl Fn(HarnessId) -> bool) {
        self.slots().retain(|id, _| keep(*id));
        self.order().retain(|id| keep(*id));
    }

    fn gate(&self, id: HarnessId) -> Arc<tokio::sync::RwLock<()>> {
        self.gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    /// Shared lease held for the full lifetime of a harness subprocess.
    pub async fn execution_lease(&self, id: HarnessId) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.gate(id).read_owned().await
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn prefs(&self) -> MutexGuard<'_, HarnessPrefsFile> {
        self.prefs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load `harness-prefs.json` from the engine data dir and remember the
    /// path for writes. Corrupt/missing files fall back to the default set.
    pub fn load_prefs(&self, data_dir: &Path) {
        let path = data_dir.join("harness-prefs.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HarnessPrefsFile>(&text).ok())
            .unwrap_or_default();
        *self.prefs() = loaded;
        *self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
        self.migrate_legacy_prefs();
    }

    /// Fold a legacy allow-list into the opt-out shape: a registered harness
    /// missing from it was a deliberate "no", so it stays off. Rewrites the
    /// file once, which is what lets later installs auto-enable.
    fn migrate_legacy_prefs(&self) {
        let legacy = { self.prefs().enabled.take() };
        let Some(legacy) = legacy else { return };
        let registered: Vec<HarnessId> = self.order().iter().copied().collect();
        let disabled: Vec<HarnessId> = registered
            .into_iter()
            .filter(|id| auto_enabled(*id) && !legacy.contains(id))
            .collect();
        self.prefs().disabled = disabled;
        self.persist_prefs();
    }

    /// What this device offers: every harness whose CLI is FOUND, minus the
    /// user's explicit opt-outs. Enablement follows detection, so installing
    /// an agent is all it takes for it to appear in the composer.
    pub fn enabled_set(&self) -> Vec<HarnessId> {
        // Both guards drop before the installed probes run: `descriptors()`
        // takes `slots` then `order`, so holding `order` across a probe (which
        // takes `slots`) would invert the lock order.
        let registered: Vec<HarnessId> = self.order().iter().copied().collect();
        let disabled = self.prefs().disabled.clone();
        registered
            .into_iter()
            .filter(|id| auto_enabled(*id) && !disabled.contains(id) && self.installed_for(*id))
            .collect()
    }

    /// Whether this device's CLI probe passes for `id` (no spawn, no resolve).
    fn installed_for(&self, id: HarnessId) -> bool {
        match self.slots().get(&id) {
            Some(Slot::Ready(harness)) => harness.installed(),
            Some(Slot::Lazy { installed, .. }) => installed(),
            None => false,
        }
    }

    /// Flip one harness's enablement and persist. Refuses unknown harnesses,
    /// enabling one whose CLI is missing (the settings gate, enforced where
    /// the state lives), and disabling the last enabled harness — under
    /// detection-based enablement everything enabled is runnable, so the
    /// last one standing is always worth protecting (the composer needs
    /// something to run). A harness whose CLI is missing is never enabled
    /// in the first place, so turning it off is a clean no-op.
    pub fn set_enabled(&self, id: HarnessId, on: bool) -> Result<(), String> {
        if !self.slots().contains_key(&id) {
            return Err(format!("unknown harness {id:?}"));
        }
        if on && !auto_enabled(id) {
            return Err(format!("{id:?} cannot be enabled from Settings"));
        }
        if on && !self.installed_for(id) {
            return Err(format!("{id:?} CLI is not installed on this device"));
        }
        let enabled = self.enabled_set();
        match (on, enabled.contains(&id)) {
            (true, false) => {
                let mut prefs = self.prefs();
                prefs.disabled.retain(|h| *h != id);
            }
            (false, true) => {
                if enabled.len() == 1 {
                    return Err("cannot disable the last enabled harness".into());
                }
                let mut prefs = self.prefs();
                prefs.disabled.push(id);
            }
            _ => return Ok(()),
        }
        self.persist_prefs();
        Ok(())
    }

    /// Best-effort atomic write (temp + rename, the ui-settings pattern).
    fn persist_prefs(&self) {
        let Some(path) = self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let json = match serde_json::to_string_pretty(&*self.prefs()) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "harness-prefs serialize failed");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %err, "harness-prefs save failed");
        }
    }

    pub fn title_settings(&self) -> TitleSettings {
        self.prefs().titles.clone()
    }

    pub fn set_title_settings(&self, mut settings: TitleSettings) -> Result<(), String> {
        if let Some(id) = settings.harness {
            if !zeron_harness::supports_titles(id) || !self.enabled_set().contains(&id) {
                return Err("Choose an enabled harness that supports title generation".into());
            }
        } else if settings.model.is_some() {
            return Err("Choose a title harness before choosing a model".into());
        }
        settings.model = settings.model.filter(|model| !model.trim().is_empty());
        self.prefs().titles = settings;
        self.persist_prefs();
        Ok(())
    }

    /// Whether runs of `id` keep the provider's own subagent tools. Off only
    /// by the user's choice, for a driver that can launch without them.
    pub fn native_subagents(&self, id: HarnessId) -> bool {
        !(zeron_harness::switches_native_subagents(id)
            && self.prefs().native_subagents_off.contains(&id))
    }

    pub fn native_subagent_settings(&self) -> NativeSubagentSettings {
        let registered: Vec<HarnessId> = self.order().iter().copied().collect();
        NativeSubagentSettings {
            switchable: registered
                .into_iter()
                .filter(|id| zeron_harness::switches_native_subagents(*id))
                .collect(),
            off: self.prefs().native_subagents_off.clone(),
        }
    }

    /// Turn `id`'s native subagents on or off and persist. Each chat picks
    /// the change up at its next turn once its background work is done: a
    /// parked session still holding some is kept as it started.
    pub fn set_native_subagents(&self, id: HarnessId, on: bool) -> Result<(), String> {
        if !zeron_harness::switches_native_subagents(id) {
            return Err(format!("{id:?} has no native subagents to turn off"));
        }
        {
            let mut prefs = self.prefs();
            match (on, prefs.native_subagents_off.contains(&id)) {
                (true, true) => prefs.native_subagents_off.retain(|h| *h != id),
                (false, false) => prefs.native_subagents_off.push(id),
                _ => return Ok(()),
            }
        }
        self.persist_prefs();
        Ok(())
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is
    /// cached). `installed` is the CLI-presence probe run per `descriptors()`
    /// call; it must never spawn.
    pub fn register_lazy(
        &self,
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    ) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor,
                    installed,
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn register_copilot(&self, credentials: Arc<dyn zeron_harness::CopilotCredentialSource>) {
        self.register(Arc::new(zeron_harness::CopilotHarness::new(credentials)));
        let mut order = self.order();
        order.retain(|id| *id != HarnessId::Copilot);
        order.insert(0, HarnessId::Copilot);
    }

    pub fn descriptors_for(&self, _include_registry: bool) -> Vec<HarnessDescriptor> {
        self.descriptors()
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        if matches!(id, HarnessId::Unknown(_)) {
            return Err(HarnessError::NotInstalled(
                "This session's agent is no longer available".into(),
            ));
        }
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    /// Catalog for `ListHarnesses` — never forces a lazy resolve.
    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let enabled = self.enabled_set();
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| {
                let mut descriptor = match slots.get(id) {
                    Some(Slot::Ready(harness)) => describe(harness.as_ref()),
                    Some(Slot::Lazy {
                        descriptor,
                        installed,
                        ..
                    }) => HarnessDescriptor {
                        installed: installed(),
                        ..descriptor.clone()
                    },
                    None => return None,
                };
                descriptor.enabled = Some(enabled.contains(id));
                descriptor.can_install = false;
                Some(descriptor)
            })
            .collect()
    }
}

/// The production registry for this engine's platform (see
/// [`registry_for_platform`]).
pub fn default_registry() -> HarnessRegistry {
    registry_for_platform(std::env::consts::OS)
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `zeron_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it). On a
/// `cloud` platform only [`CLOUD_HARNESS_IDS`] (plus the test-only mock)
/// are registered.
pub fn registry_for_platform(platform: &str) -> HarnessRegistry {
    let registry = full_registry();
    if platform == zeron_proto::CLOUD_PLATFORM {
        registry.retain(|id| id == HarnessId::Mock || CLOUD_HARNESS_IDS.contains(&id));
    }
    registry
}

fn full_registry() -> HarnessRegistry {
    // Warm the login-shell PATH snapshot in the background so the first
    // claude/codex resolve doesn't pay the shell-startup latency inline.
    zeron_harness::shell_env::prewarm();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: vec![
            AgentEvent::TextDelta {
                text: "## Streaming pipeline\n\nEvery turn flows through the same path:\n\n".into(),
            },
            AgentEvent::TextDelta {
                text: "1. **Doc command** — the composer queues a durable `run` entry\n2. **Host executor** — the chat's host device marks it processed, then dispatches\n3. **Fold** — events fold into parts and diff into the Loro doc every 120ms\n\n".into(),
            },
            AgentEvent::ToolCall {
                id: "mock-tool-1".into(),
                call: zeron_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
                preview_paths: Vec::new(),
                image_payloads: Vec::new(),
                exit_code: None,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: zeron_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
                output: None,
                diff: None,
                preview_paths: Vec::new(),
                image_payloads: Vec::new(),
                exit_code: None,
            },
            AgentEvent::TextDelta {
                text: "The `SegmentWriter` appends into `LoroText` so the oplog stays RLE-merged:\n\n```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms coalesced commits\n```\n\nSynced to every device through the session room. *Mock harness reporting in.*".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    }));
    registry.register_copilot(Arc::new(registry.copilot_credentials.clone()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_rpc::{RpcReply, RpcService};

    #[test]
    fn only_copilot_and_mock_are_registered_and_unknown_sessions_cannot_run() {
        for platform in ["linux", "macos", "windows", "cloud"] {
            let registry = registry_for_platform(platform);
            let ids: Vec<_> = registry.descriptors().iter().map(|d| d.id).collect();
            let expected = vec![HarnessId::Copilot, HarnessId::Mock];
            assert_eq!(ids, expected);
            assert!(registry.descriptors().iter().all(|d| !d.can_install));
            let legacy: HarnessId = serde_json::from_str("\"codex\"").unwrap();
            assert!(registry.resolve(legacy).is_err());
            assert!(registry.resolve(HarnessId::Unknown("unknown")).is_err());
        }
    }

    #[test]
    fn copilot_is_enabled_by_default_and_preferences_preserve_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let registry = default_registry();
        registry.load_prefs(dir.path());
        assert!(registry.enabled_set().contains(&HarnessId::Copilot));
        assert!(!registry.enabled_set().contains(&HarnessId::Mock));
        assert!(registry.set_enabled(HarnessId::Copilot, false).is_err());
        assert!(registry.set_enabled(HarnessId::Mock, true).is_err());
        std::fs::write(
            dir.path().join("harness-prefs.json"),
            r#"{"disabled":["copilot"]}"#,
        )
        .unwrap();
        let reopened = default_registry();
        reopened.load_prefs(dir.path());
        assert!(!reopened.enabled_set().contains(&HarnessId::Copilot));
    }

    #[test]
    fn cloud_ids_match_the_wire_contract() {
        assert_eq!(
            CLOUD_HARNESS_IDS
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>(),
            zeron_proto::CLOUD_HARNESSES
        );
    }

    #[test]
    fn mid_turn_steering_requires_support_and_a_step_boundary() {
        let mut descriptor = HarnessDescriptor {
            icon: None,
            id: HarnessId::Mock,
            name: "Mock".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            can_install: false,
            enabled: Some(true),
            access_modes: Vec::new(),
            supports_plan: false,
        };
        assert!(descriptor.steers_mid_turn());

        descriptor.steering_mode = SteeringMode::TurnBoundary;
        assert!(!descriptor.steers_mid_turn());

        descriptor.steering_mode = SteeringMode::StepBoundary;
        descriptor.supports_steering = false;
        assert!(!descriptor.steers_mid_turn());
    }

    #[test]
    fn lazy_slot_lists_without_resolving() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        registry.register_lazy(
            HarnessDescriptor {
                icon: None,
                id: HarnessId::Mock,
                name: "Lazy Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                can_install: false,
                enabled: None,
                access_modes: Vec::new(),
                supports_plan: false,
            },
            Box::new(|| false),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        // The listing runs the probe, not the stored placeholder.
        assert!(!listed[0].installed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn descriptor_without_new_fields_parses_with_fallbacks() {
        let parse = |id: &str| -> HarnessDescriptor {
            serde_json::from_str(&format!(
                r#"{{
                    "id": "{id}",
                    "name": "x",
                    "supportsSteering": true,
                    "steeringMode": "step-boundary",
                    "reasoningLevels": []
                }}"#
            ))
            .unwrap()
        };
        let claude = parse("claude-code");
        assert!(claude.installed);
        assert!(!claude.can_install);
        assert_eq!(claude.enabled, None);
        // Unknown enablement follows detection: a found CLI is offered...
        assert!(descriptor_enabled(&claude));
        // ...and one this device never found is not.
        let missing = HarnessDescriptor {
            installed: false,
            can_install: false,
            ..parse("grok")
        };
        assert!(!descriptor_enabled(&missing));
    }

    #[test]
    fn old_harness_preferences_default_to_automatic_titles() {
        let prefs: HarnessPrefsFile = serde_json::from_str(r#"{"disabled":["codex"]}"#).unwrap();
        assert_eq!(prefs.titles, TitleSettings::default());
        assert_eq!(prefs.disabled, vec![HarnessId::Unknown("codex")]);
        assert!(prefs.native_subagents_off.is_empty());
    }

    #[tokio::test]
    async fn run_environment_fills_env_only_when_installed() {
        struct Fill;
        #[async_trait::async_trait]
        impl RunEnvironment for Fill {
            async fn prepare(
                &self,
                harness: HarnessId,
                _chat_id: Option<&str>,
                request: &mut zeron_proto::RunRequest,
            ) -> Result<(), String> {
                request.env.insert("HARNESS".into(), format!("{harness:?}"));
                Ok(())
            }
        }
        let mut request: zeron_proto::RunRequest = serde_json::from_str(
            r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#,
        )
        .unwrap();
        let registry = HarnessRegistry::new();
        registry
            .prepare_run(HarnessId::Copilot, None, &mut request)
            .await
            .unwrap();
        assert!(request.env.is_empty());
        registry.set_run_environment(Arc::new(Fill));
        registry
            .prepare_run(HarnessId::Copilot, None, &mut request)
            .await
            .unwrap();
        assert_eq!(
            request.env.get("HARNESS").map(String::as_str),
            Some("Copilot")
        );
    }

    #[tokio::test]
    async fn retired_commit_credit_preferences_do_not_add_hooks() {
        let data = tempfile::tempdir().unwrap();
        let mut request: zeron_proto::RunRequest = serde_json::from_str(
            r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#,
        )
        .unwrap();
        let preference = data.path().join("commit-credit.json");
        std::fs::write(&preference, r#"{"enabled":true}"#).unwrap();
        let registry = Arc::new(HarnessRegistry::new());
        let core = crate::EngineCore::assemble(
            data.path(), registry.clone(), HarnessId::Copilot, None,
        ).unwrap();
        let rpc = core.rpc_service();
        for method in [zeron_rpc::methods::GET_COMMIT_CREDIT, zeron_rpc::methods::SET_COMMIT_CREDIT] {
            let RpcReply::Value(value) = rpc
                .handle(method, serde_json::json!({"enabled":true}))
                .await.unwrap() else { panic!("compatibility reply") };
            assert_eq!(value, serde_json::json!({"enabled":false,"cloud":false}));
        }
        registry
            .prepare_run(HarnessId::Copilot, None, &mut request)
            .await
            .unwrap();
        let hooks = data.path().join("git-hooks");
        assert!(!hooks.exists());
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(preference).unwrap()).unwrap();
        assert_eq!(saved, serde_json::json!({"enabled":true}));
        assert!(request.env.is_empty());
    }
}
