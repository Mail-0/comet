//! Copilot catalogs and cached labels from the current run device.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessInfo {
    /// Wire id (`claude-code`).
    pub id: String,
    /// Display name.
    #[serde(alias = "name")]
    pub label: String,
    #[serde(default)]
    pub supports_steering: Option<bool>,
    /// `step-boundary` / `turn-boundary`.
    #[serde(default)]
    pub steering_mode: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// CLI present on the listing device (absent on old engines ⇒ true).
    #[serde(default = "default_true")]
    pub installed: bool,
    /// Offered by the listing device (Settings → Providers). `None` ⇒ legacy.
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl HarnessInfo {
    fn fallback(id: &str, label: &str) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            supports_steering: None,
            steering_mode: None,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        }
    }

    /// Mid-turn steering is available (known only from a live catalog).
    pub fn mid_turn_steering(&self) -> Option<bool> {
        Some(self.supports_steering? && self.steering_mode.as_deref()? == "step-boundary")
    }

    /// Listed by the device and usable (installed, not disabled).
    pub fn offered(&self) -> bool {
        self.installed && self.enabled != Some(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Lowercase wire values (`low`…`ultrathink`); empty = no effort ladder.
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

fn model(
    id: &str,
    label: &str,
    description: &str,
    ladder: &[&str],
    options: Vec<ModelOption>,
) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        reasoning_levels: ladder.iter().map(|s| (*s).to_owned()).collect(),
        options,
    }
}

pub fn harnesses_without_asking(device_id: &str) -> Option<Vec<HarnessInfo>> {
    zeron_proto::is_cloud_device(device_id, "").then(fallback_harnesses)
}

pub fn fallback_harnesses() -> Vec<HarnessInfo> {
    vec![
        HarnessInfo::fallback("copilot", "Copilot")]
}

/// Display name for any harness id the fleet can produce.
pub fn harness_label(id: &str) -> String {
    match id {
        "copilot" => "Copilot",
        "mock" => "Mock",
        _ => "Unavailable agent",
    }
    .into()
}

/// Curated static models for a harness (first row = default).
pub fn fallback_models(harness: &str) -> Vec<ModelInfo> {
    if matches!(harness, "copilot" | "mock") {
        vec![model(
            "copilot",
            "Copilot",
            "Keiki Copilot",
            &[],
            Vec::new(),
        )]
    } else {
        Vec::new()
    }
}

pub fn default_reasoning(model: &ModelInfo) -> Option<String> {
    let levels = &model.reasoning_levels;
    if levels.is_empty() {
        return None;
    }
    for preferred in ["high", "medium"] {
        if levels.iter().any(|l| l == preferred) {
            return Some(preferred.into());
        }
    }
    levels.first().cloned()
}

pub fn reasoning_label(level: &str) -> String {
    match level {
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "X-High".into(),
        "max" => "Max".into(),
        "ultra" => "Ultra".into(),
        "ultracode" => "Ultracode".into(),
        "ultrathink" => "Ultrathink".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    }
}

pub fn normalize_models(_harness: &str, models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models
}

static LEARNED_LABELS: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<(String, String), String>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn learn_labels(harness: &str, models: &[ModelInfo]) {
    let Ok(mut map) = LEARNED_LABELS.write() else {
        return;
    };
    for m in models {
        if !m.label.trim().is_empty() && m.label != m.id {
            map.insert((harness.to_owned(), m.id.clone()), m.label.clone());
        }
    }
}

fn learned_label(harness: &str, model_id: &str) -> Option<String> {
    let map = LEARNED_LABELS.read().ok()?;
    map.get(&(harness.to_owned(), model_id.to_owned()))
        .cloned()
}

/// Human label for a chat's model id (chip / row subtitle).
pub fn model_label(harness: &str, model_id: &str) -> String {
    learned_label(harness, model_id)
        .or_else(|| {
            fallback_models(harness)
                .into_iter()
                .find(|m| m.id == model_id)
                .map(|m| m.label)
        })
        .unwrap_or_else(|| model_id.to_owned())
}

