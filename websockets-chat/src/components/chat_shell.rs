use chat_ui::components::{ChatHeaderActions, ChatInput, SessionsSidebar, SessionsStrip};
use chat_ui::models::{
    AcpAgentSummary, ChatEntry, McpCustomServerSummary, McpPresetSummary, OutgoingMessage,
    SessionListItem, SessionSummaryPopup, SessionTokenUsage, SkillSummary, SpawnTaskSummary,
    ToolSummary, WorkspaceDialogState,
};
use leptos::prelude::*;

use crate::components::{
    AskQuestionCard, ConfigSavedNotice, McpChangeRequest, McpPresetsPage, MessageList,
    SettingsActions, SettingsDialog, SettingsPanel, SubagentsPage, ToolApprovalCard,
};
use crate::shortcuts::{is_sidebar_toggle_shortcut, sidebar_shortcut_label};
use crate::state::{
    background_banner, ConfigFileView, ConnectionStatus, MainView, PendingApproval, PendingQuestion,
};

/// Small dot + label in the header showing the gateway WebSocket's current
/// lifecycle state (see [`ConnectionStatus`] for what drives it).
#[component]
fn ConnectionBadge(#[prop(into)] status: Signal<ConnectionStatus>) -> impl IntoView {
    view! {
        <div class="flex shrink-0 items-center gap-1.5 rounded-full bg-slate-100 px-2.5 py-1 text-xs text-slate-500">
            <span class=move || format!("connection-dot {}", status.get().dot_modifier_class())></span>
            <span>{move || status.get().label()}</span>
        </div>
    }
}

/// The status line above the composer while background sub-agents run or the
/// agent works on their results: what is going on, the latest tool call of that
/// work, and a Stop button (the same abort as `/stop`).
#[component]
fn BackgroundWorkBanner(
    #[prop(into)] running: Signal<usize>,
    #[prop(into)] working: Signal<bool>,
    #[prop(into)] hint: Signal<Option<String>>,
    #[prop(into)] on_stop: Callback<()>,
) -> impl IntoView {
    view! {
        <Show when=move || background_banner(running.get(), working.get()).is_some()>
            <div
                class="flex items-center justify-between gap-3 border-t border-slate-100 bg-slate-50 px-4 py-1.5 text-xs text-slate-500"
                role="status"
            >
                <div class="min-w-0">
                    <p>{move || background_banner(running.get(), working.get()).unwrap_or_default()}</p>
                    <Show when=move || hint.get().is_some()>
                        <p class="truncate text-slate-400">{move || hint.get().unwrap_or_default()}</p>
                    </Show>
                </div>
                <button
                    type="button"
                    class="shrink-0 rounded-full border border-slate-300 px-2.5 py-0.5 text-slate-600 hover:bg-slate-100"
                    on:click=move |_| on_stop.run(())
                >
                    "Stop"
                </button>
            </div>
        </Show>
    }
}

