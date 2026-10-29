//! The "Tools" and "MCP Servers" pages of the Settings dialog: every built-in tool, and
//! every MCP server in `config.json`, each with a switch that turns it on or off globally.
//!
//! Switching writes `config.json` on the gateway (which backs the old file up) but the
//! running bot only picks the change up after a restart, so a row whose switch differs
//! from what the bot is running shows "Will be enabled/disabled after restart", and a
//! banner warns that a restart is needed. A search box filters the list as the user
//! types; rows are always listed A–Z.
//!
//! The row ordering, filtering and labelling are plain functions so they are
//! unit-testable on the host target without Leptos.

use chat_ui::models::{McpServerSettingSummary, ToolConfigSummary, ToolSettingSummary};
use leptos::prelude::*;

use super::general_settings::GeneralSettings;
use super::list_page::Tile;
use super::mcp_presets_page::{is_busy, row_error_for, switch_hint, ToggleSwitch};
use super::tool_config_form::{ToolConfigChange, ToolConfigForm};

/// Tooltip on the switch of a tool the gateway itself depends on.
const PROTECTED_TOOL_HINT: &str = "Required by the gateway; it cannot be switched off.";

/// Banner shown while any switch waits for a restart.
const RESTART_WARNING: &str =
    "Restart the rust-bot process for these changes to take effect. The previous config.json was backed up next to it.";

/// Which page of the Settings dialog's menu is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsPage {
    #[default]
    Tools,
    /// The settings that apply to every tool (workspace restriction, confirmation, SSRF
    /// whitelist), kept apart from the long tool list.
    ToolSafety,
    McpServers,
}

impl SettingsPage {
    /// All pages, in menu order.
    pub const ALL: [SettingsPage; 3] = [
        SettingsPage::Tools,
        SettingsPage::ToolSafety,
        SettingsPage::McpServers,
    ];

    /// Menu entry and page heading.
    pub fn title(self) -> &'static str {
        match self {
            Self::Tools => "Tools",
            Self::ToolSafety => "Tool Safety",
            Self::McpServers => "MCP Servers",
        }
    }

    /// One line under the heading.
    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Tools => "Switch the agent's built-in tools on or off. MCP tools are managed under MCP Servers.",
            Self::ToolSafety => "Limits that apply to all tools: file access, confirmation before running and network access.",
            Self::McpServers => "Switch the configured MCP servers on or off. A server that is off is not started. Servers with many tools can load them on demand, which keeps requests small.",
        }
    }

    /// Plural noun used in the search box and the count line.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::ToolSafety => "settings",
            Self::McpServers => "MCP servers",
        }
    }
}

/// What the page asks the app to do when the user flips a switch: the page it is on, the
/// item's name and whether to turn it on.
pub type ToggleRequest = (SettingsPage, String, bool);

/// One row of either page, in the shape the shared row component draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    pub name: String,
    pub description: String,
    /// What `config.json` says (the switch position).
    pub enabled: bool,
    /// What the running bot has.
    pub active: bool,
    /// Whether the switch can be flipped at all.
    pub toggleable: bool,
    /// The settings form, for a row that stands for a whole tool section.
    pub config: Option<ToolConfigSummary>,
    /// For an MCP server: whether its tools are loaded on demand. `None` for tools.
    pub on_demand: Option<bool>,
}

/// An item the Settings pages list: it has a name, a switch and a running state.
pub trait SettingItem {
    /// The row to draw for this item.
    fn to_row(&self) -> SettingRow;
    /// The item's name, which is also its key in the configuration.
    fn name(&self) -> &str;
    /// Move the switch to `enabled` without touching what the bot is running.
    fn set_enabled(&mut self, enabled: bool);
}

