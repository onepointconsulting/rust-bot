//! The "Available ACP agents" section of the parent's system prompt.
//!
//! Without it the model would answer a follow-up coding question itself, or
//! create a duplicate agent, instead of sending it to the agent that already has
//! the relevant memory and conversation (plan decision 7).

use crate::agent::acp::store::AgentSummary;

/// The prompt section listing `agents`; `None` when there are none.
pub fn agents_section(agents: &[AgentSummary]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## Available ACP agents".to_string(),
        String::new(),
        "These agents were created earlier. Each has its own long-term memory and remembers \
         its earlier conversations with you, so send follow-up questions on their subject to \
         the same agent with `acp_run_agent {name, prompt, cwd?}` instead of answering them \
         yourself or creating a new agent."
            .to_string(),
        String::new(),
    ];
    for agent in agents {
        let purpose = agent.meta.purpose.trim();
        if purpose.is_empty() {
            lines.push(format!("- {}", agent.meta.name));
        } else {
            lines.push(format!("- {}: {}", agent.meta.name, purpose));
        }
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::acp::store::{AgentMeta, Overrides};

    fn summary(name: &str, purpose: &str) -> AgentSummary {
        AgentSummary {
            meta: AgentMeta {
                name: name.to_string(),
                purpose: purpose.to_string(),
                preset: "rustbot".to_string(),
                created_at: "2026-10-01T00:00:00Z".to_string(),
                created_by: "cli:direct".to_string(),
                depth: 1,
                cwd: None,
                overrides: Overrides::default(),
                resynced_at: None,
            },
            drifted_files: Vec::new(),
        }
    }

    #[test]
    fn no_agents_means_no_section() {
        assert_eq!(agents_section(&[]), None);
    }

    #[test]
    fn each_agent_is_listed_with_its_purpose() {
        let section = agents_section(&[
            summary("code-review", "reviews Rust code"),
            summary("docs", "writes documentation"),
        ])
        .unwrap();

        assert!(section.starts_with("## Available ACP agents"));
        assert!(
            section.contains("- code-review: reviews Rust code"),
            "{section}"
        );
        assert!(
            section.contains("- docs: writes documentation"),
            "{section}"
        );
        assert!(section.contains("acp_run_agent"), "{section}");
    }

    #[test]
    fn an_agent_without_a_purpose_is_still_listed() {
        let section = agents_section(&[summary("scribe", "  ")]).unwrap();
        assert!(
            section.contains("- scribe\n") || section.ends_with("- scribe"),
            "{section}"
        );
    }
}
