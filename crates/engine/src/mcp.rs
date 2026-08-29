use std::sync::{Arc, Mutex};

use zeron_harness::McpServerSpec;

#[derive(Clone, Default)]
pub struct McpServerHolder {
    servers: Arc<Mutex<Vec<McpServerSpec>>>,
}

impl McpServerHolder {
    pub fn set(&self, servers: Vec<McpServerSpec>) {
        let mut current_servers = match self.servers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *current_servers = servers;
    }

    pub fn clear(&self) {
        self.set(Vec::new());
    }

    pub fn snapshot(&self) -> Vec<McpServerSpec> {
        let current_servers = match self.servers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        current_servers.clone()
    }
}

static MCP_SERVERS: std::sync::OnceLock<McpServerHolder> = std::sync::OnceLock::new();

pub fn mcp_servers() -> &'static McpServerHolder {
    MCP_SERVERS.get_or_init(McpServerHolder::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_sets_clears_and_snapshots_without_exposing_tokens_in_debug() {
        let holder = McpServerHolder::default();
        holder.set(vec![McpServerSpec {
            name: "keiki".into(),
            url: "https://onkeiki.com/mcp".into(),
            bearer_token: "secret-token".into(),
        }]);
        let snapshot = holder.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].bearer_token, "secret-token");
        assert!(!format!("{snapshot:?}").contains("secret-token"));
        holder.clear();
        assert!(holder.snapshot().is_empty());
    }
}
