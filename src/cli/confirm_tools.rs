use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::Select;

use crate::agent::hook::{AgentHook, CallDecision, ToolCallAuthorization};
use crate::agent::tool_approval::{
    self, ApprovalGrant, ApprovalGrants, ToolApprovalBroker, ToolApprovalCall,
};
use crate::bus::outbound_events::{
    OutboundEvent, ToolApprovalRequestEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL;
use crate::cli::cancel::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::integrations::herdr::HerdrReporter;

const ARG_PREVIEW_LIMIT: usize = 120;
/// How long a WebSocket approval request waits before denying the call.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Reason shown to the model when the user declined (or never answered).
const USER_DENIED_REASON: &str = "User denied tool execution";

/// Chat id the CLI's grants are kept under when the turn names none.
const CLI_CHAT_ID: &str = "cli";

const MSG_RUN_THIS_CALL: &str = "Run this call";
const MSG_SKIP_THIS_CALL: &str = "Skip this call";

/// CLI-only hook that asks before each tool call when stdin is a terminal.
///
/// The prompt can also pre-approve later calls (for the turn, for one tool, or
/// for everything). Those grants live in memory until the CLI exits.
pub struct CliAskHook {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
    herdr: Arc<HerdrReporter>,
    grants: ApprovalGrants,
}

impl CliAskHook {
    pub fn new(herdr: Arc<HerdrReporter>) -> Self {
        Self {
            renderer: StdMutex::new(None),
            herdr,
            grants: ApprovalGrants::new(),
        }
    }

    pub fn set_renderer(&self, renderer: Arc<Mutex<StreamRenderer>>) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = Some(renderer);
    }

    pub fn clear_renderer(&self) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn renderer(&self) -> Option<Arc<Mutex<StreamRenderer>>> {
        self.renderer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn preview_arguments(arguments: &serde_json::Value) -> String {
    tool_approval::preview_arguments(arguments, ARG_PREVIEW_LIMIT)
}

/// Denial for a call that needs confirmation when nobody can be asked.
///
/// Confirmation being switched on means no call may run unconfirmed, so a
/// missing way to ask is a refusal, never a pass.
fn unanswerable(why: &str) -> CallDecision {
    log::warn!("tools.confirmBeforeExecute is enabled but {why}; denying the tool call");
    CallDecision::deny(format!(
        "confirmation is required (tools.confirmBeforeExecute) but {why}, so the call was not run"
    ))
}

/// What the user can answer at the terminal prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CliChoice {
    /// Run this call only.
    Run,
    /// Do not run this call.
    Skip,
    /// Run this call and pre-approve later ones as far as the grant says. The
    /// text is the label shown in the menu.
    RunAndGrant(ApprovalGrant, String),
}

impl std::fmt::Display for CliChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run => f.write_str(MSG_RUN_THIS_CALL),
            Self::Skip => f.write_str(MSG_SKIP_THIS_CALL),
            Self::RunAndGrant(_, label) => f.write_str(label),
        }
    }
}

/// The menu for a call of `tool_name`. The turn choice is only offered when the
/// turn is known, because a turn grant is pinned to it.
fn cli_choices(tool_name: &str, turn_known: bool) -> Vec<CliChoice> {
    let mut choices = vec![CliChoice::Run, CliChoice::Skip];
    if turn_known {
        choices.push(CliChoice::RunAndGrant(
            ApprovalGrant::Turn,
            "Run, and allow all calls for this turn".to_string(),
        ));
    }
    choices.push(CliChoice::RunAndGrant(
        ApprovalGrant::SessionTool,
        format!("Run, and always allow `{tool_name}` this session"),
    ));
    choices.push(CliChoice::RunAndGrant(
        ApprovalGrant::Session,
        "Run, and allow everything this session".to_string(),
    ));
    choices
}

/// One line telling the user what a grant they just gave covers.
fn grant_note(grant: ApprovalGrant, tool_name: &str) -> String {
    match grant {
        ApprovalGrant::Turn => "Allowing every tool call for the rest of this turn.".to_string(),
        ApprovalGrant::SessionTool => {
            format!("Allowing `{tool_name}` for the rest of this session (until you quit).")
        }
        ApprovalGrant::Session => {
            "Allowing every tool call for the rest of this session (until you quit).".to_string()
        }
    }
}

