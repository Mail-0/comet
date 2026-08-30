//! Local workspace index persisted alongside session documents.
//!
//! [`RegistryDoc`] stores materialized rows and an HLC clock stamping every
//! local write. Legacy pending batches are accepted while loading old
//! snapshots and folded into the authoritative rows.
//!
//! The typed API is used by `WorkspaceHost` for local sidebar persistence.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use zeron_proto::{Chat, ChatConfig, Device, Session, SessionStatus, Space};

use crate::schema::DocError;

/// Row kinds — the four sidebar tables.
pub const KIND_DEVICES: &str = "devices";
pub const KIND_SPACES: &str = "spaces";
pub const KIND_CHATS: &str = "chats";
pub const KIND_SESSIONS: &str = "sessions";

/// Snapshot row id in the local `DocsStore` for the persisted registry state.
pub const REGISTRY_DOC_ID: &str = "registry1";

/// Result of deleting a space and its indexed chats.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletedSpace {
    pub existed: bool,
    pub chat_ids: Vec<String>,
}

/// Materialized local workspace index state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
}

fn dt(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

// ── doc-resident row shapes (epoch-millis timestamps) ───────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawDevice {
    id: String,
    name: String,
    platform: String,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    version: Option<String>,
}

impl From<RawDevice> for Device {
    fn from(raw: RawDevice) -> Self {
        Device {
            id: raw.id,
            name: raw.name,
            platform: raw.platform,
            last_seen_at: raw.last_seen_at.map(dt),
            created_at: raw.created_at.map(dt),
            version: raw.version,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSpace {
    id: String,
    device_id: String,
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    git_detected: bool,
    #[serde(default)]
    git_checked_at: Option<i64>,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    created_at: i64,
}

impl From<RawSpace> for Space {
    fn from(raw: RawSpace) -> Self {
        Space {
            id: raw.id,
            device_id: raw.device_id,
            path: raw.path,
            name: raw.name,
            git_detected: raw.git_detected,
            git_checked_at: raw.git_checked_at.map(dt),
            checkout_id: raw.checkout_id,
            created_at: dt(raw.created_at),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawChat {
    id: String,
    device_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    source_context: Option<zeron_proto::ConversationSourceContext>,
    /// LENIENT: a config this build can't decode (a harness/reasoning/sandbox
    /// id from a NEWER peer — field incident: pre-v0.2.10 laptops dropped
    /// every `"opencode"` chat row wholesale, so new sessions silently never
    /// appeared in the sidebar) degrades to `None` instead of failing the
    /// row. The chat stays visible and selectable with generic defaults;
    /// up-to-date devices still see the real config.
    #[serde(default, deserialize_with = "lenient_chat_config")]
    config: Option<ChatConfig>,
    #[serde(default)]
    last_message_preview: Option<String>,
    #[serde(default)]
    last_message_at: Option<i64>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    harness_session_id: Option<String>,
    #[serde(default)]
    harness_session_cwd: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    last_seen_at: Option<i64>,
}

/// Decode a chat row's `config` leniently: unknown enum values (a newer
/// peer's harness id, reasoning level or sandbox mode) cost the CONFIG, not
/// the row. Mirrors the transcript salvage rule: a missing field must cost
/// at most what the field carried.
fn lenient_chat_config<'de, D>(deserializer: D) -> Result<Option<ChatConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match serde_json::from_value::<ChatConfig>(value) {
        Ok(config) => Some(config),
        Err(err) => {
            tracing::warn!(error = %err, "chat config from a newer peer; showing the row without it");
            None
        }
    }))
}

impl From<RawChat> for Chat {
    fn from(raw: RawChat) -> Self {
        Chat {
            id: raw.id,
            device_id: raw.device_id,
            title: raw.title,
            archived: raw.archived,
            cwd: raw.cwd,
            branch: raw.branch,
            checkout_id: raw.checkout_id,
            source_context: raw.source_context,
            config: raw.config,
            last_message_preview: raw.last_message_preview,
            last_message_at: raw.last_message_at.map(dt),
            created_at: dt(raw.created_at),
            harness_session_id: raw.harness_session_id,
            harness_session_cwd: raw.harness_session_cwd,
            space_id: raw.space_id,
            last_seen_at: raw.last_seen_at.map(dt),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSession {
    chat_id: String,
    device_id: String,
    status: SessionStatus,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    updated_at: i64,
}

impl From<RawSession> for Session {
    fn from(raw: RawSession) -> Self {
        Session {
            chat_id: raw.chat_id,
            device_id: raw.device_id,
            status: raw.status,
            started_at: raw.started_at.map(dt),
            updated_at: dt(raw.updated_at),
        }
    }
}

// ── HLC ─────────────────────────────────────────────────────────────────────

/// Encode an HLC string: `{ms:013}-{counter:06}-{device}`. Fixed-width zero
/// padding makes lexicographic order = (ms, counter, device) order, and the
/// device suffix makes the order total (two writers can never tie).
pub fn encode_hlc(ms: i64, counter: u32, device: &str) -> String {
    format!("{ms:013}-{counter:06}-{device}")
}

/// `a` strictly newer than `b` (`None` = never written, loses to any).
fn hlc_newer(a: &str, b: Option<&str>) -> bool {
    match b {
        None => true,
        Some(b) => a > b,
    }
}

/// Monotonic HLC source: never emits the same or an earlier clock twice, even
/// across a wall-clock regression or restart (state persists with the doc).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HlcClock {
    last_ms: i64,
    counter: u32,
}

impl HlcClock {
    fn next(&mut self, now_ms: i64, device: &str) -> String {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.counter = 0;
        } else {
            self.counter += 1;
            if self.counter > 999_999 {
                self.last_ms += 1;
                self.counter = 0;
            }
        }
        encode_hlc(self.last_ms, self.counter, device)
    }
}

// ── rows and ops ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRow {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub deleted: bool,
    /// Tombstone clock — an upsert newer than this revives the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub del_hlc: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    /// Per-field last-write clocks.
    #[serde(default)]
    pub clocks: BTreeMap<String, String>,
}

impl RegistryRow {
    fn tombstone(kind: &str, id: &str, hlc: String) -> Self {
        Self {
            kind: kind.to_string(),
            id: id.to_string(),
            deleted: true,
            del_hlc: Some(hlc),
            fields: BTreeMap::new(),
            clocks: BTreeMap::new(),
        }
    }

    /// The newest clock anywhere on the row (delete-vs-live comparison base).
    fn max_clock(&self) -> Option<&str> {
        let mut max = self.del_hlc.as_deref();
        for clock in self.clocks.values() {
            if max.is_none_or(|m| clock.as_str() > m) {
                max = Some(clock);
            }
        }
        max
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    /// Creates, and revives tombstones when newer.
    Upsert,
    /// Never creates or revives ("never invent rows").
    Update,
    /// Tombstones when causally newer than the row.
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowOp {
    pub kind: String,
    pub id: String,
    pub op: OpKind,
    /// Field writes; `Value::Null` deletes the field (still a clocked write).
    /// Absent for deletes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, Value>>,
    /// Clock for every write in `set` without an entry in `clocks`.
    pub hlc: String,
    /// Per-field clock overrides — re-seed pushes carry a row's ORIGINAL
    /// clocks so recovery never coarsens causality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clocks: Option<BTreeMap<String, String>>,
}

impl RowOp {
    fn clock_for<'a>(&'a self, field: &str) -> &'a str {
        self.clocks
            .as_ref()
            .and_then(|c| c.get(field))
            .map_or(self.hlc.as_str(), String::as_str)
    }
}

/// Apply one op to a row — the 1:1 mirror of `applyOp` in
/// `local registry merge rules`. Returns the new row (`None` only for an
/// `update` on a missing row) and whether anything changed.
pub fn apply_op(row: Option<&RegistryRow>, op: &RowOp) -> (Option<RegistryRow>, bool) {
    if op.op == OpKind::Delete {
        return match row {
            // Tombstone-on-missing guards against a late create racing the delete.
            None => (
                Some(RegistryRow::tombstone(&op.kind, &op.id, op.hlc.clone())),
                true,
            ),
            Some(row) => {
                let beats = if row.deleted {
                    hlc_newer(&op.hlc, row.del_hlc.as_deref())
                } else {
                    hlc_newer(&op.hlc, row.max_clock())
                };
                if beats {
                    let mut gone = row.clone();
                    gone.deleted = true;
                    gone.del_hlc = Some(op.hlc.clone());
                    gone.fields.clear();
                    gone.clocks.clear();
                    (Some(gone), true)
                } else {
                    (Some(row.clone()), false)
                }
            }
        };
    }

    let mut base = match row {
        None => {
            if op.op == OpKind::Update {
                return (None, false);
            }
            RegistryRow {
                kind: op.kind.clone(),
                id: op.id.clone(),
                deleted: false,
                del_hlc: None,
                fields: BTreeMap::new(),
                clocks: BTreeMap::new(),
            }
        }
        Some(row) if row.deleted => {
            if op.op == OpKind::Update || !hlc_newer(&op.hlc, row.del_hlc.as_deref()) {
                return (Some(row.clone()), false);
            }
            // Revival: the tombstone loses wholesale; the upsert's fields are
            // the row.
            RegistryRow {
                kind: row.kind.clone(),
                id: row.id.clone(),
                deleted: false,
                del_hlc: row.del_hlc.clone(),
                fields: BTreeMap::new(),
                clocks: BTreeMap::new(),
            }
        }
        Some(row) => row.clone(),
    };

    let mut changed = row.is_none() || (row.is_some_and(|r| r.deleted) && !base.deleted);
    if let Some(set) = &op.set {
        for (key, value) in set {
            let clock = op.clock_for(key);
            if !hlc_newer(clock, base.clocks.get(key).map(String::as_str)) {
                continue;
            }
            if value.is_null() {
                base.fields.remove(key);
            } else {
                base.fields.insert(key.clone(), value.clone());
            }
            base.clocks.insert(key.clone(), clock.to_string());
            changed = true;
        }
    }
    if changed {
        base.deleted = false;
        (Some(base), true)
    } else {
        (Some(base), false)
    }
}

// ── the doc ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    v: u32,
    device_id: String,
    clock: HlcClock,
    rows: Vec<RegistryRow>,
    /// Retained only to read snapshots written before registry became local-only.
    #[serde(default, skip_serializing)]
    pending: Vec<LegacyPendingBatch>,
}

#[derive(Deserialize)]
struct LegacyPendingBatch {
    ops: Vec<RowOp>,
}

/// The local registry. Pure data — no I/O or async work.
pub struct RegistryDoc {
    device_id: String,
    /// kind → id → row (persisted rows).
    authoritative: HashMap<String, HashMap<String, RegistryRow>>,
    clock: HlcClock,
    /// Bumped on every mutation (local or applied) — the engine host converts
    /// this into watch-channel publishes and snapshot debounces.
    generation: u64,
}

impl RegistryDoc {
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            authoritative: HashMap::new(),
            clock: HlcClock::default(),
            generation: 0,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Monotonic local change counter (any mutation bumps it).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    // ── snapshot persistence ────────────────────────────────────────────────

