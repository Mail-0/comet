pub mod motion;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    NeedsAttention,
    Running,
    Idle,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub status: AgentStatus,
    pub updated_at: DateTime<Utc>,
}

pub fn sort_agents(agents: &mut [AgentSummary]) {
    agents.sort_by(|left, right| {
        status_rank(left.status)
            .cmp(&status_rank(right.status))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::NeedsAttention => 0,
        AgentStatus::Running => 1,
        AgentStatus::Idle => 2,
        AgentStatus::Offline => 3,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn agent(name: &str, status: AgentStatus, minute: u32) -> AgentSummary {
        AgentSummary {
            id: name.to_lowercase(),
            name: name.into(),
            status,
            updated_at: Utc.with_ymd_and_hms(2026, 8, 28, 12, minute, 0).unwrap(),
        }
    }

    #[test]
    fn attention_agents_sort_before_recent_activity() {
        let mut agents = vec![
            agent("Idle", AgentStatus::Idle, 59),
            agent("Attention", AgentStatus::NeedsAttention, 1),
            agent("Running", AgentStatus::Running, 58),
        ];

        sort_agents(&mut agents);

        assert_eq!(
            agents
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            ["Attention", "Running", "Idle"]
        );
    }

    #[test]
    fn agents_with_the_same_status_sort_by_recent_activity() {
        let mut agents = vec![
            agent("Older", AgentStatus::Idle, 1),
            agent("Newer", AgentStatus::Idle, 59),
        ];

        sort_agents(&mut agents);

        assert_eq!(agents[0].name, "Newer");
    }
}