/// Turn the user's answer into a decision, recording any grant it carries.
///
/// No answer (Esc, Ctrl-C, a failed prompt) skips the call: a question that
/// was not answered is never a yes.
fn apply_choice(
    grants: &ApprovalGrants,
    answer: Option<CliChoice>,
    chat_id: &str,
    turn_id: Option<&str>,
    tool_name: &str,
) -> CallDecision {
    match answer {
        Some(CliChoice::Run) => CallDecision::Allow,
        Some(CliChoice::RunAndGrant(grant, _)) => {
            grants.add(chat_id, grant, turn_id, vec![tool_name.to_string()]);
            CallDecision::Allow
        }
        Some(CliChoice::Skip) | None => CallDecision::deny(USER_DENIED_REASON),
    }
}

fn prompt_for_tool(
    name: &str,
    args_preview: &str,
    choices: Vec<CliChoice>,
) -> Result<CliChoice, inquire::InquireError> {
    let message = if args_preview == "{}" {
        format!("Execute tool `{name}`?")
    } else {
        format!("Execute tool `{name}` ({args_preview})?")
    };
    Select::new(&message, choices)
        .with_help_message("up/down to move, enter to choose, esc to skip this call")
        .prompt()
}

#[async_trait]
impl AgentHook for CliAskHook {
    async fn authorize_call(&self, request: &ToolCallAuthorization<'_>) -> CallDecision {
        let chat_id = request.chat_id.unwrap_or(CLI_CHAT_ID);
        if self
            .grants
            .is_granted(chat_id, request.turn_id, request.tool_name)
        {
            log::info!("tool '{}' approved by an earlier grant", request.tool_name);
            return CallDecision::Allow;
        }
        if !std::io::stdin().is_terminal() {
            return unanswerable("stdin is not a terminal");
        }

        if let Some(renderer) = self.renderer() {
            renderer.lock().await.stop_for_input();
        }
        self.herdr.report_blocked(None);
        let _pause = pause_for_prompt();
        let tool_name = request.tool_name.to_string();
        let arguments_preview = preview_arguments(request.arguments);
        let choices = cli_choices(request.tool_name, request.turn_id.is_some());
        let answer = tokio::task::spawn_blocking(move || {
            prompt_for_tool(&tool_name, &arguments_preview, choices)
        })
        .await
        .ok()
        .and_then(Result::ok);
        if let Some(CliChoice::RunAndGrant(grant, _)) = &answer {
            println!("{}", grant_note(*grant, request.tool_name));
        }
        drop(_pause);
        self.herdr.report_working();
        if let Some(renderer) = self.renderer() {
            renderer.lock().await.resume_after_input();
        }
        apply_choice(
            &self.grants,
            answer,
            chat_id,
            request.turn_id,
            request.tool_name,
        )
    }
}

/// Gateway hook that asks the WebSocket chat's user to approve each tool
/// call. The request goes out over the bus; the answer comes back through
/// the shared [`ToolApprovalBroker`], resolved by the WebSocket channel.
pub struct WebsocketsAskHook {
    broker: Arc<ToolApprovalBroker>,
    bus: StdMutex<Option<Arc<MessageBus>>>,
    timeout: Duration,
}