    pub fn to_bytes(&self) -> Result<Vec<u8>, DocError> {
        let rows = self
            .authoritative
            .values()
            .flat_map(|m| m.values().cloned())
            .collect();
        let state = PersistedState {
            v: 1,
            device_id: self.device_id.clone(),
            clock: self.clock.clone(),
            rows,
            pending: Vec::new(),
        };
        Ok(serde_json::to_vec(&state)?)
    }

    pub fn from_bytes(bytes: &[u8], device_id: &str) -> Result<Self, DocError> {
        let state: PersistedState = serde_json::from_slice(bytes)?;
        if state.v != 1 {
            return Err(DocError::Schema(format!(
                "unknown registry snapshot version {}",
                state.v
            )));
        }
        let mut doc = Self::new(device_id);
        doc.clock = state.clock;
        for row in state.rows {
            doc.authoritative
                .entry(row.kind.clone())
                .or_default()
                .insert(row.id.clone(), row);
        }
        // Apply mutations left in snapshots by the removed outbox.
        for batch in state.pending {
            for op in batch.ops {
                let current = doc
                    .authoritative
                    .get(&op.kind)
                    .and_then(|rows| rows.get(&op.id))
                    .cloned();
                if let (Some(next), true) = apply_op(current.as_ref(), &op) {
                    doc.put_authoritative(next);
                }
            }
        }
        Ok(doc)
    }

