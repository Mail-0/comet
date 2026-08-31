//! Sticky composer defaults — the new-chat "remember my last picks" store
//! (zeron parity: localStorage `zeron.composer.defaults:v1`, defaults.ts).
//!
//! A small JSON file beside `ui-settings.json` stores the last target
//! selections. It is written synchronously when a target changes; corrupt or
//! missing files fall back to defaults.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "composer-defaults.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ComposerDefaults {
    /// Last device picked for new sessions (the composer's device selector).
    pub device: Option<String>,
    /// Last project picked for new sessions.
    pub project: Option<String>,
    /// Remembered "Don't work in a project" opt-out.
    pub no_project: bool,
}

impl ComposerDefaults {
    /// Load from `{data_dir}/composer-defaults.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<ComposerDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "composer-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = ComposerDefaults {
            device: Some("local".into()),
            project: Some("project".into()),
            ..Default::default()
        };
        defaults.save(dir.path()).unwrap();
        let loaded = ComposerDefaults::load(dir.path());
        assert_eq!(loaded, defaults);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
        std::fs::write(ComposerDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
    }
}
