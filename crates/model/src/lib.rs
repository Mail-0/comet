pub mod motion;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRuntime {
    Cloud,
    Local,
}

impl AgentRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingStatus {
    Inactive,
    Pending,
    Active,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    NeedsAttention,
    Running,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    pub runtime: AgentRuntime,
    pub line_number: Option<String>,
    pub routing_status: Option<RoutingStatus>,
    pub routing_message: Option<String>,
    pub active: bool,
    pub tool_count: u32,
    pub last_seen_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsResponse {
    pub agents: Vec<AgentSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTemplateSecret {
    pub name: String,
    pub required: bool,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTemplateCapability {
    pub slug: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTemplateSummary {
    pub id: String,
    pub name: String,
    pub emoji: String,
    pub blurb: String,
    pub description: String,
    pub model: String,
    pub secrets: Vec<AgentTemplateSecret>,
    pub connections: Vec<String>,
    pub capabilities: Vec<AgentTemplateCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTemplatesResponse {
    pub templates: Vec<AgentTemplateSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgentFromTemplate {
    pub template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub ok: bool,
    pub id: String,
    #[serde(default)]
    pub missing_secrets: Vec<AgentTemplateSecret>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentHarness {
    Flue,
    Hermes,
}

impl AgentHarness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flue => "flue",
            Self::Hermes => "hermes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFeatures {
    pub memory: bool,
    pub steering: bool,
    pub media: bool,
    pub browser: bool,
    pub scrape: bool,
    pub sandbox: bool,
    pub mcp: bool,
    pub escalation: bool,
    pub loops: bool,
    pub guards: bool,
    pub wallet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EscalationRoutes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackEscalationRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackEscalationRoute {
    pub channel: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInput {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub max_steps: u32,
    pub history_limit: u32,
    pub reasoning_effort: ReasoningEffort,
    pub line_number: Option<String>,
    pub harness: AgentHarness,
    pub features: AgentFeatures,
    pub escalation_routes: EscalationRoutes,
    pub skill_ids: Vec<String>,
    pub sandbox_script_ids: Vec<String>,
    pub sandbox_env_secrets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_mode: Option<StorageMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentEditResponse {
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub model: String,
    pub runtime: AgentRuntime,
    pub system_prompt: String,
    pub max_steps: u32,
    pub history_limit: u32,
    pub reasoning_effort: ReasoningEffort,
    pub line_number: Option<String>,
    pub storage_mode: String,
    pub harness: AgentHarness,
    pub features: AgentFeatures,
    pub escalation_routes: EscalationRoutes,
    pub skill_ids: Vec<String>,
    pub sandbox_script_ids: Vec<String>,
    pub sandbox_env_secrets: Vec<String>,
}

impl AgentSummary {
    pub fn status(&self) -> AgentStatus {
        if !self.active {
            AgentStatus::Offline
        } else if matches!(
            self.routing_status,
            Some(RoutingStatus::Pending | RoutingStatus::Error)
        ) {
            AgentStatus::NeedsAttention
        } else {
            AgentStatus::Running
        }
    }
}

pub fn sort_agents(agents: &mut [AgentSummary]) {
    agents.sort_by(|left, right| {
        status_rank(left.status())
            .cmp(&status_rank(right.status()))
            .then_with(|| right.last_seen_at.cmp(&left.last_seen_at))
            .then_with(|| left.name.cmp(&right.name))
    });
}

impl AgentFeatures {
    pub fn enabled_count(&self) -> usize {
        [
            self.memory,
            self.steering,
            self.media,
            self.browser,
            self.scrape,
            self.sandbox,
            self.mcp,
            self.escalation,
            self.loops,
            self.guards,
            self.wallet,
        ]
        .into_iter()
        .filter(|enabled| *enabled)
        .count()
    }
}

fn status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::NeedsAttention => 0,
        AgentStatus::Running => 1,
        AgentStatus::Offline => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(
        name: &str,
        active: bool,
        routing_status: Option<RoutingStatus>,
        minute: u32,
    ) -> AgentSummary {
        AgentSummary {
            id: name.to_lowercase(),
            name: name.into(),
            model: "google/gemini-3.5-flash".into(),
            runtime: AgentRuntime::Cloud,
            line_number: None,
            routing_status,
            routing_message: None,
            active,
            tool_count: 0,
            last_seen_at: format!("2026-08-28T12:{minute:02}:00Z"),
            created_at: "2026-08-20T12:00:00Z".into(),
        }
    }

    #[test]
    fn attention_agents_sort_before_recent_activity() {
        let mut agents = vec![
            agent("Offline", false, None, 59),
            agent("Attention", true, Some(RoutingStatus::Error), 1),
            agent("Running", true, Some(RoutingStatus::Active), 58),
        ];

        sort_agents(&mut agents);

        assert_eq!(
            agents
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            ["Attention", "Running", "Offline"]
        );
    }

    #[test]
    fn agents_with_the_same_status_sort_by_recent_activity() {
        let mut agents = vec![
            agent("Older", true, None, 1),
            agent("Newer", true, None, 59),
        ];

        sort_agents(&mut agents);

        assert_eq!(agents[0].name, "Newer");
    }

    #[test]
    fn agent_and_template_payloads_decode_without_losing_contract_fields() {
        let agents: AgentsResponse = serde_json::from_value(serde_json::json!({
            "agents": [{
                "id": "agent-1",
                "name": "Orchid",
                "model": "google/gemini-3.5-flash",
                "runtime": "cloud",
                "lineNumber": null,
                "routingStatus": "pending",
                "routingMessage": "Waiting for webhook",
                "active": true,
                "toolCount": 3,
                "lastSeenAt": "2026-08-28T12:00:00Z",
                "createdAt": "2026-08-20T12:00:00Z"
            }]
        }))
        .unwrap();
        assert_eq!(agents.agents[0].runtime, AgentRuntime::Cloud);
        assert_eq!(
            agents.agents[0].routing_status,
            Some(RoutingStatus::Pending)
        );
        assert_eq!(agents.agents[0].tool_count, 3);

        let templates: AgentTemplatesResponse = serde_json::from_value(serde_json::json!({
            "templates": [{
                "id": "orchid",
                "name": "Orchid",
                "emoji": "🌸",
                "blurb": "Personal assistant",
                "description": "A complete assistant",
                "model": "google/gemini-3.5-flash",
                "secrets": [{
                    "name": "GOOGLE_OAUTH_CLIENT_ID",
                    "required": true,
                    "description": "Google client id"
                }],
                "connections": ["gmail", "calendar"],
                "capabilities": [{"slug": "orchid", "version": "1.0.0"}]
            }]
        }))
        .unwrap();
        assert_eq!(templates.templates[0].secrets.len(), 1);
        assert_eq!(templates.templates[0].connections, ["gmail", "calendar"]);

        let created: CreateAgentResponse = serde_json::from_value(serde_json::json!({
            "ok": true,
            "id": "agent-2",
            "missing_secrets": [{
                "name": "GOOGLE_OAUTH_CLIENT_ID",
                "required": true,
                "description": "Google client id"
            }]
        }))
        .unwrap();
        assert_eq!(created.id, "agent-2");
        assert_eq!(created.missing_secrets[0].name, "GOOGLE_OAUTH_CLIENT_ID");

        let config: AgentEditResponse = serde_json::from_value(serde_json::json!({
            "agent": {
                "id": "agent-1",
                "apiKey": "not-retained-by-the-client-model",
                "name": "Orchid",
                "model": "google/gemini-3.5-flash",
                "runtime": "cloud",
                "systemPrompt": "Be helpful",
                "maxSteps": 25,
                "historyLimit": 50,
                "reasoningEffort": "medium",
                "lineNumber": null,
                "storageMode": "managed",
                "harness": "flue",
                "harnessConfig": null,
                "features": {
                    "memory": true,
                    "steering": true,
                    "media": true,
                    "browser": true,
                    "scrape": true,
                    "sandbox": true,
                    "mcp": true,
                    "escalation": true,
                    "loops": true,
                    "guards": true,
                    "wallet": false
                },
                "escalationRoutes": {},
                "skillIds": ["skill-1"],
                "sandboxScriptIds": ["script-1"],
                "sandboxEnvSecrets": ["DATABASE_URL"],
                "mcpPresets": []
            },
            "lines": []
        }))
        .unwrap();
        assert_eq!(config.agent.max_steps, 25);
        assert_eq!(config.agent.skill_ids, ["skill-1"]);
        assert!(config.agent.features.browser);
    }
}
