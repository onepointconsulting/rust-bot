//! The "Sub-agents" page: the durable ACP child agents and the spawn tasks of the
//! gateway process, the same two sections `/subagents` prints in the terminal.
//!
//! Replaces the chat area while open (see `chat_shell.rs`). The data comes from the
//! gateway's `subagents` event and the page is read-only. "Running here" only reflects
//! runs started by the gateway process itself, and the spawn tasks listed are the ones
//! from the user's own chats.
//!
//! The ordering and labelling rules are plain functions so they are unit-testable on
//! the host target without Leptos.

use chat_ui::models::{AcpAgentSummary, SessionListItem, SpawnTaskSummary};
use leptos::prelude::*;

use super::list_page::{ListPageShell, SectionHeading, Tile};

/// Prefix of the session key of a WebUI chat (`websocket:<chat id>`).
const WEBSOCKET_SESSION_PREFIX: &str = "websocket:";

/// Tooltip on the "running here" badge.
const RUNNING_HERE_HINT: &str =
    "Only runs started by this gateway process are detected, not runs from other processes.";

/// Child agents ordered for display: the ones running here first, then alphabetically by
/// name, ignoring case.
pub fn sorted_agents(agents: &[AcpAgentSummary]) -> Vec<AcpAgentSummary> {
    let mut sorted = agents.to_vec();
    sorted.sort_by_key(|agent| (agent.state != "running_here", agent.name.to_lowercase()));
    sorted
}

/// Tailwind classes for the state badge of a child agent.
pub fn agent_state_class(state: &str) -> &'static str {
    match state {
        "running_here" => {
            "rounded-md bg-emerald-50 px-1.5 py-0.5 text-[11px] font-medium text-emerald-700"
        }
        "never_run" => {
            "rounded-md bg-amber-50 px-1.5 py-0.5 text-[11px] font-medium text-amber-700"
        }
        _ => "rounded-md bg-slate-100 px-1.5 py-0.5 text-[11px] font-medium text-slate-600",
    }
}

/// The badge text for a child agent: the gateway's label, or the raw state code when it
/// sent none.
pub fn agent_state_text(agent: &AcpAgentSummary) -> String {
    if agent.state_label.is_empty() {
        agent.state.clone()
    } else {
        agent.state_label.clone()
    }
}

/// The small line under a child agent's purpose, worded like the terminal listing:
/// `cwd: …; created 2d ago by cli:direct; last used: 1d ago`.
pub fn agent_details_line(agent: &AcpAgentSummary) -> String {
    format!(
        "cwd: {}; created {} by {}; last used: {}",
        agent.cwd.as_deref().unwrap_or("—"),
        agent.created_ago,
        agent.created_by,
        agent.last_used_ago.as_deref().unwrap_or("never"),
    )
}

/// Tailwind classes for the status badge of a spawn task.
pub fn task_status_class(status: &str) -> &'static str {
    match status {
        "running" => {
            "rounded-md bg-emerald-50 px-1.5 py-0.5 text-[11px] font-medium text-emerald-700"
        }
        "partial" => "rounded-md bg-amber-50 px-1.5 py-0.5 text-[11px] font-medium text-amber-700",
        "failed" => "rounded-md bg-red-50 px-1.5 py-0.5 text-[11px] font-medium text-red-700",
        _ => "rounded-md bg-slate-100 px-1.5 py-0.5 text-[11px] font-medium text-slate-600",
    }
}

/// `spawned 5m ago`, plus `, finished 2m ago` once the task has ended.
pub fn task_timing(task: &SpawnTaskSummary) -> String {
    match &task.finished_ago {
        Some(finished) => format!("spawned {}, finished {finished}", task.spawned_ago),
        None => format!("spawned {}", task.spawned_ago),
    }
}

/// The chat id inside a WebUI chat's session key, or `None` for any other session.
pub fn session_chat_id(session_key: &str) -> Option<&str> {
    session_key.strip_prefix(WEBSOCKET_SESSION_PREFIX)
}

