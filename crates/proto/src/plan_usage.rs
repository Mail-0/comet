use crate::{AgentUsageWindow, HarnessId};

pub fn from_reply(
    _harness: HarnessId,
    _body: &serde_json::Value,
) -> Option<(Vec<AgentUsageWindow>, Option<String>)> {
    None
}
