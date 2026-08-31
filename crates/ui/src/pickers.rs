//! Composer configuration resolution and selected-session footer rendering.

use gpui::{AnyElement, App, Context, Entity, SharedString, Window, div, prelude::*, px};

use zeron_proto::{ChatConfig, FolderListing, HarnessId, ReasoningLevel, SandboxLevel};

use crate::state::AppState;
use crate::theme::Theme;

/// The fully-resolved run configuration used by the composer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The chat configuration carried by a newly created session.
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

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
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

/// Byte length of `name`'s prefix matching `query`, compared case-insensitively.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for query_char in query.chars() {
        let name_char = name_chars.next()?;
        if !name_char.to_lowercase().eq(query_char.to_lowercase()) {
            return None;
        }
        len += name_char.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder names.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(index) = names.iter().position(|name| *name == query) {
        return Some(index);
    }
    if let Some(index) = names
        .iter()
        .position(|name| completion_prefix_len(name, query) == Some(name.len()))
    {
        return Some(index);
    }
    let mut matches = names
        .iter()
        .enumerate()
        .filter(|(_, name)| completion_prefix_len(name, query).is_some());
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

/// Interpret a palette query as an absolute or home-relative path.
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
    let mut result = vec![("/".to_string(), "/".to_string())];
    let mut accumulated = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        accumulated.push('/');
        accumulated.push_str(segment);
        result.push((segment.to_string(), accumulated.clone()));
    }
    result
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&zeron_proto::FolderEntry> {
    listing
        .entries
        .iter()
        .filter(|entry| entry.is_dir)
        .collect()
}

pub struct Pickers {
    state: Entity<AppState>,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }

    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        self.state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
            .or(Some(HarnessId::Copilot))
    }

    fn effective_model_id(&self, cx: &App) -> Option<String> {
        self.state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.as_ref().and_then(|config| config.model.clone()))
            .or_else(|| Some("copilot".to_string()))
    }

    fn effective_reasoning(&self, _cx: &App) -> Option<ReasoningLevel> {
        None
    }

    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        self.state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.as_ref())
            .map(|config| config.model_options.clone())
            .unwrap_or_default()
    }

    pub fn resolved_steering_mode(&self, cx: &App) -> Option<zeron_proto::SteeringMode> {
        (self.effective_harness(cx) == Some(HarnessId::Copilot))
            .then_some(zeron_proto::SteeringMode::TurnBoundary)
    }

    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self.effective_model_id(cx),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    /// Read-only checkout and ref information for the selected session.
    pub fn render_footer(&self, cx: &App) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let (space, session, change_request) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state.selected_chat_row().cloned();
            let change_request = session
                .as_ref()
                .and_then(|chat| state.change_request_for_chat(chat).cloned());
            (space, session, change_request)
        };
        let space = space.as_ref().filter(|space| space.git_detected)?;
        let chat = session?;
        let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
        let (icon_path, label) = if is_worktree {
            (crate::icons::FOLDER_WITH_FILES, "Worktree")
        } else {
            (crate::icons::FOLDER, "Local checkout")
        };
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
            .when_some(change_request, |element, summary| {
                element.child(crate::change_requests::pull_request_badge(
                    "composer-pull-request".into(),
                    summary,
                    crate::change_requests::ChangeRequestBadgeSurface::Composer,
                    &theme,
                ))
            })
            .child(Self::footer_label(
                crate::icons::GIT_BRANCH,
                chat.branch
                    .map(SharedString::from)
                    .unwrap_or_else(|| SharedString::from("No ref")),
                &theme,
            ));
        Some(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .mb(px(-8.0))
                .child(left)
                .child(right)
                .into_any_element(),
        )
    }

    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
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
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::Copilot | HarnessId::Mock | HarnessId::Unknown(_) => {
            (crate::icons::MONITOR, None)
        }
    }
}

impl gpui::Render for Pickers {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let labels: Vec<&str> = crumbs.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn completion_prefix_lengths() {
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("zeron", "zeron"), Some(5));
        assert_eq!(completion_prefix_len("zeron", ""), Some(0));
        assert_eq!(completion_prefix_len("zeron", "dev"), None);
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
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
        assert_eq!(typed_path_target("~x", home), None);
        assert_eq!(typed_path_target("src", home), None);
        assert_eq!(typed_path_target("~/github", None), None);
        assert_eq!(typed_path_target("/disk2", None), Some("/disk2".into()));
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                zeron_proto::FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                zeron_proto::FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                zeron_proto::FolderEntry {
                    name: "zeron".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
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
