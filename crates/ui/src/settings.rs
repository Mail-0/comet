use std::path::{Path, PathBuf};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "ui-settings.json";

pub struct SettingsStore {
    current: UiSettings,
    data_dir: PathBuf,
}

impl Global for SettingsStore {}

pub fn init(settings: UiSettings, data_dir: impl Into<PathBuf>, cx: &mut App) {
    cx.set_global(SettingsStore {
        current: settings,
        data_dir: data_dir.into(),
    });
}

pub fn update(cx: &mut App, mutate: impl FnOnce(&mut UiSettings)) -> bool {
    let Some(store) = cx.try_global::<SettingsStore>() else {
        return false;
    };
    let before = store.current.clone();
    let store = cx.global_mut::<SettingsStore>();
    mutate(&mut store.current);
    if store.current == before {
        return false;
    }
    if let Err(error) = store.current.save(&store.data_dir) {
        tracing::warn!(%error, "failed to persist UI settings");
    }
    true
}

pub fn flush(cx: &mut App) {
    let Some(store) = cx.try_global::<SettingsStore>() else {
        return;
    };
    if let Err(error) = store.current.save(&store.data_dir) {
        tracing::warn!(%error, "failed to persist UI settings");
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub appearance: crate::appearance::AppearanceMode,
    pub ui_font_family: crate::typography::UiFontFamily,
    pub ui_font_size: crate::typography::UiFontSize,
    pub theme_selection: keiki_theme::ThemeSelection,
    pub accent: keiki_theme::AccentSelection,
    pub surface: keiki_theme::SurfacePreference,
}

impl UiSettings {
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse UI settings");
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to read UI settings");
                Self::default()
            }
        }
    }

    fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let encoded = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(data_dir), encoded)
    }

    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_settings_use_defaults() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(UiSettings::load(directory.path()), UiSettings::default());
    }

    #[test]
    fn settings_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let settings = UiSettings {
            appearance: crate::appearance::AppearanceMode::Dark,
            ..UiSettings::default()
        };

        settings.save(directory.path()).unwrap();

        assert_eq!(UiSettings::load(directory.path()), settings);
    }

    #[test]
    fn old_settings_ignore_removed_fields() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(directory.path()),
            r#"{"terminalOpen":true,"sidebarWidth":320}"#,
        )
        .unwrap();

        assert_eq!(UiSettings::load(directory.path()), UiSettings::default());
    }
}
