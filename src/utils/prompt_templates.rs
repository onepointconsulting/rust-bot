/// Load and render agent system prompt templates (Tera/Jinja2) under templates/.
///
/// Agent prompts live in `templates/agent/` (pass names like `"agent/identity.md"`).
/// Shared snippets live under `agent/_snippets/` and are pulled in via
/// `{% include 'agent/_snippets/....md' %}` — identical to the Python/Jinja2 setup.
///
/// The templates root is resolved at runtime (see [`resolve_templates_root`]). When no
/// on-disk `templates/` directory can be found, templates are loaded from the
/// compile-time embedded bundle instead (see [`crate::utils::embedded_templates`]), so a
/// standalone binary always has prompts available.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tera::{Context, Tera};

use crate::utils::embedded_templates;

// ── templates root ────────────────────────────────────────────────────────────

/// Resolve the on-disk `templates/` directory, if one can be found.
///
/// Strategy (first match wins):
/// 1. `RUST_BOT_TEMPLATES_DIR` env var — explicit override for any layout.
/// 2. Walk up from the executable's directory (at most 8 ancestors), use the first
///    `{ancestor}/templates` that exists — finds the repo's `templates/` when the binary
///    lives under `target/debug` or `target/release`, and supports install layouts like
///    `prefix/bin/app` → `prefix/templates` if distributors nest that way.
/// 3. `{current_working_directory}/templates` — last resort when launched from the project
///    root or another directory that contains a `templates/` folder.
///
/// Returns `None` when no candidate directory exists on disk, in which case callers
/// should fall back to the embedded bundle in [`crate::utils::embedded_templates`].
pub fn resolve_templates_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("RUST_BOT_TEMPLATES_DIR") {
        return Some(PathBuf::from(dir));
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir: Option<PathBuf> = exe.parent().map(Path::to_path_buf);
        for _ in 0..8 {
            let Some(ref d) = dir else {
                break;
            };
            let candidate = d.join("templates");
            if candidate.is_dir() {
                return Some(candidate);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }

    let cwd_candidate = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("templates");
    cwd_candidate.is_dir().then_some(cwd_candidate)
}

// ── cached Tera environment ───────────────────────────────────────────────────

static TERA: OnceLock<Result<Tera, String>> = OnceLock::new();

fn environment() -> Result<&'static Tera, String> {
    TERA.get_or_init(|| build_environment(resolve_templates_root().as_deref()))
        .as_ref()
        .map_err(|e| e.clone())
}

/// Build a Tera environment from an on-disk `templates/` root, or from the embedded
/// bundle when `root` is `None`.
fn build_environment(root: Option<&Path>) -> Result<Tera, String> {
    match root {
        Some(root) => {
            let glob = format!("{}/**/*.md", root.to_string_lossy());
            Tera::new(&glob)
                .map_err(|e| format!("Failed to load templates from {:?}: {}", root, e))
        }
        None => {
            let mut tera = Tera::default();
            for path in embedded_templates::paths() {
                if !path.ends_with(".md") {
                    continue;
                }
                let content = embedded_templates::get(&path).ok_or_else(|| {
                    format!("Embedded template '{path}' is not valid UTF-8")
                })?;
                tera.add_raw_template(&path, &content)
                    .map_err(|e| format!("Failed to load embedded template '{path}': {e}"))?;
            }
            Ok(tera)
        }
    }
}

// ── public API ────────────────────────────────────────────────────────────────