impl WebsocketsAskHook {
    pub fn new(broker: Arc<ToolApprovalBroker>) -> Self {
        Self {
            broker,
            bus: StdMutex::new(None),
            timeout: APPROVAL_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn set_bus(&self, bus: Arc<MessageBus>) {
        *self.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
    }

    fn bus(&self) -> Option<Arc<MessageBus>> {
        self.bus.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Removes the broker entry if the waiting turn is dropped (e.g. aborted).
struct PendingGuard<'a> {
    broker: &'a ToolApprovalBroker,
    request_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.broker.cancel(&self.request_id);
    }
}

#[async_trait]
impl AgentHook for WebsocketsAskHook {
    async fn authorize_call(&self, request: &ToolCallAuthorization<'_>) -> CallDecision {
        let chat_id = match (request.channel, request.chat_id) {
            (Some(WEBSOCKET_CHANNEL), Some(chat_id)) if !chat_id.is_empty() => chat_id.to_string(),
            (channel, _) => {
                return unanswerable(&format!("channel {channel:?} cannot ask for it"));
            }
        };
        if self
            .broker
            .is_granted(&chat_id, request.turn_id, request.tool_name)
        {
            log::info!(
                "tool '{}' approved by an earlier grant of chat {chat_id}",
                request.tool_name
            );
            return CallDecision::Allow;
        }
        let Some(bus) = self.bus() else {
            return unanswerable("no bus is wired to ask through");
        };

        let call = ToolApprovalCall::from_authorization(request);
        let (request_id, rx) =
            self.broker
                .register_for_turn(&chat_id, vec![call.clone()], request.turn_id);
        let _guard = PendingGuard {
            broker: &self.broker,
            request_id: request_id.clone(),
        };
        let outbound = outbound_message_for_event(
            WEBSOCKET_CHANNEL,
            &chat_id,
            OutboundEvent::ToolApprovalRequest(ToolApprovalRequestEvent {
                request_id: request_id.clone(),
                calls: vec![call],
            }),
            None,
            None,
        );
        if let Err(e) = bus.publish_outbound(outbound) {
            log::error!("Failed to publish tool approval request: {e}");
            return CallDecision::deny(USER_DENIED_REASON);
        }

        let approved = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                log::warn!("Tool approval request {request_id} timed out; denying the call");
                Vec::new()
            }
        };
        if approved.iter().any(|id| id == request.call_id) {
            CallDecision::Allow
        } else {
            CallDecision::deny(USER_DENIED_REASON)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An authorization request for call `call_id` of tool `exec` in `channel`/`chat_id`.
    fn request<'a>(
        arguments: &'a serde_json::Value,
        channel: Option<&'a str>,
        chat_id: Option<&'a str>,
    ) -> ToolCallAuthorization<'a> {
        ToolCallAuthorization {
            call_id: "a",
            tool_name: "exec",
            arguments,
            channel,
            chat_id,
            turn_id: None,
        }
    }

    #[test]
    fn preview_arguments_truncates_long_payloads() {
        let arguments = serde_json::json!({"content": "x".repeat(200)});
        let preview = preview_arguments(&arguments);
        assert!(preview.ends_with("..."));
        assert!(preview.chars().count() <= ARG_PREVIEW_LIMIT + 3);
    }

    #[test]
    fn preview_arguments_keeps_short_payloads() {
        let arguments = serde_json::json!({"path": "/tmp/foo.txt"});
        assert_eq!(
            preview_arguments(&arguments),
            serde_json::to_string(&arguments).unwrap()
        );
    }