/// Last-known live catalogs on disk (`{data_dir}/catalogs/{device}.json`):
/// pickers open instantly with the device's real list, and an unreachable
/// device still shows what it offered last time before the static fallback.
#[derive(Debug, Clone)]
pub(crate) struct DiskCatalog {
    dir: std::path::PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceCatalog {
    #[serde(default)]
    harnesses: Option<Vec<HarnessInfo>>,
    #[serde(default)]
    models: std::collections::BTreeMap<String, Vec<ModelInfo>>,
}

fn file_safe(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl DiskCatalog {
    pub(crate) fn new(data_dir: &std::path::Path) -> Self {
        Self {
            dir: data_dir.join("catalogs"),
        }
    }

    fn path(&self, device_id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{}.json", file_safe(device_id)))
    }

    fn load(&self, device_id: &str) -> DeviceCatalog {
        std::fs::read(self.path(device_id))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn store(&self, device_id: &str, catalog: &DeviceCatalog) {
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            let path = self.path(device_id);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(catalog).unwrap_or_default())?;
            std::fs::rename(tmp, path)
        };
        if let Err(err) = write() {
            tracing::debug!(error = %err, "catalog cache write failed");
        }
    }

    pub(crate) fn harnesses(&self, device_id: &str) -> Option<Vec<HarnessInfo>> {
        self.load(device_id).harnesses
    }

    pub(crate) fn put_harnesses(&self, device_id: &str, list: &[HarnessInfo]) {
        let mut catalog = self.load(device_id);
        catalog.harnesses = Some(list.to_vec());
        self.store(device_id, &catalog);
    }

    pub(crate) fn models(&self, device_id: &str, harness: &str) -> Option<Vec<ModelInfo>> {
        self.load(device_id).models.remove(harness)
    }

    /// Teach [`model_label`] every label cached on disk (client start).
    pub(crate) fn warm_labels(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(catalog) = serde_json::from_slice::<DeviceCatalog>(&bytes) else {
                continue;
            };
            for (harness, models) in &catalog.models {
                learn_labels(harness, models);
            }
        }
    }

    pub(crate) fn put_models(&self, device_id: &str, harness: &str, list: &[ModelInfo]) {
        learn_labels(harness, list);
        let mut catalog = self.load(device_id);
        catalog.models.insert(harness.to_owned(), list.to_vec());
        self.store(device_id, &catalog);
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_cloud_device_s_agents_are_known_without_asking_it() {
        let ids = |d: &str| {
            super::harnesses_without_asking(d).map(|l| l.into_iter().map(|h| h.id).collect::<Vec<_>>())
        };
        assert_eq!(ids("cloud-s-chat-1"), Some(vec!["copilot".to_string()]));
        assert_eq!(ids("mac-123"), None);
    }

    use super::*;

    #[test]
    fn labels_resolve_copilot_and_unknown_models() {
        assert_eq!(model_label("copilot", "copilot"), "Copilot");
        assert_eq!(model_label("copilot", "new-model"), "new-model");
        assert!(fallback_models("retired").is_empty());
    }

    #[test]
    fn live_catalog_labels_replace_raw_ids() {
        assert_eq!(model_label("cursor-test", "sonnet-9-thinking"),
            "sonnet-9-thinking"
        );
        learn_labels(
            "cursor-test",
            &[model(
                "sonnet-9-thinking",
                "Sonnet 9 Thinking",
                "x",
                &[],
                vec![],
            )],
        );
        assert_eq!(model_label("cursor-test", "sonnet-9-thinking"),
            "Sonnet 9 Thinking"
        );
    }

    #[test]
    fn disk_catalog_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCatalog::new(dir.path());
        assert!(cache.harnesses("dev/mac").is_none());
        cache.put_harnesses("dev/mac", &fallback_harnesses());
        cache.put_models("dev/mac", "copilot", &fallback_models("copilot"));
        assert_eq!(cache.harnesses("dev/mac").unwrap().len(), 1);
        assert_eq!(cache.models("dev/mac", "copilot").unwrap()[0].id, "copilot");
        assert!(cache.models("dev/mac", "grok").is_none());
    }
}
