//! The "MCPs" page: the MCP server presets and custom MCP servers, each with a switch
//! showing whether it is turned on.
//!
//! Replaces the chat area while open (see `chat_shell.rs`). The data comes from the
//! gateway's `mcp_presets` event. When the gateway says this connection may change MCP
//! settings (`can_modify`: signed in and running on the gateway machine), the switches
//! work: turning a preset on that still needs credentials opens an inline form for them,
//! and every change is marked as needing a restart. Otherwise the whole page is
//! read-only.
//!
//! The ordering, labelling and form rules are plain functions so they are unit-testable
//! on the host target without Leptos.

use std::collections::HashMap;

use chat_ui::models::{McpCustomServerSummary, McpPresetFieldSummary, McpPresetSummary};
use leptos::prelude::*;

use super::list_page::{ListPageShell, SectionHeading, Tile};

/// Tooltip on the switches when this connection may not change MCP settings.
const READ_ONLY_HINT: &str =
    "Read-only: MCP servers can only be changed from a signed-in browser on the gateway machine.";

/// Everything the page reports when the user changes a preset: its name, whether to turn
/// it on, and the values typed into the credentials form (empty when there was none).
pub type McpChangeRequest = (String, bool, HashMap<String, String>);

/// Presets ordered for display: enabled first, then alphabetically by display name
/// (falling back to the name when a preset has none), ignoring case.
pub fn sorted_presets(presets: &[McpPresetSummary]) -> Vec<McpPresetSummary> {
    let mut sorted = presets.to_vec();
    sorted.sort_by_key(|preset| (!preset.enabled, preset_title(preset).to_lowercase()));
    sorted
}

/// The preset's display name, or its identifier when no display name was given.
pub fn preset_title(preset: &McpPresetSummary) -> &str {
    if preset.display_name.is_empty() {
        &preset.name
    } else {
        &preset.display_name
    }
}

/// How many of `presets` are enabled.
pub fn enabled_count(presets: &[McpPresetSummary]) -> usize {
    presets.iter().filter(|preset| preset.enabled).count()
}

/// Short badge text for a preset's readiness `status`, or `None` when no badge is
/// needed (`configured`, `not_installed`, or an unknown value).
pub fn status_badge_label(status: &str) -> Option<&'static str> {
    match status {
        "missing_credentials" => Some("Needs credentials"),
        "missing_dependency" => Some("Missing dependency"),
        "coming_soon" => Some("Coming soon"),
        _ => None,
    }
}

/// Human-readable transport name for the small label under a custom server's name.
pub fn transport_label(transport: &str) -> &str {
    match transport {
        "stdio" => "stdio",
        "streamableHttp" => "HTTP",
        "sse" => "SSE",
        "oauth" => "OAuth",
        _ => "unknown transport",
    }
}

/// The fields the credentials form asks for: every field without a usable value yet,
/// required ones first.
pub fn fields_needing_input(preset: &McpPresetSummary) -> Vec<McpPresetFieldSummary> {
    let mut fields: Vec<McpPresetFieldSummary> = preset
        .fields
        .iter()
        .filter(|field| !field.configured)
        .cloned()
        .collect();
    fields.sort_by_key(|field| !field.required);
    fields
}

/// Whether turning the preset on needs the credentials form: a required field has no
/// value yet.
pub fn needs_credentials(preset: &McpPresetSummary) -> bool {
    preset
        .fields
        .iter()
        .any(|field| field.required && !field.configured)
}

/// Whether the form can be submitted: every required field it asks for is filled in
/// (blank or whitespace-only counts as empty).
pub fn can_submit(fields: &[McpPresetFieldSummary], values: &HashMap<String, String>) -> bool {
    fields.iter().filter(|field| field.required).all(|field| {
        values
            .get(&field.name)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// The values to send: trimmed, non-empty, and only for fields the form asked about.
pub fn submitted_values(
    fields: &[McpPresetFieldSummary],
    values: &HashMap<String, String>,
) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|field| {
            let value = values.get(&field.name)?.trim();
            (!value.is_empty()).then(|| (field.name.clone(), value.to_string()))
        })
        .collect()
}

/// Banner text listing the presets changed since the page was loaded, or `None` when
/// nothing needs a restart.
pub fn restart_banner_text(changed: &[String]) -> Option<String> {
    (!changed.is_empty()).then(|| {
        format!(
            "Restart rust-bot to apply your changes ({}).",
            changed.join(", ")
        )
    })
}

/// Tooltip for a switch: the read-only explanation when changes are not allowed.
pub fn switch_hint(can_modify: bool) -> &'static str {
    if can_modify {
        ""
    } else {
        READ_ONLY_HINT
    }
}