    fn put_authoritative(&mut self, row: RegistryRow) {
        self.authoritative
            .entry(row.kind.clone())
            .or_default()
            .insert(row.id.clone(), row);
    }

    // ── local writes ────────────────────────────────────────────────────────

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn next_hlc(&mut self) -> String {
        let device = self.device_id.clone();
        self.clock.next(Self::now_ms(), &device)
    }

    fn write(&mut self, kind: &str, id: &str, op: OpKind, set: BTreeMap<String, Value>) {
        let operation = RowOp {
            kind: kind.to_string(),
            id: id.to_string(),
            op,
            set: Some(set),
            hlc: self.next_hlc(),
            clocks: None,
        };
        if let (Some(row), true) = apply_op(self.overlay_row(kind, id).as_ref(), &operation) {
            self.put_authoritative(row);
            self.generation += 1;
        }
    }

    fn delete_row_ops(&mut self, keys: &[(&str, &str)]) {
        let hlc = self.next_hlc();
        for (kind, id) in keys {
            let operation = RowOp {
                kind: (*kind).to_string(),
                id: (*id).to_string(),
                op: OpKind::Delete,
                set: None,
                hlc: hlc.clone(),
                clocks: None,
            };
            if let (Some(row), true) = apply_op(self.overlay_row(kind, id).as_ref(), &operation) {
                self.put_authoritative(row);
                self.generation += 1;
            }
        }
    }

