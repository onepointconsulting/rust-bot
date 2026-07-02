use std::path::PathBuf;

use crate::{
    agent::{
        skills::BUILTIN_SKILLS_DIR, tools::{
            base::Tool, filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool}, gmail::{GmailEmailDownloadTool, GmailEmailSendTool, GmailEmailsTool}, ocr::OcrTool, registry::ToolRegistry, search::{GlobTool, GrepTool}, web::{WebFetchTool, WebSearchTool},
        },
    }, config::schema::{GmailToolConfig, OcrToolConfig, WebToolsConfig},
};

/// Workspace restriction and builtin-skills read path used by filesystem tools.
pub fn filesystem_tool_scope(
    workspace: &PathBuf,
    restrict_to_workspace: bool,
    exec_sandbox: &str,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let allowed_dir = if restrict_to_workspace || !exec_sandbox.is_empty() {
        Some(workspace.clone())
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
    workspace: &PathBuf,
    allowed_dir: Option<PathBuf>,
    extra_read: Vec<PathBuf>,
) {
    let workspace = Some(workspace.clone());
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
        tools.register(Box::new(WebSearchTool::new(
            Some(web_config.search.clone()),
            web_config.proxy.clone(),
        )));
        tools.register(Box::new(WebFetchTool::new(None, web_config.proxy.clone())));
    }
}

pub fn register_gmail_tools(
    gmail_config: &GmailToolConfig,
    workspace: &PathBuf,
    tools: &mut ToolRegistry,
) {
    log::info!("Gmail config: {:?}", gmail_config);
    if gmail_config.enable {
        log::debug!("Registering gmail tool");
        tools.register(Box::new(GmailEmailsTool::new(gmail_config.clone())));
        tools.register(Box::new(GmailEmailSendTool::new(gmail_config.clone())));
        tools.register(Box::new(GmailEmailDownloadTool::new(
            gmail_config.clone(),
            workspace.clone(),
        )));
    }
}

pub fn register_ocr_tools(
    ocr_config: &OcrToolConfig,
    workspace: &PathBuf,
    allowed_dir: Option<PathBuf>,
    extra_read: Vec<PathBuf>,
    tools: &mut ToolRegistry,
) {
    if !ocr_config.enable {
        return;
    }
    log::debug!("Registering OCR tool");
    match OcrTool::new(
        ocr_config.clone(),
        workspace.clone(),
        allowed_dir,
        extra_read,
    ) {
        Ok(tool) => tools.register(Box::new(tool)),
        Err(e) => log::error!("Failed to register OCR tool: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_tool_scope_restricted_includes_builtin_skills() {
        let workspace = PathBuf::from("/workspace");
        let (allowed_dir, extra_read) =
            filesystem_tool_scope(&workspace, true, "");
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
