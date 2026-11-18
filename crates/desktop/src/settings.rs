use serde_json::{Value, json};
use url::Url;

#[derive(Default)]
pub struct Settings {
    route: String,
    deep_link: Option<String>,
}

impl Settings {
    pub fn new(route: Option<&str>, launch_url: Option<&str>) -> Self {
        let mut state = Self {
            route: match route {
                Some("settings" | "settings/devices" | "settings/appearance") => {
                    "settings/appearance"
                }
                Some("settings/notifications") => "settings/notifications",
                Some("settings/shortcuts") => "settings/shortcuts",
                Some("settings/archived") => "settings/archived",
                Some("new") => "new",
                _ => "chat",
            }
            .into(),
            deep_link: None,
        };
        if let Some(url) = launch_url {
            state.open_url(url);
        }
        state
    }

    pub fn open_url(&mut self, value: &str) -> bool {
        let Ok(mut url) = Url::parse(value) else {
            return false;
        };
        if url.scheme() != "keiki" || url.host_str() == Some("oauth") {
            return false;
        }
        url.set_query(None);
        url.set_fragment(None);
        self.deep_link = Some(url.to_string());
        self.route = "chat".into();
        true
    }

    pub fn snapshot(&self) -> Value {
        json!({"route": self.route, "deep_link": self.deep_link})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_routes_and_links_are_state_only() {
        assert_eq!(
            Settings::new(Some("settings/devices"), None).snapshot()["route"],
            "settings/appearance"
        );
        assert_eq!(Settings::new(Some("new"), None).snapshot()["route"], "new");
        let state = Settings::new(None, Some("keiki://conversation/chat-1"));
        assert_eq!(state.snapshot()["deep_link"], "keiki://conversation/chat-1");
    }

    #[test]
    fn invalid_and_oauth_urls_do_not_publish_secrets() {
        let mut state = Settings::new(None, None);
        for url in [
            "",
            "not a URL",
            "https://onkeiki.com",
            "keiki://oauth/callback?code=secret",
        ] {
            assert!(!state.open_url(url));
        }
        assert!(state.snapshot()["deep_link"].is_null());
        assert!(state.open_url("keiki://conversation/chat?token=secret#secret"));
        assert_eq!(state.snapshot()["deep_link"], "keiki://conversation/chat");
    }
}
