//! Local engine identity shared by the engine and its clients.

use serde::{Deserialize, Serialize};

/// Stable information about the local engine runtime reached by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    pub device_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_info_uses_camel_case_fields() {
        let info = EngineInfo {
            device_id: "device-1".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({"deviceId":"device-1"})
        );
    }
}
