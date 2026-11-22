use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::{
    agent::{
        skill_store::{SkillLimits, SkillStore},
        skills::BUILTIN_SKILLS_DIR,
        tools::{
            base::Tool,
            docx::DocxConversionTool,
            filesystem::{EditFileTool, FsToolConfig, ListDirTool, ReadFileTool, WriteFileTool},
            gmail::{GmailEmailDownloadTool, GmailEmailSendTool, GmailEmailsTool},
            gmail_oauth::gmail_unavailable_reason,
            image_generation::ImageGenerationTool,
            ocr::OcrTool,
            question::QuestionTool,
            registry::ToolRegistry,
            search::{GlobTool, GrepTool},
            skill_manage::SkillManageTool,
            startup_warning::ToolStartupWarning,
            web::{WebFetchTool, WebSearchTool},
        },
    },
    bus::queue::MessageBus,
    config::schema::{
        DocxToolConfig, GmailToolConfig, ImageGenerationToolConfig, OcrToolConfig,
        QuestionToolConfig, SkillsToolConfig, WebToolsConfig,
    },
};

/// Workspace restriction and builtin-skills read path used by filesystem tools.
pub fn filesystem_tool_scope(
    workspace: &Path,
    restrict_to_workspace: bool,
    exec_sandbox: &str,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let allowed_dir = if restrict_to_workspace || !exec_sandbox.is_empty() {
        Some(workspace.to_path_buf())
    } else {
        None
    };
    let extra_read = if allowed_dir.is_some() {
        vec![BUILTIN_SKILLS_DIR.clone()]
    } else {
        vec![]
    };
    (allowed_dir, extra_read)
}

pub fn register_filesystem_tools(
    tools: &mut ToolRegistry,
    workspace: &Path,
    allowed_dir: Option<PathBuf>,
    extra_read: Vec<PathBuf>,
) {
    let workspace = Some(workspace.to_path_buf());
    tools.register(Box::new(ReadFileTool::new(
        workspace.clone(),
        allowed_dir.clone(),
        Some(extra_read),
    )));
    for tool in [
        Box::new(WriteFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )) as Box<dyn Tool>,
        Box::new(EditFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )),
        Box::new(ListDirTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )),
        Box::new(GlobTool::new(workspace.clone(), allowed_dir.clone(), None)),
        Box::new(GrepTool::new(workspace.clone(), allowed_dir.clone(), None)),
    ] {
        tools.register(tool);
    }
}

pub fn register_web_tools(web_config: &WebToolsConfig, tools: &mut ToolRegistry) {
    if web_config.enable {
        log::debug!("Registering web tools");
        let timeout_secs = Some(u64::from(web_config.timeout));
        tools.register(Box::new(WebSearchTool::new(
            Some(web_config.search.clone()),
            web_config.proxy.clone(),
            timeout_secs,
        )));
        tools.register(Box::new(WebFetchTool::new(
            None,
            web_config.proxy.clone(),
            timeout_secs,
        )));
    }
}

/// Config section name of the Gmail tools (`tools.gmail`).
pub const GMAIL_TOOL_SECTION: &str = "gmail";

/// Register `gmail`, `gmail_email_send` and `gmail_email_download` when
/// `tools.gmail.enable` is set and both credential files exist.
///
/// When Gmail is enabled but a credential file is missing, registers nothing,
/// logs a warning and returns it so the caller can show it to the user; the
/// process keeps running. Returns `None` when Gmail is disabled or registered.
pub fn register_gmail_tools(
    gmail_config: &GmailToolConfig,
    workspace: &Path,
    tools: &mut ToolRegistry,
) -> Option<ToolStartupWarning> {
    log::info!("Gmail config: {:?}", gmail_config);
    if !gmail_config.enable {
        return None;
    }
    if let Some(reason) = gmail_unavailable_reason(gmail_config) {
        log::warn!("{reason}");
        return Some(ToolStartupWarning::new(GMAIL_TOOL_SECTION, reason));
    }
    log::debug!("Registering gmail tool");
    tools.register(Box::new(GmailEmailsTool::new(gmail_config.clone())));
    tools.register(Box::new(GmailEmailSendTool::new(gmail_config.clone())));
    tools.register(Box::new(GmailEmailDownloadTool::new(
        gmail_config.clone(),
        workspace.to_path_buf(),
    )));
    None
}

pub fn register_ocr_tools(
    ocr_config: &OcrToolConfig,
    workspace: &Path,
    allowed_dir: Option<PathBuf>,
    extra_read: Vec<PathBuf>,
    tools: &mut ToolRegistry,
) {
    if !ocr_config.enable {
        return;
    }
    log::debug!("Registering OCR tool");
    let tool = OcrTool::new(
        ocr_config.clone(),
        workspace.to_path_buf(),
        allowed_dir,
        extra_read,
    );
    tools.register(Box::new(tool));
}

pub fn register_conversion_tools(
    docx_config: &DocxToolConfig,
    fs_tool_config: &FsToolConfig,
    tools: &mut ToolRegistry,
) {
    if !docx_config.enable {
        return;
    }
    log::debug!("Registering DOCX tool");
    let tool = DocxConversionTool::new(fs_tool_config.clone());
    tools.register(Box::new(tool))
}

pub fn register_image_generation_tools(
    image_generation_config: &ImageGenerationToolConfig,
    workspace: &Path,
    tools: &mut ToolRegistry,
) {
    if !image_generation_config.enable {
        return;
    }
    log::debug!("Registering image generation tool");
    let tool = ImageGenerationTool::new(image_generation_config.clone(), workspace.to_path_buf());
    tools.register(Box::new(tool));
}

