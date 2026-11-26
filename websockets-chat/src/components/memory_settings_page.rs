//! The Dream, Skills and Consolidator pages of the Settings dialog. Each page shows the
//! one settings form the gateway sends for its section (`memory_settings` event), laid
//! out like the Tool Safety page: one row per setting, saved as soon as it is changed.
//! Changes apply after a restart; the page's restart banner says so.

use chat_ui::models::ToolConfigSummary;
use leptos::prelude::*;

use super::general_settings::GeneralSettings;
use super::mcp_presets_page::{is_busy, row_error_for};
use super::settings_pages::{SettingsActions, SettingsPanel};

/// Shown when the gateway sent no form for the open page (e.g. an older gateway).
const NO_FORM_MESSAGE: &str = "These settings are not available from this gateway.";

/// The form of the section `section` among the `sections` the gateway sent.
pub fn section_form(sections: &[ToolConfigSummary], section: &str) -> Option<ToolConfigSummary> {
    sections.iter().find(|form| form.name == section).cloned()
}

/// The open memory page's settings: loading, its form, or a note that there is none.
#[component]
pub fn MemorySettingsSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        <div class="min-h-0 flex-1 overflow-y-auto">
            {move || {
                let Some(section) = panel.page.get().memory_section() else {
                    return ().into_any();
                };
                if !panel.memory_settings_loaded.get() {
                    return view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }
                        .into_any();
                }
                match section_form(&panel.memory_settings.get(), section) {
                    Some(config) => {
                        view! {
                            <GeneralSettings
                                config=config
                                can_modify=Signal::derive(move || panel.can_change())
                                busy=Signal::derive(move || is_busy(&panel.busy.get(), section))
                                error=Signal::derive(move || {
                                    row_error_for(&panel.error.get(), section)
                                })
                                on_save=actions.on_save_memory_settings
                            />
                        }
                            .into_any()
                    }
                    None => {
                        view! { <p class="py-4 text-sm text-slate-500">{NO_FORM_MESSAGE}</p> }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(name: &str) -> ToolConfigSummary {
        serde_json::from_value(serde_json::json!({"name": name, "fields": []})).unwrap()
    }

    #[test]
    fn the_form_of_the_open_section_is_picked_by_name() {
        let sections = vec![form("dream"), form("skills")];
        assert_eq!(section_form(&sections, "skills").unwrap().name, "skills");
        assert!(section_form(&sections, "consolidator").is_none());
    }
}