/// Whether `session_key` is the chat the user has open.
pub fn is_this_chat(session_key: Option<&str>, active_chat_id: Option<&str>) -> bool {
    match (session_key.and_then(session_chat_id), active_chat_id) {
        (Some(task_chat), Some(active)) => task_chat == active,
        _ => false,
    }
}

/// What to show as the origin of a spawn task: the title of its chat when the sidebar
/// knows it, a short chat id otherwise, the raw key for non-chat sessions, `—` for none.
pub fn task_session_label(session_key: Option<&str>, sessions: &[SessionListItem]) -> String {
    let Some(key) = session_key else {
        return "—".to_string();
    };
    let Some(chat_id) = session_chat_id(key) else {
        return key.to_string();
    };
    match sessions.iter().find(|session| session.id == chat_id) {
        Some(session) if !session.title.trim().is_empty() => session.title.clone(),
        Some(_) => "Untitled chat".to_string(),
        None => format!("chat {}", chat_id.chars().take(8).collect::<String>()),
    }
}

/// One child agent row.
#[component]
fn AgentRow(agent: AcpAgentSummary) -> impl IntoView {
    let badge_hint = if agent.state == "running_here" {
        RUNNING_HERE_HINT
    } else {
        ""
    };
    let details = agent_details_line(&agent);
    view! {
        <li class="flex items-start gap-4 py-3">
            <Tile title=agent.name.clone() />
            <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-2">
                    <span class="text-sm font-medium text-slate-900">{agent.name.clone()}</span>
                    <span class=agent_state_class(&agent.state) title=badge_hint>
                        {agent_state_text(&agent)}
                    </span>
                    <span class="rounded-md bg-slate-100 px-1.5 py-0.5 text-[11px] text-slate-600">
                        {format!("preset: {}", agent.preset)}
                    </span>
                    <span class="rounded-md bg-slate-100 px-1.5 py-0.5 text-[11px] text-slate-600">
                        {format!("depth: {}", agent.depth)}
                    </span>
                </div>
                <p class="text-sm text-slate-600">{agent.purpose.clone()}</p>
                <p class="break-all text-xs text-slate-400">{details}</p>
            </div>
        </li>
    }
}

/// One spawn task row.
#[component]
fn TaskRow(
    task: SpawnTaskSummary,
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_chat_id: Signal<Option<String>>,
) -> impl IntoView {
    let timing = task_timing(&task);
    let session_key = task.session_key.clone();
    let session_for_label = session_key.clone();
    let session_for_this_chat = session_key.clone();
    view! {
        <li class="flex items-start gap-4 py-3">
            <Tile title=task.label.clone() />
            <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-2">
                    <span class="text-sm font-medium text-slate-900">{task.label.clone()}</span>
                    <span class=task_status_class(&task.status)>{task.status.clone()}</span>
                    <Show when=move || {
                        is_this_chat(session_for_this_chat.as_deref(), active_chat_id.get().as_deref())
                    }>
                        <span class="rounded-md bg-sky-50 px-1.5 py-0.5 text-[11px] font-medium text-sky-700">
                            "this chat"
                        </span>
                    </Show>
                </div>
                <p class="text-sm text-slate-600">{task.task_summary.clone()}</p>
                <p class="break-all text-xs text-slate-400">
                    {move || {
                        format!(
                            "{} ({}); {}",
                            task.task_id,
                            task_session_label(session_for_label.as_deref(), &sessions.get()),
                            timing,
                        )
                    }}
                </p>
            </div>
        </li>
    }
}