impl SettingItem for ToolSettingSummary {
    fn to_row(&self) -> SettingRow {
        SettingRow {
            name: self.name.clone(),
            description: self.description.clone(),
            enabled: self.enabled,
            active: self.active,
            toggleable: self.toggleable,
            config: self.config.clone(),
            on_demand: None,
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl SettingItem for McpServerSettingSummary {
    fn to_row(&self) -> SettingRow {
        SettingRow {
            name: self.name.clone(),
            description: self.description.clone(),
            enabled: self.enabled,
            active: self.active,
            toggleable: true,
            config: None,
            on_demand: Some(self.deferred),
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

/// Set whether the MCP server called `name` loads its tools on demand, after the gateway
/// confirmed it saved the change. Does nothing when no server has that name.
pub fn apply_on_demand(servers: &mut [McpServerSettingSummary], name: &str, deferred: bool) {
    if let Some(server) = servers.iter_mut().find(|server| server.name == name) {
        server.deferred = deferred;
    }
}

/// Label of the on-demand switch of an MCP server row.
pub const ON_DEMAND_LABEL: &str = "Load tools on demand";

/// Explanation shown next to the on-demand switch.
pub const ON_DEMAND_HINT: &str =
    "Only send this server's tools to the model after it finds them with tool_search. \
     Saves tokens for servers with many tools.";

/// Replace the settings form of the tool called `name` with the gateway's fresh `summary`.
/// Does nothing when no tool has that name.
pub fn apply_tool_config(tools: &mut [ToolSettingSummary], name: &str, summary: ToolConfigSummary) {
    if let Some(tool) = tools.iter_mut().find(|tool| tool.name == name) {
        tool.config = Some(summary);
    }
}

/// Move the switch of the item called `name` to `enabled`, after the gateway confirmed it
/// saved the change. Does nothing when no item has that name.
pub fn apply_enabled<T: SettingItem>(items: &mut [T], name: &str, enabled: bool) {
    if let Some(item) = items.iter_mut().find(|item| item.name() == name) {
        item.set_enabled(enabled);
    }
}

/// Whether `query` matches a row: every word of the query appears in the name or the
/// description, ignoring case. An empty query matches everything.
pub fn matches_query(row: &SettingRow, query: &str) -> bool {
    let haystack = format!("{} {}", row.name, row.description).to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// The rows to show for `query`, sorted A–Z by name ignoring case.
pub fn visible_rows(rows: Vec<SettingRow>, query: &str) -> Vec<SettingRow> {
    let mut visible: Vec<SettingRow> = rows
        .into_iter()
        .filter(|row| matches_query(row, query))
        .collect();
    visible.sort_by_key(|row| row.name.to_lowercase());
    visible
}

/// The note under a row whose switch differs from what the bot is running, or `None`.
pub fn pending_label(row: &SettingRow) -> Option<&'static str> {
    match (row.enabled, row.active) {
        (true, false) => Some("Will be enabled after restart"),
        (false, true) => Some("Will be disabled after restart"),
        _ => None,
    }
}

/// Whether any row waits for a restart.
pub fn any_pending(rows: &[SettingRow]) -> bool {
    rows.iter().any(|row| pending_label(row).is_some())
}

/// The count line: "29 tools", or "3 of 29 tools" while a search hides some.
pub fn count_label(shown: usize, total: usize, noun: &str) -> String {
    if shown == total {
        format!("{total} {noun}")
    } else {
        format!("{shown} of {total} {noun}")
    }
}

/// The general tool settings (the section without an on/off switch), if the gateway sent
/// them.
pub fn general_settings(tools: &[ToolSettingSummary]) -> Option<ToolConfigSummary> {
    tools
        .iter()
        .filter_map(|tool| tool.config.as_ref())
        .find(|config| config.enabled.is_none())
        .cloned()
}

/// The heading of a row: the tool section's title (e.g. "Image generation") when the row
/// has a settings form, else its name.
pub fn row_title(row: &SettingRow) -> String {
    row.config
        .as_ref()
        .map(|config| config.title.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or(&row.name)
        .to_string()
}

/// Whether the row has an on/off switch: a tool section without an `enable` setting
/// (the general tool settings) only has its form.
pub fn has_switch(row: &SettingRow) -> bool {
    row.config
        .as_ref()
        .is_none_or(|config| config.enabled.is_some())
}

/// The tooltip for a row's switch.
fn row_hint(row: &SettingRow, can_modify: bool) -> &'static str {
    if !can_modify {
        switch_hint(false)
    } else if !row.toggleable {
        PROTECTED_TOOL_HINT
    } else {
        ""
    }
}

/// Everything the pages show, as signals owned by the app: which page is open, the two
/// lists, and the state of the switch being changed.
#[derive(Clone, Copy)]
pub struct SettingsPanel {
    pub page: RwSignal<SettingsPage>,
    pub tools: RwSignal<Vec<ToolSettingSummary>>,
    pub tools_loaded: RwSignal<bool>,
    pub mcp_servers: RwSignal<Vec<McpServerSettingSummary>>,
    pub mcp_servers_loaded: RwSignal<bool>,
    /// Whether the gateway lets this connection flip switches (signed in and local).
    pub can_modify: RwSignal<bool>,
    /// The item a change request is in flight for.
    pub busy: RwSignal<Option<String>>,
    /// The tool whose settings form is open, if any.
    pub expanded_tool: RwSignal<Option<String>>,
    /// The last failed request as `(item name, message)`; the name is empty for a
    /// failure of the whole list.
    pub error: RwSignal<Option<(String, String)>>,
    /// Set once any change was saved: the restart is pending for the rest of the session,
    /// whichever page is showing.
    pub restart_notice: RwSignal<bool>,
    /// Whether a restart was requested and the gateway is about to go down.
    pub restarting: RwSignal<bool>,
}

impl SettingsPanel {
    pub fn new() -> Self {
        Self {
            page: RwSignal::new(SettingsPage::default()),
            tools: RwSignal::new(Vec::new()),
            tools_loaded: RwSignal::new(false),
            mcp_servers: RwSignal::new(Vec::new()),
            mcp_servers_loaded: RwSignal::new(false),
            can_modify: RwSignal::new(false),
            busy: RwSignal::new(None),
            expanded_tool: RwSignal::new(None),
            error: RwSignal::new(None),
            restart_notice: RwSignal::new(false),
            restarting: RwSignal::new(false),
        }
    }

    /// The rows of the open page.
    fn open_rows(&self) -> Vec<SettingRow> {
        match self.page.get() {
            // The general settings have their own page.
            SettingsPage::Tools => self
                .tools
                .get()
                .iter()
                .map(SettingItem::to_row)
                .filter(has_switch)
                .collect(),
            SettingsPage::ToolSafety => Vec::new(),
            SettingsPage::McpServers => self
                .mcp_servers
                .get()
                .iter()
                .map(SettingItem::to_row)
                .collect(),
        }
    }

    /// Whether the open page's list has arrived.
    fn open_page_loaded(&self) -> bool {
        match self.page.get() {
            SettingsPage::Tools | SettingsPage::ToolSafety => self.tools_loaded.get(),
            SettingsPage::McpServers => self.mcp_servers_loaded.get(),
        }
    }

    /// Drop both lists and any error, so the next opening starts clean. The restart
    /// notice stays: the restart is still pending.
    pub fn clear(&self) {
        self.tools.set(Vec::new());
        self.tools_loaded.set(false);
        self.mcp_servers.set(Vec::new());
        self.mcp_servers_loaded.set(false);
        self.can_modify.set(false);
        self.busy.set(None);
        self.expanded_tool.set(None);
        self.error.set(None);
    }
}

impl Default for SettingsPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// What the pages can ask the app to do.
#[derive(Clone, Copy)]
pub struct SettingsActions {
    /// Show a page (and fetch its list).
    pub on_select_page: Callback<SettingsPage>,
    /// Flip one switch.
    pub on_toggle: Callback<ToggleRequest>,
    /// Load an MCP server's tools on demand or not: `(server name, deferred)`.
    pub on_set_on_demand: Callback<(String, bool)>,
    /// Re-fetch the open page's list.
    pub on_refresh: Callback<()>,
    /// Save the changed settings of one tool section.
    pub on_save_config: Callback<ToolConfigChange>,
    /// Restart the whole bot process so the saved changes take effect.
    pub on_restart: Callback<()>,
}

/// Label of the button that restarts the bot.
pub fn restart_button_label(restarting: bool) -> &'static str {
    if restarting {
        "Restarting…"
    } else {
        "Restart now"
    }
}

/// One row: tile, name, description, restart note, error line and switch.
#[component]
fn SettingRowView(
    row: SettingRow,
    page: SettingsPage,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let name = row.name.clone();
    let name_for_toggle = row.name.clone();
    let name_for_error = row.name.clone();
    let name_for_busy = row.name.clone();
    let enabled = row.enabled;
    let toggleable = row.toggleable;
    let pending = pending_label(&row);
    let hint_row = row.clone();
    let config = row.config.clone();
    let title = row_title(&row);
    let switch_shown = has_switch(&row);
    let name_for_expand = row.name.clone();
    let name_for_form_busy = row.name.clone();
    let name_for_open = row.name.clone();
    let name_for_on_demand = row.name.clone();
    let on_demand = row.on_demand;
    let disabled = Signal::derive(move || {
        !panel.can_modify.get() || !toggleable || is_busy(&panel.busy.get(), &name_for_busy)
    });
    view! {
        <li class="py-3">
            <div class="flex items-start gap-4">
                <Tile title=name.clone() />
                <div class="min-w-0 flex-1">
                    <span class="break-words text-sm font-medium text-slate-900">{title}</span>
                    <p class="line-clamp-2 text-sm text-slate-500">{row.description.clone()}</p>
                    {pending
                        .map(|label| {
                            view! { <p class="mt-1 text-xs font-medium text-amber-700">{label}</p> }
                        })}
                    {on_demand
                        .map(|deferred| {
                            view! {
                                <div class="mt-2 flex items-center gap-2" title=ON_DEMAND_HINT>
                                    <ToggleSwitch
                                        on=deferred
                                        label=format!("{name_for_on_demand}: {ON_DEMAND_LABEL}")
                                        disabled=disabled
                                        hint=Signal::derive(move || switch_hint(panel.can_modify.get()))
                                        on_toggle=Callback::new(move |()| {
                                            actions
                                                .on_set_on_demand
                                                .run((name_for_on_demand.clone(), !deferred));
                                        })
                                    />
                                    <span class="text-xs text-slate-500">{ON_DEMAND_LABEL}</span>
                                </div>
                            }
                        })}
                    {config
                        .as_ref()
                        .map(|_| {
                            view! {
                                <button
                                    type="button"
                                    class="mt-2 rounded-full border border-slate-200 px-3 py-1 text-sm text-slate-600 hover:bg-slate-100"
                                    on:click=move |_| {
                                        let target = name_for_expand.clone();
                                        panel
                                            .expanded_tool
                                            .update(|open| {
                                                *open = if open.as_deref() == Some(target.as_str()) {
                                                    None
                                                } else {
                                                    Some(target)
                                                };
                                            });
                                    }
                                >
                                    "Settings"
                                </button>
                            }
                        })}
                </div>
                {switch_shown
                    .then(|| {
                        view! {
                            <ToggleSwitch
                                on=enabled
                                label=format!("{name} enabled")
                                disabled=disabled
                                hint=Signal::derive(move || row_hint(&hint_row, panel.can_modify.get()))
                                on_toggle=Callback::new(move |()| {
                                    actions.on_toggle.run((page, name_for_toggle.clone(), !enabled));
                                })
                            />
                        }
                    })}
            </div>
            {config
                .map(|config| {
                    let form_busy = Signal::derive(move || {
                        is_busy(&panel.busy.get(), &name_for_form_busy)
                    });
                    view! {
                        <Show when=move || {
                            panel.expanded_tool.get().as_deref() == Some(name_for_open.as_str())
                        }>
                            <div class="mt-3">
                                <ToolConfigForm
                                    config=config.clone()
                                    can_modify=panel.can_modify
                                    busy=form_busy
                                    on_save=actions.on_save_config
                                />
                            </div>
                        </Show>
                    }
                })}
            {move || {
                panel
                    .error
                    .get()
                    .filter(|(failed_name, _)| *failed_name == name_for_error)
                    .map(|(_, message)| {
                        view! { <p class="mt-2 text-xs text-red-600 sm:ml-16">{message}</p> }
                    })
            }}
        </li>
    }
}

/// The open page: heading, restart warning, search box and the filtered list.
#[component]
pub fn SettingsPageView(
    panel: SettingsPanel,
    actions: SettingsActions,
    /// The search text; owned by the dialog so it can clear it when the page changes.
    search: RwSignal<String>,
) -> impl IntoView {
    let page = move || panel.page.get();
    let rows_pending = move || any_pending(&panel.open_rows());
    let show_restart_warning = move || panel.restart_notice.get() || rows_pending();
    view! {
        <div class="flex min-h-0 flex-1 flex-col">
            <div class="flex items-start justify-between gap-4">
                <div>
                    <h3 class="text-base font-semibold text-slate-900">{move || page().title()}</h3>
                    <p class="line-clamp-2 text-sm text-slate-500 sm:line-clamp-none">
                        {move || page().subtitle()}
                    </p>
                </div>
                <button
                    type="button"
                    class="rounded-full px-3 py-1.5 text-sm text-slate-600 hover:bg-slate-100"
                    on:click=move |_| actions.on_refresh.run(())
                >
                    "Refresh"
                </button>
            </div>
            <Show when=show_restart_warning>
                <p
                    role="status"
                    class="mt-3 rounded-lg bg-amber-50 px-3 py-2 text-sm text-amber-800"
                >
                    {RESTART_WARNING}
                    <button
                        type="button"
                        class="ml-3 rounded-lg bg-amber-600 px-2.5 py-1 text-xs font-medium text-white hover:bg-amber-700 disabled:opacity-60"
                        disabled=move || panel.restarting.get()
                        on:click=move |_| actions.on_restart.run(())
                    >
                        {move || restart_button_label(panel.restarting.get())}
                    </button>
                </p>
            </Show>
            {move || {
                panel
                    .error
                    .get()
                    .filter(|(name, _)| name.is_empty())
                    .map(|(_, message)| {
                        view! {
                            <p role="alert" class="mt-3 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-700">
                                {message}
                            </p>
                        }
                    })
            }}
            <Show when=move || page() == SettingsPage::ToolSafety>
                <div class="min-h-0 flex-1 overflow-y-auto">
                    {move || {
                        if !panel.tools_loaded.get() {
                            return view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }
                                .into_any();
                        }
                        match general_settings(&panel.tools.get()) {
                            Some(config) => {
                                let tool = config.name.clone();
                                let busy_tool = tool.clone();
                                view! {
                                    <GeneralSettings
                                        config=config
                                        can_modify=panel.can_modify
                                        busy=Signal::derive(move || {
                                            is_busy(&panel.busy.get(), &busy_tool)
                                        })
                                        error=Signal::derive(move || {
                                            row_error_for(&panel.error.get(), &tool)
                                        })
                                        on_save=actions.on_save_config
                                    />
                                }
                                    .into_any()
                            }
                            None => {
                                view! {
                                    <p class="py-4 text-sm text-slate-500">
                                        "No general tool settings available."
                                    </p>
                                }
                                    .into_any()
                            }
                        }
                    }}
                </div>
            </Show>
            <Show when=move || page() != SettingsPage::ToolSafety>
            <input
                type="search"
                aria-label=move || format!("Search {}", page().noun())
                placeholder=move || format!("Search {}", page().noun())
                autocomplete="off"
                spellcheck="false"
                class="mt-3 w-full rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm text-slate-900 outline-none focus:border-orange-500"
                prop:value=move || search.get()
                on:input=move |event| search.set(event_target_value(&event))
            />
            <Show
                when=move || panel.open_page_loaded()
                fallback=|| view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }
            >
                {move || {
                    let all_rows = panel.open_rows();
                    let total = all_rows.len();
                    let rows = visible_rows(all_rows, &search.get());
                    let count = count_label(rows.len(), total, page().noun());
                    let empty_message = if total == 0 {
                        format!("No {} configured.", page().noun())
                    } else {
                        format!("No {} match your search.", page().noun())
                    };
                    let current_page = page();
                    view! {
                        <p class="pt-4 pb-1 text-xs text-slate-400">{count}</p>
                        <div class="min-h-0 flex-1 overflow-y-auto">
                            {rows
                                .is_empty()
                                .then(|| {
                                    view! { <p class="py-4 text-sm text-slate-500">{empty_message}</p> }
                                })}
                            <ul class="divide-y divide-slate-100">
                                {rows
                                    .into_iter()
                                    .map(|row| {
                                        view! {
                                            <SettingRowView
                                                row=row
                                                page=current_page
                                                panel=panel
                                                actions=actions
                                            />
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        </div>
                    }
                }}
            </Show>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, description: &str, enabled: bool, active: bool) -> SettingRow {
        SettingRow {
            name: name.to_string(),
            description: description.to_string(),
            enabled,
            active,
            toggleable: true,
            config: None,
            on_demand: None,
        }
    }

    fn tool(name: &str, enabled: bool) -> ToolSettingSummary {
        ToolSettingSummary {
            name: name.to_string(),
            description: String::new(),
            enabled,
            active: enabled,
            toggleable: true,
            config: None,
        }
    }

    fn section_row(name: &str, title: &str, enabled: Option<bool>) -> SettingRow {
        SettingRow {
            config: Some(ToolConfigSummary {
                name: name.to_string(),
                title: title.to_string(),
                description: String::new(),
                enabled,
                tool_names: Vec::new(),
                fields: Vec::new(),
            }),
            ..row(name, "", true, true)
        }
    }

    #[test]
    fn general_settings_are_found_by_having_no_enable_setting() {
        let mut tools = vec![tool("grep", true)];
        assert_eq!(general_settings(&tools), None);
        tools.push(ToolSettingSummary {
            config: section_row("general", "General", None).config,
            ..tool("general", true)
        });
        tools.push(ToolSettingSummary {
            config: section_row("gmail", "Gmail", Some(false)).config,
            ..tool("gmail", false)
        });
        assert_eq!(general_settings(&tools).unwrap().name, "general");
    }

    #[test]
    fn section_rows_are_titled_and_plain_rows_keep_their_name() {
        assert_eq!(
            row_title(&section_row(
                "imageGeneration",
                "Image generation",
                Some(true)
            )),
            "Image generation"
        );
        assert_eq!(row_title(&section_row("general", "", None)), "general");
        assert_eq!(row_title(&row("grep", "", true, true)), "grep");
    }

    #[test]
    fn only_a_section_without_an_enable_setting_lacks_a_switch() {
        assert!(has_switch(&row("grep", "", true, true)));
        assert!(has_switch(&section_row("gmail", "Gmail", Some(false))));
        assert!(!has_switch(&section_row("general", "General", None)));
    }

    #[test]
    fn visible_rows_sorts_a_to_z_ignoring_case() {
        let rows = vec![
            row("write_file", "", true, true),
            row("Edit", "", true, true),
            row("grep", "", true, true),
        ];

        let names: Vec<String> = visible_rows(rows, "").into_iter().map(|r| r.name).collect();

        assert_eq!(names, vec!["Edit", "grep", "write_file"]);
    }

    #[test]
    fn search_matches_name_or_description_ignoring_case() {
        let rows = vec![
            row("grep", "Search file contents", true, true),
            row("shell", "Run a command", true, true),
        ];

        assert_eq!(visible_rows(rows.clone(), "GREP").len(), 1);
        assert_eq!(visible_rows(rows.clone(), "command")[0].name, "shell");
        assert_eq!(visible_rows(rows.clone(), "  ").len(), 2);
        assert!(visible_rows(rows, "nothing like this").is_empty());
    }

    #[test]
    fn search_requires_every_word_to_match() {
        let rows = vec![row("web_search", "Search the web", true, true)];

        assert_eq!(visible_rows(rows.clone(), "web search").len(), 1);
        assert!(visible_rows(rows, "web files").is_empty());
    }

    #[test]
    fn pending_label_appears_only_when_switch_and_running_state_differ() {
        assert_eq!(
            pending_label(&row("a", "", true, false)),
            Some("Will be enabled after restart")
        );
        assert_eq!(
            pending_label(&row("a", "", false, true)),
            Some("Will be disabled after restart")
        );
        assert_eq!(pending_label(&row("a", "", true, true)), None);
        assert_eq!(pending_label(&row("a", "", false, false)), None);
    }

    #[test]
    fn any_pending_is_true_when_one_row_waits() {
        let rows = vec![row("a", "", true, true), row("b", "", false, true)];
        assert!(any_pending(&rows));
        assert!(!any_pending(&rows[..1]));
        assert!(!any_pending(&[]));
    }

    #[test]
    fn count_label_shows_the_filtered_count_only_when_filtering() {
        assert_eq!(count_label(29, 29, "tools"), "29 tools");
        assert_eq!(count_label(3, 29, "tools"), "3 of 29 tools");
        assert_eq!(count_label(0, 0, "MCP servers"), "0 MCP servers");
    }

    #[test]
    fn apply_enabled_moves_only_the_named_switch() {
        let mut tools = vec![tool("grep", true), tool("shell", true)];

        apply_enabled(&mut tools, "grep", false);

        assert!(!tools[0].enabled);
        assert!(tools[0].active, "the running state is untouched");
        assert!(tools[1].enabled);
    }

    #[test]
    fn apply_enabled_ignores_unknown_names() {
        let mut tools = vec![tool("grep", true)];
        apply_enabled(&mut tools, "ghost", false);
        assert!(tools[0].enabled);
    }

    fn mcp_server(name: &str, deferred: bool) -> McpServerSettingSummary {
        McpServerSettingSummary {
            name: name.to_string(),
            description: String::new(),
            enabled: true,
            active: true,
            deferred,
        }
    }

    #[test]
    fn mcp_server_rows_carry_the_on_demand_switch_and_tool_rows_do_not() {
        assert_eq!(mcp_server("cms", true).to_row().on_demand, Some(true));
        assert_eq!(mcp_server("files", false).to_row().on_demand, Some(false));
        assert_eq!(tool("grep", true).to_row().on_demand, None);
    }

    #[test]
    fn apply_on_demand_updates_only_the_named_server() {
        let mut servers = vec![mcp_server("cms", false), mcp_server("files", false)];

        apply_on_demand(&mut servers, "cms", true);
        apply_on_demand(&mut servers, "ghost", true);

        assert!(servers[0].deferred);
        assert!(!servers[1].deferred);
    }

    #[test]
    fn mcp_servers_are_always_toggleable_and_tools_keep_their_lock() {
        let server = mcp_server("files", false);
        let mut locked = tool("message", true);
        locked.toggleable = false;

        assert!(server.to_row().toggleable);
        assert!(!locked.to_row().toggleable);
    }

    #[test]
    fn row_hint_explains_why_a_switch_is_locked() {
        let mut locked = row("message", "", true, true);
        locked.toggleable = false;

        assert_eq!(row_hint(&locked, true), PROTECTED_TOOL_HINT);
        assert_eq!(row_hint(&locked, false), switch_hint(false));
        assert_eq!(row_hint(&row("a", "", true, true), true), "");
    }

    #[test]
    fn pages_have_titles_in_menu_order() {
        let titles: Vec<&str> = SettingsPage::ALL.iter().map(|p| p.title()).collect();
        assert_eq!(titles, vec!["Tools", "Tool Safety", "MCP Servers"]);
        assert_eq!(SettingsPage::default(), SettingsPage::Tools);
    }

    #[test]
    fn restart_button_says_what_it_is_doing() {
        assert_eq!(restart_button_label(false), "Restart now");
        assert_eq!(restart_button_label(true), "Restarting…");
    }

    #[test]
    fn restart_warning_mentions_the_backup() {
        assert!(RESTART_WARNING.contains("Restart the rust-bot process"));
        assert!(RESTART_WARNING.contains("backed up"));
    }
}