/// Whether `busy` (the preset currently being changed) is `name`.
pub fn is_busy(busy: &Option<String>, name: &str) -> bool {
    busy.as_deref() == Some(name)
}

/// The error to show under `name`'s row, if the last failed change was for it.
pub fn row_error_for(error: &Option<(String, String)>, name: &str) -> Option<String> {
    error
        .as_ref()
        .filter(|(failed_name, _)| failed_name == name)
        .map(|(_, message)| message.clone())
}

/// Tailwind classes for a switch track in the given state.
fn switch_track_class(on: bool, disabled: bool) -> &'static str {
    match (on, disabled) {
        (true, false) => "relative inline-flex h-6 w-11 shrink-0 items-center rounded-full bg-emerald-500 transition-colors",
        (true, true) => "relative inline-flex h-6 w-11 shrink-0 cursor-not-allowed items-center rounded-full bg-emerald-500 opacity-60",
        (false, false) => "relative inline-flex h-6 w-11 shrink-0 items-center rounded-full bg-slate-300 transition-colors",
        (false, true) => "relative inline-flex h-6 w-11 shrink-0 cursor-not-allowed items-center rounded-full bg-slate-300 opacity-60",
    }
}

/// Tailwind classes for a switch knob in the given state.
fn switch_knob_class(on: bool) -> &'static str {
    if on {
        "inline-block h-5 w-5 translate-x-5 rounded-full bg-white shadow transition-transform"
    } else {
        "inline-block h-5 w-5 translate-x-0.5 rounded-full bg-white shadow transition-transform"
    }
}

/// An on/off switch. Inert while `disabled` (read-only page, or a change in flight).
#[component]
pub(crate) fn ToggleSwitch(
    on: bool,
    label: String,
    #[prop(into)] disabled: Signal<bool>,
    #[prop(into)] hint: Signal<&'static str>,
    on_toggle: Callback<()>,
) -> impl IntoView {
    view! {
        <span title=move || hint.get() class=move || if disabled.get() { "cursor-not-allowed" } else { "" }>
            <button
                type="button"
                role="switch"
                aria-checked=on.to_string()
                aria-label=label
                disabled=move || disabled.get()
                class=move || switch_track_class(on, disabled.get())
                on:click=move |_| on_toggle.run(())
            >
                <span class=switch_knob_class(on)></span>
            </button>
        </span>
    }
}