/// Render a template by name (e.g. `"agent/identity.md"`) with the given context.
///
/// Pass `strip: true` to `rstrip` the result — useful when the file ends with a
/// trailing newline you don't want preserved (mirrors `strip=True` in Python).
pub fn render_template(name: &str, context: &Context, strip: bool) -> Result<String, String> {
    let tera = environment()?;
    let text = tera
        .render(name, context)
        .map_err(|e| format!("Failed to render template '{}': {}", name, e))?;
    Ok(if strip { text.trim_end().to_string() } else { text })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context::new()
    }

    /// Render directly against a chosen root, bypassing runtime discovery and the
    /// cached [`TERA`] instance — lets tests exercise the embedded-bundle branch
    /// deterministically, regardless of where the test binary happens to live on disk.
    fn render_with_root(
        root: Option<&Path>,
        name: &str,
        context: &Context,
        strip: bool,
    ) -> Result<String, String> {
        let tera = build_environment(root)?;
        let text = tera
            .render(name, context)
            .map_err(|e| format!("Failed to render template '{}': {}", name, e))?;
        Ok(if strip { text.trim_end().to_string() } else { text })
    }

    // ── static / no-variable templates ───────────────────────────────────────

    #[test]
    fn test_render_static_template() {
        // dream_phase1.md has no variables — must render without error
        let result = render_template("agent/dream_phase1.md", 
        &ctx(), false);
        assert!(result.is_ok(), "unexpected error: {:?}", result.err());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_render_strip_trims_trailing_whitespace() {
        let unstripped = render_template("agent/dream_phase1.md", &ctx(), false).unwrap();
        let stripped = render_template("agent/dream_phase1.md", &ctx(), true).unwrap();
        assert_eq!(stripped, unstripped.trim_end());
    }

    // ── embedded bundle fallback (no on-disk templates/ root) ────────────────

    #[test]
    fn test_render_from_embedded_bundle_without_disk_root() {
        let result = render_with_root(None, "agent/dream_phase1.md", &ctx(), false);
        assert!(result.is_ok(), "unexpected error: {:?}", result.err());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_embedded_bundle_includes_snippet() {
        let mut ctx = Context::new();
        ctx.insert("runtime", "test-runtime");
        ctx.insert("workspace_path", "/tmp/ws");
        ctx.insert("platform_policy", "");
        ctx.insert("channel", "cli");
        let result = render_with_root(None, "agent/identity.md", &ctx, true).unwrap();
        assert!(
            result.contains("untrusted external data"),
            "snippet not included: {}",
            result
        );
    }

    // ── variable interpolation ────────────────────────────────────────────────

    #[test]
    fn test_render_max_iterations_message() {
        let mut ctx = Context::new();
        let max_iterations: u32 = 42;
        ctx.insert("max_iterations", &max_iterations);
        let result = render_template("agent/max_iterations_message.md", &ctx, true).unwrap();
        println!("result: {}", result);
        assert!(result.contains("42"), "expected iteration count in output: {}", result);
    }

    #[test]
    fn test_render_subagent_announce() {
        let mut ctx = Context::new();
        ctx.insert("label", "worker-1");
        ctx.insert("status_text", "completed");
        ctx.insert("task", "summarise the logs");
        ctx.insert("result", "Done.");
        let result = render_template("agent/subagent_announce.md", &ctx, true).unwrap();
        assert!(result.contains("worker-1"));
        assert!(result.contains("completed"));
        assert!(result.contains("summarise the logs"));
    }

    // ── conditional blocks ────────────────────────────────────────────────────

    #[test]
    fn test_platform_policy_windows() {
        let mut ctx = Context::new();
        ctx.insert("system", "Windows");
        let result = render_template("agent/platform_policy.md", &ctx, true).unwrap();
        assert!(result.contains("Windows"), "expected Windows section");
        assert!(!result.contains("POSIX"), "should not contain POSIX section");
    }

    #[test]
    fn test_platform_policy_posix() {
        let mut ctx = Context::new();
        ctx.insert("system", "Linux");
        let result = render_template("agent/platform_policy.md", &ctx, true).unwrap();
        assert!(result.contains("POSIX"), "expected POSIX section");
        assert!(!result.contains("Windows"), "should not contain Windows section");
    }

    // ── include directive ─────────────────────────────────────────────────────

    #[test]
    fn test_identity_includes_snippet() {
        let mut ctx = Context::new();
        ctx.insert("runtime", "test-runtime");
        ctx.insert("workspace_path", "/tmp/ws");
        ctx.insert("platform_policy", "");
        ctx.insert("channel", "cli");
        let result = render_template("agent/identity.md", &ctx, true).unwrap();
        // The snippet text must be present via {% include %}
        assert!(
            result.contains("untrusted external data"),
            "snippet not included: {}",
            result
        );
    }

    // ── optional / falsy variable ─────────────────────────────────────────────

    #[test]
    fn test_subagent_system_without_skills() {
        let mut ctx = Context::new();
        ctx.insert("time_ctx", "2026-04-08 10:00");
        ctx.insert("workspace", "/tmp/ws");
        ctx.insert("skills_summary", &""); // falsy → skills block should be omitted
        let result = render_template("agent/subagent_system.md", &ctx, true).unwrap();
        assert!(!result.contains("## Skills"), "skills block should be absent");
    }

    #[test]
    fn test_subagent_system_with_skills() {
        let mut ctx = Context::new();
        ctx.insert("time_ctx", "2026-04-08 10:00");
        ctx.insert("workspace", "/tmp/ws");
        ctx.insert("skills_summary", "- /skills/search/SKILL.md");
        let result = render_template("agent/subagent_system.md", &ctx, true).unwrap();
        assert!(result.contains("## Skills"), "skills block should be present");
        assert!(result.contains("SKILL.md"));
    }

    // ── unknown template ──────────────────────────────────────────────────────

    #[test]
    fn test_render_unknown_template_returns_error() {
        let result = render_template("agent/does_not_exist.md", &ctx(), false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("does_not_exist.md"),
            "error should name the template: {}",
            err
        );
    }
}
