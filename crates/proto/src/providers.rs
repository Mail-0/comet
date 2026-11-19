//! Copilot is authenticated by the engine's Keiki credential holder.

use crate::HarnessId;

pub const PROVIDERS: [(HarnessId, &str, &str); 1] = [
    (HarnessId::Copilot, "Copilot", "")];

pub fn signs_in(_harness: HarnessId) -> bool {
    false
}

pub fn provider_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Copilot => "Copilot",
        HarnessId::Mock => "Mock",
        HarnessId::Unknown(_) => "Unavailable agent",
    }
}
pub fn reports_usage(_harness: HarnessId) -> bool {
    false
}
pub fn keeps_one_login(harness: HarnessId) -> bool {
    harness == HarnessId::Copilot
}
pub fn switches_accounts(_harness: HarnessId) -> bool {
    false
}
pub fn provider_note(_harness: HarnessId) -> Option<&'static str> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginOption {
    pub provider: Option<&'static str>,
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiKeyOption {
    pub provider: &'static str,
    pub label: &'static str,
}

pub fn login_options(_harness: HarnessId) -> Vec<LoginOption> {
    Vec::new()
}
pub fn api_key_options(_harness: HarnessId) -> Vec<ApiKeyOption> {
    Vec::new()
}
pub fn api_key_option(_harness: HarnessId, _provider: &str) -> Option<ApiKeyOption> {
    None
}
pub fn add_key_label(option: ApiKeyOption, in_group: bool) -> String {
    if in_group {
        "Add API key".into()
    } else {
        format!("Add {} API key", option.label)
    }
}
pub fn groups_by_provider(_harness: HarnessId) -> bool {
    false
}
pub fn login_option(_harness: HarnessId, _provider: &str) -> Option<LoginOption> {
    None
}
pub fn account_group_label(harness: HarnessId, _provider: &str) -> String {
    provider_name(harness).into()
}
pub fn account_group_hint(_harness: HarnessId, _provider: &str) -> Option<String> {
    None
}
pub fn add_account_label(_harness: HarnessId, _empty: bool) -> String {
    "Sign in to Keiki".into()
}
pub fn add_option_label(_harness: HarnessId, option: LoginOption, _empty: bool) -> String {
    option.label.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_copilot_is_listed_without_cli_login_or_keys() {
        assert_eq!(PROVIDERS, [(HarnessId::Copilot, "Copilot", "")]);
        assert!(!signs_in(HarnessId::Copilot));
        assert!(login_options(HarnessId::Copilot).is_empty());
        assert!(api_key_options(HarnessId::Copilot).is_empty());
    }
}