/// The sub-agents page.
///
/// `loaded` is false while the first reply is in flight (shows a loading line).
/// `acp_enabled` is false when child agents are switched off on the gateway. `sessions`
/// and `active_chat_id` let a task show the title of its chat and a "this chat" tag.
#[component]
pub fn SubagentsPage(
    #[prop(into)] agents: Signal<Vec<AcpAgentSummary>>,
    #[prop(into)] tasks: Signal<Vec<SpawnTaskSummary>>,
    #[prop(into)] acp_enabled: Signal<bool>,
    #[prop(into)] loaded: Signal<bool>,
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_chat_id: Signal<Option<String>>,
    on_refresh: Callback<()>,
) -> impl IntoView {
    view! {
        <ListPageShell
            title="Sub-agents"
            subtitle=Signal::stored("Child agents and spawn tasks. Read-only.")
            refresh_label="Refresh sub-agents"
            on_refresh=on_refresh
        >
            <Show
                when=move || loaded.get()
                fallback=|| view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }
            >
                {move || {
                    let agent_rows = sorted_agents(&agents.get());
                    let task_rows = tasks.get();
                    let agent_total = agent_rows.len();
                    let task_total = task_rows.len();
                    let agents_section = if !acp_enabled.get() {
                        view! {
                            <p class="py-3 text-sm text-slate-500">
                                "Child agents are unavailable: ACP is disabled."
                            </p>
                        }
                            .into_any()
                    } else if agent_rows.is_empty() {
                        view! { <p class="py-3 text-sm text-slate-500">"No child agents."</p> }
                            .into_any()
                    } else {
                        view! {
                            <ul class="divide-y divide-slate-100">
                                {agent_rows
                                    .into_iter()
                                    .map(|agent| view! { <AgentRow agent=agent /> })
                                    .collect_view()}
                            </ul>
                        }
                            .into_any()
                    };
                    let tasks_section = if task_rows.is_empty() {
                        view! { <p class="py-3 text-sm text-slate-500">"No spawn tasks."</p> }
                            .into_any()
                    } else {
                        view! {
                            <ul class="divide-y divide-slate-100">
                                {task_rows
                                    .into_iter()
                                    .map(|task| {
                                        view! {
                                            <TaskRow
                                                task=task
                                                sessions=sessions
                                                active_chat_id=active_chat_id
                                            />
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                            .into_any()
                    };
                    view! {
                        <SectionHeading
                            title="Child agents"
                            count=agent_total
                            detail=Some("durable definitions".to_string())
                        />
                        {agents_section}
                        <SectionHeading
                            title="Spawn tasks"
                            count=task_total
                            detail=Some("this process, from your chats, cleared on restart".to_string())
                        />
                        {tasks_section}
                    }
                }}
            </Show>
        </ListPageShell>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, state: &str) -> AcpAgentSummary {
        AcpAgentSummary {
            name: name.to_string(),
            purpose: "Plans things".to_string(),
            preset: "rustbot".to_string(),
            depth: 1,
            cwd: Some("C:/work".to_string()),
            state: state.to_string(),
            state_label: state.replace('_', " "),
            created_at: "2026-10-08T10:00:00+00:00".to_string(),
            created_ago: "2d ago".to_string(),
            created_by: "cli:direct".to_string(),
            last_used: Some("2026-10-09T10:00:00+00:00".to_string()),
            last_used_ago: Some("1d ago".to_string()),
        }
    }

    fn task(finished_ago: Option<&str>) -> SpawnTaskSummary {
        SpawnTaskSummary {
            task_id: "t1".to_string(),
            label: "research".to_string(),
            task_summary: "Find things".to_string(),
            status: "running".to_string(),
            session_key: Some("websocket:abc".to_string()),
            spawned_at: "2026-10-10T09:55:00+00:00".to_string(),
            spawned_ago: "5m ago".to_string(),
            finished_at: finished_ago.map(|_| "2026-10-10T09:58:00+00:00".to_string()),
            finished_ago: finished_ago.map(str::to_string),
        }
    }

    fn session(id: &str, title: &str) -> SessionListItem {
        SessionListItem {
            id: id.to_string(),
            title: title.to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            has_summary: false,
            pinned: false,
        }
    }

    #[test]
    fn sorted_agents_puts_running_first_then_alphabetical_ignoring_case() {
        let agents = vec![
            agent("zeta", "idle"),
            agent("Beta", "never_run"),
            agent("omega", "running_here"),
            agent("alpha", "idle"),
        ];
        let names: Vec<String> = sorted_agents(&agents).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["omega", "alpha", "Beta", "zeta"]);
    }

    #[test]
    fn agent_state_class_distinguishes_running_never_run_and_the_rest() {
        assert!(agent_state_class("running_here").contains("emerald"));
        assert!(agent_state_class("never_run").contains("amber"));
        assert_eq!(agent_state_class("idle"), agent_state_class("unknown"));
        assert_ne!(agent_state_class("idle"), agent_state_class("running_here"));
    }

    #[test]
    fn agent_state_text_prefers_the_gateway_label() {
        assert_eq!(
            agent_state_text(&agent("a", "running_here")),
            "running here"
        );
        let mut unlabeled = agent("a", "idle");
        unlabeled.state_label = String::new();
        assert_eq!(agent_state_text(&unlabeled), "idle");
    }

    #[test]
    fn agent_details_line_matches_the_terminal_wording() {
        assert_eq!(
            agent_details_line(&agent("planner", "idle")),
            "cwd: C:/work; created 2d ago by cli:direct; last used: 1d ago"
        );
    }

    #[test]
    fn agent_details_line_handles_no_cwd_and_never_used() {
        let mut fresh = agent("planner", "never_run");
        fresh.cwd = None;
        fresh.last_used_ago = None;
        assert_eq!(
            agent_details_line(&fresh),
            "cwd: —; created 2d ago by cli:direct; last used: never"
        );
    }

    #[test]
    fn task_status_class_marks_running_partial_and_failed() {
        assert!(task_status_class("running").contains("emerald"));
        assert!(task_status_class("partial").contains("amber"));
        assert!(task_status_class("failed").contains("red"));
        assert_eq!(
            task_status_class("completed"),
            task_status_class("cancelled")
        );
    }

    #[test]
    fn task_timing_adds_the_finish_time_only_when_finished() {
        assert_eq!(task_timing(&task(None)), "spawned 5m ago");
        assert_eq!(
            task_timing(&task(Some("2m ago"))),
            "spawned 5m ago, finished 2m ago"
        );
    }

    #[test]
    fn session_chat_id_only_for_webui_chats() {
        assert_eq!(session_chat_id("websocket:abc"), Some("abc"));
        assert_eq!(session_chat_id("cli:direct"), None);
    }

    #[test]
    fn is_this_chat_compares_the_chat_ids() {
        assert!(is_this_chat(Some("websocket:abc"), Some("abc")));
        assert!(!is_this_chat(Some("websocket:abc"), Some("other")));
        assert!(!is_this_chat(Some("cli:direct"), Some("direct")));
        assert!(!is_this_chat(None, Some("abc")));
        assert!(!is_this_chat(Some("websocket:abc"), None));
    }

    #[test]
    fn task_session_label_uses_the_chat_title_when_known() {
        let sessions = vec![session("abc", "Refactor plan")];
        assert_eq!(
            task_session_label(Some("websocket:abc"), &sessions),
            "Refactor plan"
        );
    }

    #[test]
    fn task_session_label_falls_back_for_untitled_unknown_and_other_sessions() {
        let sessions = vec![session("abc", "  ")];
        assert_eq!(
            task_session_label(Some("websocket:abc"), &sessions),
            "Untitled chat"
        );
        assert_eq!(
            task_session_label(Some("websocket:0123456789abcdef"), &sessions),
            "chat 01234567"
        );
        assert_eq!(
            task_session_label(Some("cli:direct"), &sessions),
            "cli:direct"
        );
        assert_eq!(task_session_label(None, &sessions), "—");
    }
}