    // ── overlay reads ───────────────────────────────────────────────────────

    /// The row as this device should display it.
    fn overlay_row(&self, kind: &str, id: &str) -> Option<RegistryRow> {
        self.authoritative
            .get(kind)
            .and_then(|m| m.get(id))
            .cloned()
            .filter(|r| !r.deleted)
    }

    /// All live rows of `kind`.
    fn overlay_rows(&self, kind: &str) -> Vec<RegistryRow> {
        self.authoritative
            .get(kind)
            .map(|rows| rows.values().filter(|row| !row.deleted).cloned().collect())
            .unwrap_or_default()
    }

    fn read_kind<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Vec<T> {
        let mut out = Vec::new();
        for row in self.overlay_rows(kind) {
            let value = Value::Object(row.fields.clone().into_iter().collect());
            match serde_json::from_value::<T>(value) {
                Ok(parsed) => out.push(parsed),
                Err(err) => {
                    tracing::warn!(kind, row = %row.id, error = %err, "skipping malformed registry row");
                }
            }
        }
        out
    }

    fn row_exists(&self, kind: &str, id: &str) -> bool {
        self.overlay_row(kind, id).is_some()
    }

    // ── typed API (the WorkspaceDoc surface) ────────────────────────────────

    /// Upsert a full device row (writer discipline: callers pass their OWN device).
    pub fn upsert_device(&mut self, device: &Device) -> Result<(), DocError> {
        let set = fields([
            ("id", json!(device.id)),
            ("name", json!(device.name)),
            ("platform", json!(device.platform)),
            ("lastSeenAt", opt_ms(device.last_seen_at)),
            ("createdAt", opt_ms(device.created_at)),
            ("version", opt_str(device.version.as_deref())),
        ]);
        self.write(KIND_DEVICES, &device.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    /// LWW rename (settings UI; any device may write). `false` when no such row.
    pub fn rename_device(&mut self, device_id: &str, name: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_DEVICES, device_id) {
            return Ok(false);
        }
        self.write(
            KIND_DEVICES,
            device_id,
            OpKind::Update,
            fields([("name", json!(name))]),
        );
        Ok(true)
    }

    /// Stamp `lastSeenAt` on an existing device row (boot/shutdown only —
    /// periodic liveness is not persisted).
    pub fn set_device_last_seen(
        &mut self,
        device_id: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_DEVICES, device_id) {
            return Ok(false);
        }
        self.write(
            KIND_DEVICES,
            device_id,
            OpKind::Update,
            fields([("lastSeenAt", json!(at.timestamp_millis()))]),
        );
        Ok(true)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, DocError> {
        let mut devices: Vec<Device> = self
            .read_kind::<RawDevice>(KIND_DEVICES)
            .into_iter()
            .map(Device::from)
            .collect();
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(devices)
    }

    // ── spaces ──────────────────────────────────────────────────────────────

    pub fn upsert_space(&mut self, space: &Space) -> Result<(), DocError> {
        let set = fields([
            ("id", json!(space.id)),
            ("deviceId", json!(space.device_id)),
            ("path", json!(space.path)),
            ("name", opt_str(space.name.as_deref())),
            ("gitDetected", json!(space.git_detected)),
            ("gitCheckedAt", opt_ms(space.git_checked_at)),
            ("checkoutId", opt_str(space.checkout_id.as_deref())),
            ("createdAt", json!(space.created_at.timestamp_millis())),
        ]);
        self.write(KIND_SPACES, &space.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    pub fn space(&self, space_id: &str) -> Result<Option<Space>, DocError> {
        Ok(self
            .overlay_row(KIND_SPACES, space_id)
            .and_then(|row| row_to::<RawSpace>(&row))
            .map(Space::from))
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, DocError> {
        let mut spaces: Vec<Space> = self
            .read_kind::<RawSpace>(KIND_SPACES)
            .into_iter()
            .map(Space::from)
            .collect();
        spaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(spaces)
    }

    /// LWW display-name set from any device; `None` clears back to the derived
    /// name (basename of path). `false` when no such row.
    pub fn rename_space(&mut self, space_id: &str, name: Option<&str>) -> Result<bool, DocError> {
        if !self.row_exists(KIND_SPACES, space_id) {
            return Ok(false);
        }
        self.write(
            KIND_SPACES,
            space_id,
            OpKind::Update,
            fields([("name", opt_str(name))]),
        );
        Ok(true)
    }

    /// Owner-stamped git presence for the space folder (ownership is asserted
    /// by the engine layer, this is mechanism only).
    pub fn set_space_git(
        &mut self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
        checked_at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_SPACES, space_id) {
            return Ok(false);
        }
        self.write(
            KIND_SPACES,
            space_id,
            OpKind::Update,
            fields([
                ("gitDetected", json!(detected)),
                ("checkoutId", opt_str(checkout_id)),
                ("gitCheckedAt", json!(checked_at.timestamp_millis())),
            ]),
        );
        Ok(true)
    }

    /// Hard-delete a space and cascade to its chats: one batch tombstones the
    /// space row and every chat/session row whose `spaceId` matches — the
    /// local snapshot stores the batch. Returns the removed chat ids so
    /// the engine can drop local state.
    pub fn delete_space(&mut self, space_id: &str) -> Result<DeletedSpace, DocError> {
        let existed = self.row_exists(KIND_SPACES, space_id);
        let chat_ids: Vec<String> = self
            .read_chats()?
            .into_iter()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .map(|c| c.id)
            .collect();
        let mut keys: Vec<(&str, &str)> = Vec::with_capacity(chat_ids.len() * 2 + 1);
        for chat_id in &chat_ids {
            keys.push((KIND_CHATS, chat_id));
            keys.push((KIND_SESSIONS, chat_id));
        }
        keys.push((KIND_SPACES, space_id));
        self.delete_row_ops(&keys);
        Ok(DeletedSpace { existed, chat_ids })
    }

    // ── chats ───────────────────────────────────────────────────────────────

    pub fn upsert_chat(&mut self, chat: &Chat) -> Result<(), DocError> {
        let config = match &chat.config {
            Some(config) => serde_json::to_value(config)?,
            None => Value::Null,
        };
        let set = fields([
            ("id", json!(chat.id)),
            ("deviceId", json!(chat.device_id)),
            ("title", opt_str(chat.title.as_deref())),
            ("archived", json!(chat.archived)),
            ("cwd", opt_str(chat.cwd.as_deref())),
            ("branch", opt_str(chat.branch.as_deref())),
            ("checkoutId", opt_str(chat.checkout_id.as_deref())),
            (
                "sourceContext",
                chat.source_context
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?
                    .unwrap_or(Value::Null),
            ),
            ("config", config),
            (
                "lastMessagePreview",
                opt_str(chat.last_message_preview.as_deref()),
            ),
            ("lastMessageAt", opt_ms(chat.last_message_at)),
            ("createdAt", json!(chat.created_at.timestamp_millis())),
            (
                "harnessSessionId",
                opt_str(chat.harness_session_id.as_deref()),
            ),
            (
                "harnessSessionCwd",
                opt_str(chat.harness_session_cwd.as_deref()),
            ),
            ("spaceId", opt_str(chat.space_id.as_deref())),
            ("lastSeenAt", opt_ms(chat.last_seen_at)),
        ]);
        self.write(KIND_CHATS, &chat.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    /// Claim-on-first-command row: ONLY the fields the claim actually knows
    /// (identity, cwd, space). Absent fields carry no clocked write, so the
    /// real `createChat` upsert — racing behind on the registry channel with
    /// older clocks — still lands its `config`/`title` under per-field LWW
    /// instead of losing them to `Null` deletes.
    pub fn claim_chat(
        &mut self,
        chat_id: &str,
        cwd: Option<&str>,
        space_id: Option<&str>,
        created_at: DateTime<Utc>,
    ) {
        let mut set = fields([
            ("id", json!(chat_id)),
            ("deviceId", json!(self.device_id())),
            ("createdAt", json!(created_at.timestamp_millis())),
        ]);
        if let Some(cwd) = cwd {
            set.insert("cwd".into(), json!(cwd));
        }
        if let Some(space_id) = space_id {
            set.insert("spaceId".into(), json!(space_id));
        }
        self.write(KIND_CHATS, chat_id, OpKind::Upsert, set);
    }

    /// Seen marker (LWW) with a monotonic guard: no write when the
    /// stored stamp is already >= `at`.
    pub fn set_chat_seen(&mut self, chat_id: &str, at: DateTime<Utc>) -> Result<bool, DocError> {
        let Some(row) = self.overlay_row(KIND_CHATS, chat_id) else {
            return Ok(false);
        };
        let current = row.fields.get("lastSeenAt").and_then(Value::as_i64);
        if current.is_some_and(|ms| ms >= at.timestamp_millis()) {
            return Ok(true);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("lastSeenAt", json!(at.timestamp_millis()))]),
        );
        Ok(true)
    }

    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, DocError> {
        Ok(self
            .overlay_row(KIND_CHATS, chat_id)
            .and_then(|row| row_to::<RawChat>(&row))
            .map(Chat::from))
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, DocError> {
        let mut chats: Vec<Chat> = self
            .read_kind::<RawChat>(KIND_CHATS)
            .into_iter()
            .map(Chat::from)
            .collect();
        chats.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(chats)
    }

    pub fn rename_chat(&mut self, chat_id: &str, title: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("title", json!(title))]),
        );
        Ok(true)
    }

    pub fn set_chat_archived(&mut self, chat_id: &str, archived: bool) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("archived", json!(archived))]),
        );
        Ok(true)
    }

    pub fn set_chat_branch(&mut self, chat_id: &str, branch: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("branch", json!(branch))]),
        );
        Ok(true)
    }

    pub fn set_chat_source_context(
        &mut self,
        chat_id: &str,
        context: &zeron_proto::ConversationSourceContext,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("sourceContext", serde_json::to_value(context)?),
                ("branch", json!(context.branch)),
                ("checkoutId", json!(context.checkout_id)),
            ]),
        );
        Ok(true)
    }

    /// Retarget the chat onto another folder — the mid-session "switch to an
    /// existing worktree" move. Harness resume is cwd-scoped, so the next run
    /// in the new folder starts a fresh harness conversation by design.
    pub fn set_chat_cwd(&mut self, chat_id: &str, cwd: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("cwd", json!(cwd))]),
        );
        Ok(true)
    }

    pub fn set_chat_checkout(
        &mut self,
        chat_id: &str,
        checkout_id: &str,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("checkoutId", json!(checkout_id))]),
        );
        Ok(true)
    }

    pub fn set_chat_config(
        &mut self,
        chat_id: &str,
        config: &ChatConfig,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        let value = serde_json::to_value(config)?;
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("config", value)]),
        );
        Ok(true)
    }

    /// Host-side resume continuity. An empty `session_id` is the explicit
    /// "do not resume" tombstone written after a harness rejects a resume.
    pub fn set_chat_harness_session(
        &mut self,
        chat_id: &str,
        session_id: &str,
        cwd: &str,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("harnessSessionId", json!(session_id)),
                ("harnessSessionCwd", json!(cwd)),
            ]),
        );
        Ok(true)
    }

    /// Host-side sidebar freshness: preview + timestamp of the latest message.
    pub fn set_chat_last_message(
        &mut self,
        chat_id: &str,
        preview: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("lastMessagePreview", json!(preview)),
                ("lastMessageAt", json!(at.timestamp_millis())),
            ]),
        );
        Ok(true)
    }

    /// Tombstone: delete the chat row (and its session-status row). The
    /// per-chat session doc remains — this removes the index entry only.
    pub fn delete_chat(&mut self, chat_id: &str) -> Result<bool, DocError> {
        let existed = self.row_exists(KIND_CHATS, chat_id);
        self.delete_row_ops(&[(KIND_CHATS, chat_id), (KIND_SESSIONS, chat_id)]);
        Ok(existed)
    }

    // ── sessions ────────────────────────────────────────────────────────────

    /// Upsert a session-status row (writer discipline: each device writes only
    /// its own runs' rows). Staleness is checked client-side via `updatedAt`.
    pub fn upsert_session(&mut self, session: &Session) -> Result<(), DocError> {
        let set = fields([
            ("chatId", json!(session.chat_id)),
            ("deviceId", json!(session.device_id)),
            ("status", serde_json::to_value(session.status)?),
            ("startedAt", opt_ms(session.started_at)),
            ("updatedAt", json!(session.updated_at.timestamp_millis())),
        ]);
        self.write(KIND_SESSIONS, &session.chat_id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, DocError> {
        let mut sessions: Vec<Session> = self
            .read_kind::<RawSession>(KIND_SESSIONS)
            .into_iter()
            .map(Session::from)
            .collect();
        sessions.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        Ok(sessions)
    }

    // ── whole-doc read ──────────────────────────────────────────────────────

    pub fn read_all(&self) -> Result<WorkspaceState, DocError> {
        Ok(WorkspaceState {
            devices: self.read_devices()?,
            spaces: self.read_spaces()?,
            chats: self.read_chats()?,
            sessions: self.read_sessions()?,
        })
    }
}

// ── field helpers ───────────────────────────────────────────────────────────

fn fields<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn opt_str(value: Option<&str>) -> Value {
    match value {
        Some(v) => json!(v),
        None => Value::Null,
    }
}

fn opt_ms(value: Option<DateTime<Utc>>) -> Value {
    match value {
        Some(at) => json!(at.timestamp_millis()),
        None => Value::Null,
    }
}

fn row_to<T: serde::de::DeserializeOwned>(row: &RegistryRow) -> Option<T> {
    let value = Value::Object(row.fields.clone().into_iter().collect());
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            tracing::warn!(kind = %row.kind, row = %row.id, error = %err, "skipping malformed registry row");
            None
        }
    }
}

// SessionStatus needs to serialize to the same strings the loro doc used
// ("idle"/"working"/…) — zeron_proto's serde derives already use camelCase;
// the compile-time check lives in the tests below.

#[cfg(test)]
mod tests;