/// The inline form asking for the values a preset still needs. Values live only in this
/// component's local signal and are dropped when it closes.
#[component]
fn CredentialsForm(
    preset_name: String,
    fields: Vec<McpPresetFieldSummary>,
    on_submit: Callback<McpChangeRequest>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let values = RwSignal::new(HashMap::<String, String>::new());
    let fields_for_submit = fields.clone();
    let fields_for_check = fields.clone();
    let submit_ready = move || can_submit(&fields_for_check, &values.get());
    let submit = move |_| {
        let to_send = submitted_values(&fields_for_submit, &values.get_untracked());
        values.set(HashMap::new());
        on_submit.run((preset_name.clone(), true, to_send));
    };
    view! {
        <div class="mb-3 ml-16 rounded-xl border border-slate-200 bg-slate-50 p-4">
            <p class="mb-3 text-xs text-slate-500">
                "Stored in config.json on this machine. Fields with an exported environment variable don't need a value."
            </p>
            {fields
                .into_iter()
                .map(|field| {
                    let field_name = field.name.clone();
                    let input_type = if field.secret { "password" } else { "text" };
                    let label = if field.required {
                        format!("{} (required)", field.label)
                    } else {
                        format!("{} (optional)", field.label)
                    };
                    view! {
                        <label class="mb-3 block text-sm text-slate-700">
                            <span class="mb-1 block font-medium">{label}</span>
                            <input
                                type=input_type
                                autocomplete="off"
                                spellcheck="false"
                                class="w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm"
                                on:input=move |event| {
                                    let typed = event_target_value(&event);
                                    let name = field_name.clone();
                                    values.update(|map| {
                                        map.insert(name, typed);
                                    });
                                }
                            />
                        </label>
                    }
                })
                .collect_view()}
            <div class="flex gap-2">
                <button
                    type="button"
                    class="rounded-full bg-slate-900 px-4 py-1.5 text-sm font-medium text-white disabled:cursor-not-allowed disabled:opacity-40"
                    disabled=move || !submit_ready()
                    on:click=submit
                >
                    "Enable"
                </button>
                <button
                    type="button"
                    class="rounded-full px-4 py-1.5 text-sm text-slate-600 hover:bg-slate-200"
                    on:click=move |_| on_cancel.run(())
                >
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// One catalog preset row, with its switch, status badge, restart marker, error line and
/// (when open) credentials form.
#[component]
fn PresetRow(
    preset: McpPresetSummary,
    #[prop(into)] can_modify: Signal<bool>,
    #[prop(into)] busy: Signal<Option<String>>,
    #[prop(into)] row_error: Signal<Option<(String, String)>>,
    #[prop(into)] changed: Signal<Vec<String>>,
    on_change: Callback<McpChangeRequest>,
) -> impl IntoView {
    let title = preset_title(&preset).to_string();
    let name = preset.name.clone();
    let badge = status_badge_label(&preset.status);
    let requires = (!preset.requires.is_empty()).then(|| format!("Requires: {}", preset.requires));
    let form_open = RwSignal::new(false);
    let form_fields = fields_needing_input(&preset);
    let wants_form = needs_credentials(&preset);
    let enabled = preset.enabled;
    // An enabled preset that still lacks credentials can be completed from here.
    let can_complete = enabled && wants_form;

    // Inert on a read-only page, and while any change is in flight.
    let switch_disabled = move || !can_modify.get() || busy.get().is_some();
    let name_for_toggle = name.clone();
    let on_toggle = Callback::new(move |()| {
        if enabled {
            on_change.run((name_for_toggle.clone(), false, HashMap::new()));
        } else if wants_form {
            form_open.update(|open| *open = !*open);
        } else {
            on_change.run((name_for_toggle.clone(), true, HashMap::new()));
        }
    });

    let name_for_changed = name.clone();
    let name_for_error = name.clone();
    let name_for_busy = name.clone();
    let name_for_form = name.clone();
    view! {
        <li class="py-3">
            <div class="flex items-center gap-4">
                <Tile title=title.clone() />
                <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                        <span class="text-sm font-medium text-slate-900">{title.clone()}</span>
                        {badge
                            .map(|text| {
                                view! {
                                    <span class="rounded-md bg-amber-50 px-1.5 py-0.5 text-[11px] font-medium text-amber-700">
                                        {text}
                                    </span>
                                }
                            })}
                        <Show when=move || changed.get().contains(&name_for_changed)>
                            <span class="rounded-md bg-sky-50 px-1.5 py-0.5 text-[11px] font-medium text-sky-700">
                                "Restart to apply"
                            </span>
                        </Show>
                        <Show when=move || is_busy(&busy.get(), &name_for_busy)>
                            <span class="text-[11px] text-slate-400">"Saving…"</span>
                        </Show>
                    </div>
                    <p class="text-sm text-slate-500">{preset.description.clone()}</p>
                    {requires.map(|text| view! { <p class="text-xs text-slate-400">{text}</p> })}
                    {can_complete
                        .then(|| {
                            view! {
                                <Show when=move || can_modify.get()>
                                    <button
                                        type="button"
                                        class="mt-1 text-xs font-medium text-sky-700 hover:underline"
                                        on:click=move |_| form_open.update(|open| *open = !*open)
                                    >
                                        "Set credentials"
                                    </button>
                                </Show>
                            }
                        })}
                </div>
                <ToggleSwitch
                    on=enabled
                    label=format!("{title} enabled")
                    disabled=switch_disabled
                    hint=Signal::derive(move || switch_hint(can_modify.get()))
                    on_toggle=on_toggle
                />
            </div>
            {move || {
                row_error_for(&row_error.get(), &name_for_error)
                    .map(|message| {
                        view! { <p class="mt-2 ml-16 text-xs text-red-600">{message}</p> }
                    })
            }}
            <Show when=move || form_open.get() && can_modify.get()>
                <CredentialsForm
                    preset_name=name_for_form.clone()
                    fields=form_fields.clone()
                    on_submit=Callback::new(move |request: McpChangeRequest| {
                        form_open.set(false);
                        on_change.run(request);
                    })
                    on_cancel=Callback::new(move |()| form_open.set(false))
                />
            </Show>
        </li>
    }
}

/// One custom (non-preset) server row. Custom servers are always on and not changeable
/// from here: they are only listed because they are in the configuration.
#[component]
fn CustomRow(server: McpCustomServerSummary) -> impl IntoView {
    let transport = transport_label(&server.transport).to_string();
    view! {
        <li class="flex items-center gap-4 py-3">
            <Tile title=server.name.clone() />
            <div class="min-w-0 flex-1">
                <span class="text-sm font-medium text-slate-900">{server.name.clone()}</span>
                <p class="text-sm text-slate-500">{format!("Custom server ({transport})")}</p>
            </div>
            <ToggleSwitch
                on=true
                label=format!("{} enabled", server.name)
                disabled=Signal::stored(true)
                hint=Signal::stored("Custom servers are managed in config.json.")
                on_toggle=Callback::new(|()| {})
            />
        </li>
    }
}

/// The MCP list page.
///
/// `loaded` is false while the first reply is in flight (shows a loading line).
/// `can_modify` comes from the gateway; `busy` is the preset being changed right now,
/// `row_error` the last failed change (`(preset name, message)`), `changed` the presets
/// changed since load (they need a restart). `on_refresh` asks for a fresh list and
/// `on_change` requests a change.
#[component]
pub fn McpPresetsPage(
    #[prop(into)] presets: Signal<Vec<McpPresetSummary>>,
    #[prop(into)] custom: Signal<Vec<McpCustomServerSummary>>,
    #[prop(into)] loaded: Signal<bool>,
    #[prop(into)] can_modify: Signal<bool>,
    #[prop(into)] busy: Signal<Option<String>>,
    #[prop(into)] row_error: Signal<Option<(String, String)>>,
    #[prop(into)] changed: Signal<Vec<String>>,
    on_refresh: Callback<()>,
    on_change: Callback<McpChangeRequest>,
) -> impl IntoView {
    let subtitle = Signal::derive(move || {
        if can_modify.get() {
            "Turn MCP servers on or off. Changes need a restart."
        } else {
            "Read-only: MCP servers can only be changed from a signed-in browser on the gateway machine."
        }
    });
    view! {
        <ListPageShell
            title="MCPs"
            subtitle=subtitle
            refresh_label="Refresh MCP list"
            on_refresh=on_refresh
        >
                {move || {
                    restart_banner_text(&changed.get())
                        .map(|text| {
                            view! {
                                <p class="mt-4 rounded-lg bg-sky-50 px-3 py-2 text-sm text-sky-800">
                                    {text}
                                </p>
                            }
                        })
                }}
                <Show
                    when=move || loaded.get()
                    fallback=|| view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }
                >
                    {move || {
                        let preset_rows = sorted_presets(&presets.get());
                        let enabled = enabled_count(&preset_rows);
                        let total = preset_rows.len();
                        let custom_rows = custom.get();
                        let custom_total = custom_rows.len();
                        view! {
                            <SectionHeading
                                title="Presets"
                                count=total
                                detail=Some(format!("{enabled} enabled"))
                            />
                            <ul class="divide-y divide-slate-100">
                                {preset_rows
                                    .into_iter()
                                    .map(|preset| {
                                        view! {
                                            <PresetRow
                                                preset=preset
                                                can_modify=can_modify
                                                busy=busy
                                                row_error=row_error
                                                changed=changed
                                                on_change=on_change
                                            />
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                            {(custom_total > 0)
                                .then(|| {
                                    view! {
                                        <SectionHeading title="Custom" count=custom_total detail=None />
                                        <ul class="divide-y divide-slate-100">
                                            {custom_rows
                                                .into_iter()
                                                .map(|server| view! { <CustomRow server=server /> })
                                                .collect_view()}
                                        </ul>
                                    }
                                })}
                        }
                    }}
                </Show>
        </ListPageShell>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str, display_name: &str, enabled: bool) -> McpPresetSummary {
        McpPresetSummary {
            name: name.to_string(),
            display_name: display_name.to_string(),
            description: String::new(),
            category: String::new(),
            transport: "stdio".to_string(),
            enabled,
            status: "not_installed".to_string(),
            install_supported: true,
            requires: String::new(),
            note: String::new(),
            fields: Vec::new(),
        }
    }

    fn field(name: &str, required: bool, configured: bool) -> McpPresetFieldSummary {
        McpPresetFieldSummary {
            name: name.to_string(),
            label: format!("{name} label"),
            required,
            secret: true,
            configured,
        }
    }

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn titles(presets: &[McpPresetSummary]) -> Vec<&str> {
        presets.iter().map(preset_title).collect()
    }

    #[test]
    fn sorted_presets_puts_enabled_first_then_alphabetical_ignoring_case() {
        let presets = vec![
            preset("zeta", "Zeta", false),
            preset("beta", "beta", true),
            preset("alpha", "Alpha", false),
            preset("omega", "Omega", true),
        ];
        assert_eq!(
            titles(&sorted_presets(&presets)),
            vec!["beta", "Omega", "Alpha", "Zeta"]
        );
    }

    #[test]
    fn preset_title_falls_back_to_name() {
        assert_eq!(preset_title(&preset("github", "", false)), "github");
        assert_eq!(preset_title(&preset("github", "GitHub", false)), "GitHub");
    }

    #[test]
    fn enabled_count_counts_only_enabled() {
        let presets = vec![
            preset("a", "A", true),
            preset("b", "B", false),
            preset("c", "C", true),
        ];
        assert_eq!(enabled_count(&presets), 2);
        assert_eq!(enabled_count(&[]), 0);
    }

    #[test]
    fn status_badge_label_only_for_problem_states() {
        assert_eq!(
            status_badge_label("missing_credentials"),
            Some("Needs credentials")
        );
        assert_eq!(
            status_badge_label("missing_dependency"),
            Some("Missing dependency")
        );
        assert_eq!(status_badge_label("coming_soon"), Some("Coming soon"));
        assert_eq!(status_badge_label("configured"), None);
        assert_eq!(status_badge_label("not_installed"), None);
        assert_eq!(status_badge_label("something-new"), None);
    }

    #[test]
    fn transport_label_names_known_transports() {
        assert_eq!(transport_label("stdio"), "stdio");
        assert_eq!(transport_label("streamableHttp"), "HTTP");
        assert_eq!(transport_label("sse"), "SSE");
        assert_eq!(transport_label("oauth"), "OAuth");
        assert_eq!(transport_label("unknown"), "unknown transport");
    }

    #[test]
    fn fields_needing_input_skips_configured_fields_and_lists_required_first() {
        let mut p = preset("x", "X", false);
        p.fields = vec![
            field("optional_new", false, false),
            field("done", true, true),
            field("required_new", true, false),
        ];
        let names: Vec<String> = fields_needing_input(&p)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["required_new", "optional_new"]);
    }

    #[test]
    fn needs_credentials_only_when_a_required_field_has_no_value() {
        let mut p = preset("x", "X", false);
        assert!(!needs_credentials(&p));
        p.fields = vec![field("opt", false, false), field("req", true, true)];
        assert!(!needs_credentials(&p));
        p.fields.push(field("req2", true, false));
        assert!(needs_credentials(&p));
    }

    #[test]
    fn can_submit_requires_every_required_field_to_be_non_blank() {
        let fields = vec![field("a", true, false), field("b", false, false)];
        assert!(!can_submit(&fields, &values(&[])));
        assert!(!can_submit(&fields, &values(&[("a", "   ")])));
        assert!(!can_submit(&fields, &values(&[("b", "x")])));
        assert!(can_submit(&fields, &values(&[("a", "x")])));
        assert!(can_submit(&[], &values(&[])));
    }

    #[test]
    fn submitted_values_trims_drops_blanks_and_ignores_unasked_fields() {
        let fields = vec![field("a", true, false), field("b", false, false)];
        let sent = submitted_values(
            &fields,
            &values(&[("a", "  secret  "), ("b", "  "), ("other", "x")]),
        );
        assert_eq!(sent, values(&[("a", "secret")]));
    }

    #[test]
    fn restart_banner_text_lists_changed_presets() {
        assert_eq!(restart_banner_text(&[]), None);
        assert_eq!(
            restart_banner_text(&["github".to_string(), "exa".to_string()]),
            Some("Restart rust-bot to apply your changes (github, exa).".to_string())
        );
    }

    #[test]
    fn switch_hint_explains_read_only_only_when_not_allowed() {
        assert_eq!(switch_hint(true), "");
        assert!(switch_hint(false).starts_with("Read-only"));
    }

    #[test]
    fn is_busy_matches_only_the_preset_being_changed() {
        assert!(is_busy(&Some("github".to_string()), "github"));
        assert!(!is_busy(&Some("github".to_string()), "exa"));
        assert!(!is_busy(&None, "github"));
    }

    #[test]
    fn row_error_for_shows_the_error_only_under_its_own_row() {
        let error = Some(("github".to_string(), "boom".to_string()));
        assert_eq!(row_error_for(&error, "github"), Some("boom".to_string()));
        assert_eq!(row_error_for(&error, "exa"), None);
        assert_eq!(row_error_for(&None, "github"), None);
    }

    #[test]
    fn switch_classes_differ_by_state() {
        assert_ne!(
            switch_track_class(true, false),
            switch_track_class(false, false)
        );
        assert_ne!(
            switch_track_class(true, false),
            switch_track_class(true, true)
        );
        assert_ne!(switch_knob_class(true), switch_knob_class(false));
    }
}
