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

use chat_ui::models::{McpServerSettingSummary, ToolSettingSummary};
use leptos::prelude::*;

use super::list_page::Tile;
use super::mcp_presets_page::{is_busy, switch_hint, ToggleSwitch};

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
    McpServers,
}

impl SettingsPage {
    /// Both pages, in menu order.
    pub const ALL: [SettingsPage; 2] = [SettingsPage::Tools, SettingsPage::McpServers];

    /// Menu entry and page heading.
    pub fn title(self) -> &'static str {
        match self {
            Self::Tools => "Tools",
            Self::McpServers => "MCP Servers",
        }
    }

    /// One line under the heading.
    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Tools => "Switch the agent's built-in tools on or off. MCP tools are managed under MCP Servers.",
            Self::McpServers => "Switch the configured MCP servers on or off. A server that is off is not started.",
        }
    }

    /// Plural noun used in the search box and the count line.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Tools => "tools",
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
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
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
    /// The last failed request as `(item name, message)`; the name is empty for a
    /// failure of the whole list.
    pub error: RwSignal<Option<(String, String)>>,
    /// Set once any change was saved: the restart is pending for the rest of the session,
    /// whichever page is showing.
    pub restart_notice: RwSignal<bool>,
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
            error: RwSignal::new(None),
            restart_notice: RwSignal::new(false),
        }
    }

    /// The rows of the open page.
    fn open_rows(&self) -> Vec<SettingRow> {
        match self.page.get() {
            SettingsPage::Tools => self.tools.get().iter().map(SettingItem::to_row).collect(),
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
            SettingsPage::Tools => self.tools_loaded.get(),
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
    /// Re-fetch the open page's list.
    pub on_refresh: Callback<()>,
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
    let disabled = Signal::derive(move || {
        !panel.can_modify.get() || !toggleable || is_busy(&panel.busy.get(), &name_for_busy)
    });
    view! {
        <li class="py-3">
            <div class="flex items-start gap-4">
                <Tile title=name.clone() />
                <div class="min-w-0 flex-1">
                    <span class="break-all text-sm font-medium text-slate-900">{name.clone()}</span>
                    <p class="line-clamp-2 text-sm text-slate-500">{row.description.clone()}</p>
                    {pending
                        .map(|label| {
                            view! { <p class="mt-1 text-xs font-medium text-amber-700">{label}</p> }
                        })}
                </div>
                <ToggleSwitch
                    on=enabled
                    label=format!("{name} enabled")
                    disabled=disabled
                    hint=Signal::derive(move || row_hint(&hint_row, panel.can_modify.get()))
                    on_toggle=Callback::new(move |()| {
                        actions.on_toggle.run((page, name_for_toggle.clone(), !enabled));
                    })
                />
            </div>
            {move || {
                panel
                    .error
                    .get()
                    .filter(|(failed_name, _)| *failed_name == name_for_error)
                    .map(|(_, message)| {
                        view! { <p class="mt-2 ml-16 text-xs text-red-600">{message}</p> }
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
                    <p class="text-sm text-slate-500">{move || page().subtitle()}</p>
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
        }
    }

    fn tool(name: &str, enabled: bool) -> ToolSettingSummary {
        ToolSettingSummary {
            name: name.to_string(),
            description: String::new(),
            enabled,
            active: enabled,
            toggleable: true,
        }
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

    #[test]
    fn mcp_servers_are_always_toggleable_and_tools_keep_their_lock() {
        let server = McpServerSettingSummary {
            name: "files".to_string(),
            description: String::new(),
            enabled: true,
            active: true,
        };
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
        assert_eq!(titles, vec!["Tools", "MCP Servers"]);
        assert_eq!(SettingsPage::default(), SettingsPage::Tools);
    }

    #[test]
    fn restart_warning_mentions_the_backup() {
        assert!(RESTART_WARNING.contains("Restart the rust-bot process"));
        assert!(RESTART_WARNING.contains("backed up"));
    }
}