/// A question tool that waits as long as `tools.question.timeoutSeconds` says.
fn question_tool_from_config(
    question_config: &QuestionToolConfig,
    bus: Arc<MessageBus>,
) -> QuestionTool {
    QuestionTool::new(Some(bus)).with_timeout(Duration::from_secs(question_config.timeout_seconds))
}

pub fn register_question_tool(
    question_config: &QuestionToolConfig,
    bus: Arc<MessageBus>,
    tools: &mut ToolRegistry,
) {
    if !question_config.enabled {
        return;
    }
    log::debug!("Registering question tool");
    tools.register(Box::new(question_tool_from_config(question_config, bus)));
}

/// Register `skill_manage` when `tools.skills.enabled`. Skills are written to
/// `<workspace>/skills`, the folder the skills loader reads first.
pub fn register_skill_manage_tool(
    skills_config: &SkillsToolConfig,
    workspace: &Path,
    tools: &mut ToolRegistry,
) {
    if !skills_config.enabled {
        return;
    }
    log::debug!("Registering skill_manage tool");
    let store = SkillStore::new(workspace, None, SkillLimits::from(skills_config));
    tools.register(Box::new(SkillManageTool::new(Arc::new(store))));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gmail config pointing at `client_secret.json` / `token_cache.json` in `dir`.
    fn gmail_config_in(dir: &Path, enable: bool) -> GmailToolConfig {
        GmailToolConfig {
            enable,
            client_secret_path: dir.join("client_secret.json").to_string_lossy().to_string(),
            token_cache_path: dir.join("token_cache.json").to_string_lossy().to_string(),
            ..GmailToolConfig::default()
        }
    }

    /// Whether any of the three Gmail tools is registered.
    fn has_any_gmail_tool(tools: &ToolRegistry) -> bool {
        ["gmail", "gmail_email_send", "gmail_email_download"]
            .iter()
            .any(|name| tools.has(name))
    }

    #[test]
    fn disabled_gmail_registers_nothing_and_warns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::new();
        let warning =
            register_gmail_tools(&gmail_config_in(dir.path(), false), dir.path(), &mut tools);
        assert!(warning.is_none());
        assert!(!has_any_gmail_tool(&tools));
    }

    #[test]
    fn enabled_gmail_without_credentials_warns_instead_of_exiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::new();
        let warning =
            register_gmail_tools(&gmail_config_in(dir.path(), true), dir.path(), &mut tools)
                .expect("a warning for the missing credentials");
        assert_eq!(warning.section, GMAIL_TOOL_SECTION);
        assert!(warning.message.contains("gmail-auth"));
        assert!(!has_any_gmail_tool(&tools));
    }

    #[test]
    fn enabled_gmail_with_both_files_registers_three_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client_secret.json"), "{}").unwrap();
        std::fs::write(dir.path().join("token_cache.json"), "[]").unwrap();
        let mut tools = ToolRegistry::new();
        let warning =
            register_gmail_tools(&gmail_config_in(dir.path(), true), dir.path(), &mut tools);
        assert!(warning.is_none());
        for name in ["gmail", "gmail_email_send", "gmail_email_download"] {
            assert!(tools.has(name), "{name} should be registered");
        }
    }

    #[test]
    fn question_tool_waits_as_long_as_the_config_says() {
        let config = QuestionToolConfig {
            timeout_seconds: 1800,
            ..QuestionToolConfig::default()
        };
        let tool = question_tool_from_config(&config, Arc::new(MessageBus::new()));
        assert_eq!(tool.timeout(), Duration::from_secs(1800));
    }

    #[test]
    fn skill_manage_is_not_registered_while_skills_are_disabled() {
        let mut tools = ToolRegistry::new();
        register_skill_manage_tool(
            &SkillsToolConfig::default(),
            Path::new("workspace"),
            &mut tools,
        );
        assert!(!tools.has("skill_manage"));
    }

    #[test]
    fn skill_manage_is_registered_when_skills_are_enabled() {
        let mut tools = ToolRegistry::new();
        let config = SkillsToolConfig {
            enabled: true,
            ..SkillsToolConfig::default()
        };
        register_skill_manage_tool(&config, Path::new("workspace"), &mut tools);
        assert!(tools.has("skill_manage"));
    }

    #[test]
    fn question_tool_default_config_keeps_the_ten_minute_wait() {
        let tool =
            question_tool_from_config(&QuestionToolConfig::default(), Arc::new(MessageBus::new()));
        assert_eq!(tool.timeout(), Duration::from_secs(600));
    }

    #[test]
    fn filesystem_tool_scope_restricted_includes_builtin_skills() {
        let workspace = PathBuf::from("/workspace");
        let (allowed_dir, extra_read) = filesystem_tool_scope(&workspace, true, "");
        assert_eq!(allowed_dir, Some(workspace));
        assert_eq!(extra_read.len(), 1);
    }

    #[test]
    fn filesystem_tool_scope_unrestricted_has_no_extra_read() {
        let workspace = PathBuf::from("/workspace");
        let (allowed_dir, extra_read) = filesystem_tool_scope(&workspace, false, "");
        assert_eq!(allowed_dir, None);
        assert!(extra_read.is_empty());
    }

    #[test]
    fn filesystem_tool_scope_exec_sandbox_restricts_like_workspace() {
        let workspace = PathBuf::from("/workspace");
        let (allowed_dir, extra_read) = filesystem_tool_scope(&workspace, false, "docker");
        assert_eq!(allowed_dir, Some(workspace));
        assert_eq!(extra_read.len(), 1);
    }
}
