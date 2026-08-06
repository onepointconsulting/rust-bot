//! Task-local ambient workspace-scope stack (Python `ContextVar` for
//! `workspace_scope`). Mirrors `agent::cron_context`'s shape exactly; see
//! that module for the general pattern. Established once per turn in
//! `AgentLoop::process_message`/`process_system_message`; bound/reset once
//! per turn around the tool-execution loop in `AgentLoop::run_agent_loop`.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;

use crate::security::workspace_access::{ToolWorkspace, WorkspaceScope};

tokio::task_local! {
    static WORKSPACE_SCOPE_STACK: RefCell<Vec<WorkspaceScope>>;
}

/// Token returned by [`bind_workspace_scope`]; pass to [`reset_workspace_scope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceScopeToken;

/// Run a future with an initialized workspace-scope stack (call once per
/// turn — see `AgentLoop::process_message`/`process_system_message`).
pub async fn with_workspace_scope_stack<F, Fut>(f: F) -> Fut::Output
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    WORKSPACE_SCOPE_STACK.scope(RefCell::new(Vec::new()), f()).await
}

/// The currently bound scope, if any (`ContextVar.get()`, default `None`).
pub fn current_workspace_scope() -> Option<WorkspaceScope> {
    WORKSPACE_SCOPE_STACK
        .try_with(|stack| stack.borrow().last().cloned())
        .unwrap_or(None)
}

/// Bind `scope` as active for the rest of this task (`ContextVar.set`).
pub fn bind_workspace_scope(scope: WorkspaceScope) -> WorkspaceScopeToken {
    WORKSPACE_SCOPE_STACK.with(|stack| stack.borrow_mut().push(scope));
    WorkspaceScopeToken
}

/// Restore the previous scope (`ContextVar.reset`).
pub fn reset_workspace_scope(_token: WorkspaceScopeToken) {
    let _ = WORKSPACE_SCOPE_STACK.try_with(|stack| {
        stack.borrow_mut().pop();
    });
}

/// What a tool should actually read/write against right now: the ambient
/// scope if one is bound, else the tool's own construction-time default.
/// `sandbox_restricts_workspace` is OR'd in regardless of the ambient
/// scope's own access mode — an active exec sandbox always implies
/// containment (mirrors `registry_helper::filesystem_tool_scope`'s
/// `!exec_sandbox.is_empty()` check).
pub fn current_tool_workspace(
    default_workspace: Option<PathBuf>,
    restrict_to_workspace: bool,
    sandbox_restricts_workspace: bool,
) -> ToolWorkspace {
    if let Some(scope) = current_workspace_scope() {
        return ToolWorkspace {
            project_path: Some(scope.project_path.clone()),
            restrict_to_workspace: scope.restrict_to_workspace || sandbox_restricts_workspace,
            scope: Some(scope),
        };
    }
    ToolWorkspace {
        project_path: default_workspace,
        restrict_to_workspace: restrict_to_workspace || sandbox_restricts_workspace,
        scope: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::workspace_access::build_workspace_scope;
    use crate::security::workspace_access::WorkspaceAccessMode;

    fn scope_at(path: &std::path::Path, restricted: bool) -> WorkspaceScope {
        let mode = if restricted {
            WorkspaceAccessMode::Restricted
        } else {
            WorkspaceAccessMode::Full
        };
        build_workspace_scope(path, mode, None)
    }

    #[tokio::test]
    async fn default_is_none() {
        with_workspace_scope_stack(|| async {
            assert!(current_workspace_scope().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn bind_and_reset_restores_previous() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let scope_a = scope_at(dir_a.path(), true);
        let scope_b = scope_at(dir_b.path(), true);

        with_workspace_scope_stack(|| async {
            let t1 = bind_workspace_scope(scope_a.clone());
            assert_eq!(current_workspace_scope().unwrap().project_path, dir_a.path());

            let t2 = bind_workspace_scope(scope_b.clone());
            assert_eq!(current_workspace_scope().unwrap().project_path, dir_b.path());

            reset_workspace_scope(t2);
            assert_eq!(current_workspace_scope().unwrap().project_path, dir_a.path());

            reset_workspace_scope(t1);
            assert!(current_workspace_scope().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn nested_bind_reset_behaves_as_a_stack() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        with_workspace_scope_stack(|| async {
            let mut tokens = Vec::new();
            for dir in &dirs {
                tokens.push(bind_workspace_scope(scope_at(dir.path(), true)));
            }
            for dir in dirs.iter().rev() {
                assert_eq!(current_workspace_scope().unwrap().project_path, dir.path());
                reset_workspace_scope(tokens.pop().unwrap());
            }
            assert!(current_workspace_scope().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn current_tool_workspace_without_ambient_scope_uses_defaults() {
        with_workspace_scope_stack(|| async {
            let default_dir = PathBuf::from("/some/default");
            let tw = current_tool_workspace(Some(default_dir.clone()), true, false);
            assert_eq!(tw.project_path, Some(default_dir));
            assert!(tw.restrict_to_workspace);
            assert!(tw.scope.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn current_tool_workspace_prefers_ambient_scope_when_bound() {
        let other_default = PathBuf::from("/some/other/default");
        let dir_b = tempfile::tempdir().unwrap();
        with_workspace_scope_stack(|| async {
            let token = bind_workspace_scope(scope_at(dir_b.path(), true));
            let tw = current_tool_workspace(Some(other_default), false, false);
            assert_eq!(tw.project_path, Some(dir_b.path().to_path_buf()));
            assert!(tw.restrict_to_workspace);
            assert!(tw.scope.is_some());
            reset_workspace_scope(token);
        })
        .await;
    }

    #[tokio::test]
    async fn current_tool_workspace_ors_sandbox_restricts_workspace_even_for_full_access_scope() {
        let dir = tempfile::tempdir().unwrap();
        with_workspace_scope_stack(|| async {
            let token = bind_workspace_scope(scope_at(dir.path(), false));
            let tw = current_tool_workspace(None, false, true);
            assert!(tw.restrict_to_workspace);
            reset_workspace_scope(token);
        })
        .await;
    }
}