#[component]
pub fn ChatShell(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    #[prop(into)] connection_status: Signal<ConnectionStatus>,
    #[prop(into)] reconnect_exhausted: Signal<bool>,
    #[prop(into)] token_streaming: Signal<bool>,
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_session_id: Signal<Option<String>>,
    #[prop(into)] sidebar_open: Signal<bool>,
    #[prop(into)] user_email: Signal<Option<String>>,
    #[prop(into)] bot_version: Signal<Option<String>>,
    draft: RwSignal<String>,
    on_send: impl Fn(OutgoingMessage) + 'static + Copy,
    on_new_chat: impl Fn() + 'static + Send + Sync + Copy,
    on_logout: impl Fn() + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
    // Called from inside the `reconnect_exhausted` `<Show>` below, whose
    // `ChildrenFn` requires everything it captures to be `Send + Sync`
    // (unlike a plain `on:click` closure outside any `<Show>`, which
    // doesn't need that bound — see `on_logout`/`on_minimize` above).
    on_retry: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] expanded: Signal<bool>,
    on_toggle_expand: impl Fn() + 'static + Copy,
    on_toggle_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_close_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_select_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_pin_session: impl Fn(String, bool) + 'static + Send + Sync + Copy,
    on_rename_session: impl Fn(String, String) + 'static + Send + Sync + Copy,
    #[prop(default = false)] show_workspace: bool,
    #[prop(into)] workspace_popup: Signal<Option<WorkspaceDialogState>>,
    on_workspace_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_close_workspace: impl Fn() + 'static + Send + Sync + Copy,
    on_browse_workspace: impl Fn(String) + 'static + Send + Sync + Copy,
    on_save_workspace: impl Fn(String, String) + 'static + Send + Sync + Copy,
    on_default_workspace: impl Fn() + 'static + Send + Sync + Copy,
    on_summary_session: impl Fn(String) + 'static + Send + Sync + Copy,
    #[prop(into)] summary_popup: Signal<Option<SessionSummaryPopup>>,
    on_close_summary: impl Fn() + 'static + Send + Sync + Copy,
    on_fork_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_fork_reply: impl Fn(u64) + 'static + Send + Sync + Copy,
    on_delete_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_clear_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_abort_turn: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] background_agents: Signal<usize>,
    #[prop(into)] working_on_results: Signal<bool>,
    #[prop(into)] background_hint: Signal<Option<String>>,
    #[prop(into)] pending_approval: Signal<Option<PendingApproval>>,
    on_toggle_approval_call: impl Fn(String) + 'static + Send + Sync + Copy,
    on_answer_approval: impl Fn(Vec<String>) + 'static + Send + Sync + Copy,
    #[prop(into)] pending_question: Signal<Option<PendingQuestion>>,
    on_select_question_option: impl Fn(String) + 'static + Send + Sync + Copy,
    on_answer_question_free_text: impl Fn(String) + 'static + Send + Sync + Copy,
    on_chat_about_question: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] model_presets: Signal<Vec<String>>,
    #[prop(into)] selected_model_preset: Signal<String>,
    on_select_model_preset: impl Fn(String) + 'static + Send + Sync + Copy,
    #[prop(into)] selected_agent_mode: Signal<String>,
    on_select_agent_mode: impl Fn(String) + 'static + Send + Sync + Copy,
    #[prop(into)] session_usage: Signal<Option<SessionTokenUsage>>,
    #[prop(into)] skills: Signal<Vec<SkillSummary>>,
    #[prop(into)] tools: Signal<Vec<ToolSummary>>,
    on_open_tools: impl Fn() + 'static + Send + Sync + Copy,
    // The MCP list is for signed-in users only: when `false` the sidebar has no
    // "MCPs" button. The gateway enforces the same rule independently.
    #[prop(default = false)] show_mcps: bool,
    // The sub-agents list, like the MCP list, is for signed-in users only.
    #[prop(default = false)] show_subagents: bool,
    #[prop(into)] main_view: Signal<MainView>,
    #[prop(into)] subagent_agents: Signal<Vec<AcpAgentSummary>>,
    #[prop(into)] spawn_tasks: Signal<Vec<SpawnTaskSummary>>,
    #[prop(into)] subagents_acp_enabled: Signal<bool>,
    #[prop(into)] subagents_loaded: Signal<bool>,
    on_open_subagents: impl Fn() + 'static + Send + Sync + Copy,
    on_refresh_subagents: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] mcp_presets: Signal<Vec<McpPresetSummary>>,
    #[prop(into)] mcp_custom: Signal<Vec<McpCustomServerSummary>>,
    #[prop(into)] mcp_loaded: Signal<bool>,
    #[prop(into)] mcp_can_modify: Signal<bool>,
    #[prop(into)] mcp_busy: Signal<Option<String>>,
    #[prop(into)] mcp_row_error: Signal<Option<(String, String)>>,
    #[prop(into)] mcp_changed: Signal<Vec<String>>,
    on_open_mcps: impl Fn() + 'static + Send + Sync + Copy,
    on_refresh_mcps: impl Fn() + 'static + Send + Sync + Copy,
    on_change_mcp: impl Fn(McpChangeRequest) + 'static + Send + Sync + Copy,
    // The Settings dialog, like the MCP list, is for signed-in users only: when `false`
    // the sidebar has no "Settings" button. The gateway enforces the same rule
    // independently.
    #[prop(default = false)] show_settings: bool,
    // Only administrators get the Settings button; the gateway refuses everyone else.
    #[prop(into)] is_admin: Signal<bool>,
    #[prop(into)] settings_open: Signal<bool>,
    settings_panel: SettingsPanel,
    settings_actions: SettingsActions,
    #[prop(into)] config_file: Signal<ConfigFileView>,
    #[prop(into)] config_saving: Signal<bool>,
    #[prop(into)] config_error: Signal<Option<String>>,
    #[prop(into)] config_saved_notice: Signal<bool>,
    on_open_settings: impl Fn() + 'static + Send + Sync + Copy,
    on_close_settings: impl Fn() + 'static + Send + Sync + Copy,
    on_open_config_file: impl Fn() + 'static + Send + Sync + Copy,
    on_close_config_file: impl Fn() + 'static + Send + Sync + Copy,
    on_save_config_file: impl Fn(String) + 'static + Send + Sync + Copy,
    on_close_config_saved_notice: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(default = true)] show_logout: bool,
    #[prop(default = true)] show_minimize: bool,
) -> impl IntoView {
    let on_use_prompt = move |prompt: String| draft.set(prompt);
    // Starting a new chat should leave the cursor ready in the composer.
    let composer_focus_request = RwSignal::new(0_u32);
    let on_new_chat_and_focus = move || {
        on_new_chat();
        composer_focus_request.update(|count| *count += 1);
    };
    // Ctrl/Cmd+B toggles the sidebar from anywhere (including the composer).
    // Registered once for the component's lifetime; Leptos removes it on
    // unmount.
    window_event_listener(leptos::ev::keydown, move |event| {
        if is_sidebar_toggle_shortcut(
            event.ctrl_key(),
            event.meta_key(),
            event.alt_key(),
            event.shift_key(),
            &event.key(),
        ) {
            event.prevent_default();
            on_toggle_sidebar();
        }
    });
    let shortcut_label = sidebar_shortcut_label();
    // The floating widget (not expanded) gets a narrower strip and sidebar.
    let compact = Signal::derive(move || !expanded.get());
    let on_new_chat_callback = Callback::new(move |()| on_new_chat_and_focus());
    let shell_class = move || {
        if expanded.get() {
            "fixed inset-0 z-50 flex h-full w-full flex-row overflow-hidden bg-slate-50"
        } else {
            "fixed bottom-6 right-6 z-50 flex h-[min(720px,calc(100vh-3rem))] w-[min(42rem,calc(100vw-3rem))] flex-row overflow-hidden rounded-2xl bg-slate-50 shadow-2xl ring-1 ring-slate-200"
        }
    };
    view! {
        <div class=shell_class>
            <Show when=move || !sidebar_open.get()>
                <SessionsStrip
                    on_open=on_toggle_sidebar
                    on_new_chat=on_new_chat_callback
                    open_hint=shortcut_label.to_string()
                    compact=compact
                />
            </Show>
            <SessionsSidebar
                sessions=sessions
                active_id=active_session_id
                open=sidebar_open
                on_close=on_close_sidebar
                on_select=on_select_session
                on_pin=Callback::new(move |(id, pinned)| on_pin_session(id, pinned))
                on_rename=Callback::new(move |(id, title)| on_rename_session(id, title))
                on_workspace=show_workspace.then_some(Callback::new(move |id| on_workspace_session(id)))
                workspace_popup=workspace_popup
                on_close_workspace=Callback::new(move |()| on_close_workspace())
                on_browse_workspace=Callback::new(move |path| on_browse_workspace(path))
                on_save_workspace=Callback::new(move |(folder, mode)| on_save_workspace(folder, mode))
                on_default_workspace=Callback::new(move |()| on_default_workspace())
                on_summary=Callback::new(move |id| on_summary_session(id))
                summary_popup=summary_popup
                on_close_summary=Callback::new(move |()| on_close_summary())
                on_fork=Callback::new(move |id| on_fork_session(id))
                on_clear=Callback::new(move |id| on_clear_session(id))
                on_delete=Callback::new(move |id| on_delete_session(id))
                on_new_chat=on_new_chat_callback
                on_mcps=show_mcps.then_some(Callback::new(move |()| on_open_mcps()))
                mcps_active=Signal::derive(move || main_view.get() == MainView::Mcps)
                on_subagents=show_subagents
                    .then_some(Callback::new(move |()| on_open_subagents()))
                subagents_active=Signal::derive(move || main_view.get() == MainView::Subagents)
                on_settings=show_settings.then_some(Callback::new(move |()| on_open_settings()))
                settings_visible=is_admin
                show_brand=true
                collapse_hint=shortcut_label.to_string()
                compact=compact
            />
            <div class="flex min-w-0 flex-1 flex-col overflow-hidden">
                <header class="relative z-10 flex items-center justify-between gap-2 border-b border-slate-200 bg-white px-4 py-3">
                    <div class="flex min-w-0 items-center gap-2">
                        <ConnectionBadge status=connection_status />
                    </div>
                    <div class="flex min-w-0 items-center gap-1">
                        <ChatHeaderActions
                            expanded=expanded
                            email=user_email
                            version=bot_version
                            on_new_chat=on_new_chat_and_focus
                            show_new_chat=false
                            on_logout=on_logout
                            on_minimize=on_minimize
                            on_toggle_expand=on_toggle_expand
                            show_logout=show_logout
                            show_minimize=show_minimize
                        />
                    </div>
                </header>

                <Show when=move || reconnect_exhausted.get()>
                    <div class="flex items-center justify-between gap-2 border-b border-amber-100 bg-amber-50 px-4 py-2 text-xs text-amber-700">
                        <span>"Connection lost. Automatic reconnect gave up."</span>
                        <button
                            type="button"
                            class="rounded-full bg-amber-600 px-3 py-1 font-medium text-white hover:bg-amber-700"
                            on:click=move |_| on_retry()
                        >
                            "Retry"
                        </button>
                    </div>
                </Show>

                <Show when=move || error.get().is_some()>
                    <p class="border-b border-red-100 bg-red-50 px-4 py-2 text-xs text-red-600">
                        {move || error.get().unwrap_or_default()}
                    </p>
                </Show>

                // Hidden (not unmounted) while the MCP list is showing, so the transcript
                // keeps its scroll position and any half-typed draft.
                <div class=move || {
                    if !main_view.get().is_chat() {
                        "hidden"
                    } else {
                        "flex min-h-0 flex-1 flex-col overflow-hidden"
                    }
                }>
                    <MessageList
                        entries=entries
                        example_prompts=example_prompts
                        token_streaming=token_streaming
                        pending=pending
                        active_session_id=active_session_id
                        on_use_prompt=on_use_prompt
                        on_fork_reply=on_fork_reply
                    />
                    <BackgroundWorkBanner
                        running=background_agents
                        working=working_on_results
                        hint=background_hint
                        on_stop=Callback::new(move |()| on_abort_turn())
                    />
                    <ToolApprovalCard
                        approval=pending_approval
                        on_toggle=Callback::new(move |id| on_toggle_approval_call(id))
                        on_answer=Callback::new(move |ids| on_answer_approval(ids))
                    />
                    <AskQuestionCard
                        question=pending_question
                        on_select=Callback::new(move |id| on_select_question_option(id))
                        on_free_text=Callback::new(move |text| on_answer_question_free_text(text))
                        on_chat_about=Callback::new(move |()| on_chat_about_question())
                    />
                    <ChatInput
                        pending=pending
                        draft=draft
                        on_send=on_send
                        on_abort=Callback::new(move |()| on_abort_turn())
                        model_presets=model_presets
                        selected_model_preset=selected_model_preset
                        on_select_model_preset=Callback::new(move |name| on_select_model_preset(name))
                        selected_agent_mode=selected_agent_mode
                        on_select_agent_mode=Callback::new(move |name| on_select_agent_mode(name))
                        session_usage=session_usage
                        skills=skills
                        tools=tools
                        on_open_tools=Callback::new(move |()| on_open_tools())
                        focus_request=composer_focus_request
                    />
                </div>
                <Show when=move || main_view.get() == MainView::Subagents>
                    <SubagentsPage
                        agents=subagent_agents
                        tasks=spawn_tasks
                        acp_enabled=subagents_acp_enabled
                        loaded=subagents_loaded
                        sessions=sessions
                        active_chat_id=active_session_id
                        on_refresh=Callback::new(move |()| on_refresh_subagents())
                    />
                </Show>
                <Show when=move || main_view.get() == MainView::Mcps>
                    <McpPresetsPage
                        presets=mcp_presets
                        custom=mcp_custom
                        loaded=mcp_loaded
                        can_modify=mcp_can_modify
                        busy=mcp_busy
                        row_error=mcp_row_error
                        changed=mcp_changed
                        on_refresh=Callback::new(move |()| on_refresh_mcps())
                        on_change=Callback::new(move |request| on_change_mcp(request))
                    />
                </Show>
            </div>
            <SettingsDialog
                open=settings_open
                panel=settings_panel
                actions=settings_actions
                config_file=config_file
                saving=config_saving
                error=config_error
                on_close=Callback::new(move |()| on_close_settings())
                on_open_config_file=Callback::new(move |()| on_open_config_file())
                on_close_config_file=Callback::new(move |()| on_close_config_file())
                on_save_config_file=Callback::new(move |text| on_save_config_file(text))
            />
            <ConfigSavedNotice
                open=config_saved_notice
                restarting=settings_panel.restarting
                error=config_error
                on_restart=settings_actions.on_restart
                on_close=Callback::new(move |()| on_close_config_saved_notice())
            />
        </div>
    }
}