    #[tokio::test]
    async fn denies_when_stdin_is_not_a_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let hook = CliAskHook::new(HerdrReporter::disabled_for_test());
        let arguments = serde_json::json!({"path": "/tmp/foo.txt"});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("direct")))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("a question nobody can answer must deny");
        };
        assert!(reason.contains("stdin is not a terminal"), "got {reason:?}");
    }

    fn websocket_hook(
        broker: &Arc<ToolApprovalBroker>,
    ) -> (Arc<WebsocketsAskHook>, Arc<MessageBus>) {
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(broker));
        hook.set_bus(Arc::clone(&bus));
        (Arc::new(hook), bus)
    }

    /// Run the hook for call `a`, answering the request it publishes with `answer`.
    async fn run_with_answer(answer: Option<Vec<String>>, timeout: Duration) -> CallDecision {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(timeout);
        hook.set_bus(Arc::clone(&bus));
        let responder = {
            let broker = Arc::clone(&broker);
            let bus = Arc::clone(&bus);
            tokio::spawn(async move {
                let msg = bus.consume_outbound().await.expect("request published");
                assert_eq!(msg.channel, WEBSOCKET_CHANNEL);
                assert_eq!(msg.chat_id, "chat-1");
                let Some(OutboundEvent::ToolApprovalRequest(event)) = msg.event else {
                    panic!("expected tool approval request");
                };
                assert_eq!(event.calls.len(), 1, "one request per call");
                assert_eq!(event.calls[0].id, "a");
                if let Some(approved) = answer {
                    broker
                        .resolve("chat-1", &event.request_id, approved)
                        .unwrap();
                }
            })
        };
        let arguments = serde_json::json!({});
        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;
        responder.await.unwrap();
        assert!(broker.pending_for_chat("chat-1").is_empty());
        decision
    }

    fn user_denied() -> CallDecision {
        CallDecision::deny(USER_DENIED_REASON)
    }

    #[tokio::test]
    async fn websocket_hook_allows_an_approved_call() {
        let decision = run_with_answer(Some(vec!["a".into()]), Duration::from_secs(5)).await;
        assert_eq!(decision, CallDecision::Allow);
    }

    #[tokio::test]
    async fn websocket_hook_denies_a_call_missing_from_the_answer() {
        let decision = run_with_answer(Some(vec!["b".into()]), Duration::from_secs(5)).await;
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_on_timeout() {
        let decision = run_with_answer(None, Duration::from_millis(50)).await;
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_when_request_cancelled() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let canceller = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move {
                let msg = bus.consume_outbound().await.unwrap();
                let Some(OutboundEvent::ToolApprovalRequest(event)) = msg.event else {
                    panic!("expected tool approval request");
                };
                broker.cancel(&event.request_id);
            })
        };
        let arguments = serde_json::json!({});
        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;
        canceller.await.unwrap();
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_calls_of_channels_that_cannot_ask() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("chat-1")))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("a channel that cannot ask must be denied, not waved through");
        };
        assert!(reason.contains("cannot ask for it"), "got {reason:?}");
        assert_eq!(bus.outbound_size(), 0);
    }

    #[tokio::test]
    async fn websocket_hook_denies_a_websocket_turn_without_a_chat_id() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let arguments = serde_json::json!({});

        for chat_id in [None, Some("")] {
            let decision = hook
                .authorize_call(&request(&arguments, Some(WEBSOCKET_CHANNEL), chat_id))
                .await;
            assert!(!decision.is_allowed());
        }
        assert_eq!(bus.outbound_size(), 0);
    }

    #[tokio::test]
    async fn websocket_hook_denies_when_no_bus_is_wired() {
        let hook = WebsocketsAskHook::new(Arc::new(ToolApprovalBroker::new()));
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("no bus means nobody can be asked");
        };
        assert!(reason.contains("no bus is wired"), "got {reason:?}");
    }

    // ── grants ────────────────────────────────────────────────────────────────

    use crate::agent::tool_approval::ApprovalGrant;

    /// A request for tool `exec` from `chat-1` made in `turn_id`.
    fn websocket_request<'a>(
        arguments: &'a serde_json::Value,
        turn_id: Option<&'a str>,
    ) -> ToolCallAuthorization<'a> {
        ToolCallAuthorization {
            turn_id,
            ..request(arguments, Some(WEBSOCKET_CHANNEL), Some("chat-1"))
        }
    }

    #[tokio::test]
    async fn websocket_hook_allows_a_granted_tool_without_asking() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        broker.add_grant(
            "chat-1",
            ApprovalGrant::SessionTool,
            None,
            vec!["exec".into()],
        );
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&websocket_request(&arguments, Some("t1")))
            .await;

        assert_eq!(decision, CallDecision::Allow);
        assert_eq!(bus.outbound_size(), 0, "a grant means nobody is asked");
        assert!(broker.pending_for_chat("chat-1").is_empty());
    }

    #[tokio::test]
    async fn websocket_hook_still_asks_about_tools_a_grant_does_not_name() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook =
            WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(Duration::from_millis(50));
        hook.set_bus(Arc::clone(&bus));
        broker.add_grant(
            "chat-1",
            ApprovalGrant::SessionTool,
            None,
            vec!["read_file".into()],
        );
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&websocket_request(&arguments, Some("t1")))
            .await;

        assert_eq!(
            decision,
            user_denied(),
            "exec was not granted, so it asked and timed out"
        );
        assert_eq!(bus.outbound_size(), 1);
    }

    #[tokio::test]
    async fn websocket_hook_honours_a_turn_grant_only_in_that_turn() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook =
            WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(Duration::from_millis(50));
        hook.set_bus(Arc::clone(&bus));
        broker.add_grant("chat-1", ApprovalGrant::Turn, Some("t1"), vec![]);
        let arguments = serde_json::json!({});

        let same_turn = hook
            .authorize_call(&websocket_request(&arguments, Some("t1")))
            .await;
        let next_turn = hook
            .authorize_call(&websocket_request(&arguments, Some("t2")))
            .await;

        assert_eq!(same_turn, CallDecision::Allow);
        assert_eq!(next_turn, user_denied(), "the grant ended with turn t1");
    }

    #[tokio::test]
    async fn websocket_hook_stores_the_asking_turn_with_its_request() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook = Arc::new(
            WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(Duration::from_secs(5)),
        );
        hook.set_bus(Arc::clone(&bus));
        let responder = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move {
                let msg = bus.consume_outbound().await.expect("request published");
                let Some(OutboundEvent::ToolApprovalRequest(event)) = msg.event else {
                    panic!("expected tool approval request");
                };
                broker
                    .resolve_with_grant(
                        "chat-1",
                        &event.request_id,
                        vec!["a".into()],
                        Some(ApprovalGrant::Turn),
                    )
                    .unwrap();
            })
        };
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&websocket_request(&arguments, Some("turn-42")))
            .await;
        responder.await.unwrap();

        assert_eq!(decision, CallDecision::Allow);
        assert!(broker.is_granted("chat-1", Some("turn-42"), "exec"));
        assert!(!broker.is_granted("chat-1", Some("turn-43"), "exec"));
    }

    // ── CLI choices and grants ────────────────────────────────────────────────

    #[test]
    fn the_menu_offers_run_skip_and_three_ways_to_remember_the_answer() {
        let labels: Vec<String> = cli_choices("shell", true)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            labels,
            [
                MSG_RUN_THIS_CALL,
                MSG_SKIP_THIS_CALL,
                "Run, and allow all calls for this turn",
                "Run, and always allow `shell` this session",
                "Run, and allow everything this session",
            ]
        );
    }

    #[test]
    fn the_turn_choice_is_left_out_when_the_turn_is_not_known() {
        let choices = cli_choices("shell", false);
        assert_eq!(choices.len(), 4);
        assert!(
            !choices
                .iter()
                .any(|c| matches!(c, CliChoice::RunAndGrant(ApprovalGrant::Turn, _)))
        );
    }

    #[test]
    fn running_or_skipping_records_no_grant() {
        let grants = ApprovalGrants::new();
        assert_eq!(
            apply_choice(&grants, Some(CliChoice::Run), "cli", Some("t1"), "shell"),
            CallDecision::Allow
        );
        assert_eq!(
            apply_choice(&grants, Some(CliChoice::Skip), "cli", Some("t1"), "shell"),
            user_denied()
        );
        assert!(!grants.is_granted("cli", Some("t1"), "shell"));
    }

    #[test]
    fn no_answer_skips_the_call() {
        let grants = ApprovalGrants::new();
        assert_eq!(
            apply_choice(&grants, None, "cli", Some("t1"), "shell"),
            user_denied()
        );
    }

    #[test]
    fn a_tool_grant_covers_that_tool_only() {
        let grants = ApprovalGrants::new();
        let choice = CliChoice::RunAndGrant(ApprovalGrant::SessionTool, String::new());
        assert_eq!(
            apply_choice(&grants, Some(choice), "cli", Some("t1"), "read_file"),
            CallDecision::Allow
        );
        assert!(grants.is_granted("cli", Some("t2"), "read_file"));
        assert!(!grants.is_granted("cli", Some("t2"), "shell"));
    }

    #[test]
    fn a_session_grant_covers_every_tool_and_a_turn_grant_only_its_turn() {
        let grants = ApprovalGrants::new();
        let session = CliChoice::RunAndGrant(ApprovalGrant::Session, String::new());
        apply_choice(&grants, Some(session), "cli", Some("t1"), "shell");
        assert!(grants.is_granted("cli", Some("t9"), "write_file"));

        let turn_only = ApprovalGrants::new();
        let turn = CliChoice::RunAndGrant(ApprovalGrant::Turn, String::new());
        apply_choice(&turn_only, Some(turn), "cli", Some("t1"), "shell");
        assert!(turn_only.is_granted("cli", Some("t1"), "write_file"));
        assert!(!turn_only.is_granted("cli", Some("t2"), "write_file"));
    }

    #[test]
    fn grant_notes_say_how_long_the_grant_lasts() {
        assert!(grant_note(ApprovalGrant::Turn, "shell").contains("this turn"));
        assert!(grant_note(ApprovalGrant::SessionTool, "shell").contains("`shell`"));
        assert!(grant_note(ApprovalGrant::SessionTool, "shell").contains("until you quit"));
        assert!(grant_note(ApprovalGrant::Session, "shell").contains("every tool call"));
    }

    #[tokio::test]
    async fn a_granted_call_runs_without_asking_even_when_nobody_could_be_asked() {
        let hook = CliAskHook::new(HerdrReporter::disabled_for_test());
        hook.grants.add(
            "direct",
            ApprovalGrant::SessionTool,
            None,
            vec!["exec".into()],
        );
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("direct")))
            .await;

        assert_eq!(decision, CallDecision::Allow);
    }

    #[tokio::test]
    async fn a_grant_of_one_chat_does_not_cover_another() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let hook = CliAskHook::new(HerdrReporter::disabled_for_test());
        hook.grants
            .add("direct", ApprovalGrant::Session, None, vec![]);
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("other")))
            .await;

        assert!(
            !decision.is_allowed(),
            "no terminal and no grant for this chat"
        );
    }
}
