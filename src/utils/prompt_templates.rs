/// Load and render agent system prompt templates (Tera/Jinja2) under templates/.
///
/// Agent prompts live in `templates/agent/` (pass names like `"agent/identity.md"`).
/// Shared snippets live under `agent/_snippets/` and are pulled in via
/// `{% include 'agent/_snippets/....md' %}` — identical to the Python/Jinja2 setup.
///
/// The templates root is resolved at runtime (see [`resolve_templates_root`]). The
/// compile-time embedded bundle (see [`crate::utils::embedded_templates`]) is always
/// loaded first so a standalone binary has every prompt, even when an on-disk
/// `templates/` directory is missing, empty, or only a subset. Files found on disk
/// overlay the bundle by the same relative name.
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
/// Returns `None` when no candidate directory exists on disk. [`build_environment`]
/// still loads the embedded bundle in that case; a disk root is only an overlay.
pub fn resolve_templates_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("RUST_BOT_TEMPLATES_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
        log::warn!(
            "RUST_BOT_TEMPLATES_DIR is set to '{}' but is not a directory; ignoring",
            path.display()
        );
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

/// Build a Tera environment from the embedded bundle, then overlay any on-disk
/// `templates/` root. Disk files with the same relative name win.
fn build_environment(root: Option<&Path>) -> Result<Tera, String> {
    let mut tera = Tera::default();
    load_embedded_templates(&mut tera)?;
    if let Some(root) = root {
        overlay_disk_templates(&mut tera, root)?;
    }
    Ok(tera)
}

fn load_embedded_templates(tera: &mut Tera) -> Result<(), String> {
    for path in embedded_templates::paths() {
        if !path.ends_with(".md") {
            continue;
        }
        let content = embedded_templates::get(&path)
            .ok_or_else(|| format!("Embedded template '{path}' is not valid UTF-8"))?;
        tera.add_raw_template(&path, &content)
            .map_err(|e| format!("Failed to load embedded template '{path}': {e}"))?;
    }
    Ok(())
}

fn overlay_disk_templates(tera: &mut Tera, root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    overlay_disk_dir(tera, root, root)
}

fn overlay_disk_dir(tera: &mut Tera, root: &Path, dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Failed to read templates from {:?}: {}", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read templates from {:?}: {}", dir, e))?;
        let path = entry.path();
        if path.is_dir() {
            overlay_disk_dir(tera, root, &path)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read template '{rel}': {e}"))?;
        tera.add_raw_template(&rel, &content)
            .map_err(|e| format!("Failed to load on-disk template '{rel}': {e}"))?;
    }
    Ok(())
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
    Ok(if strip {
        text.trim_end().to_string()
    } else {
        text
    })
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
        Ok(if strip {
            text.trim_end().to_string()
        } else {
            text
        })
    }

    // ── static / no-variable templates ───────────────────────────────────────

    #[test]
    fn test_render_static_template() {
        // dream_phase1.md has no variables — must render without error
        let result = render_template("agent/dream_phase1.md", &ctx(), false);
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
    fn test_embedded_bundle_includes_title_generation() {
        let mut ctx = Context::new();
        ctx.insert("part", "system");
        let result = render_with_root(None, "history/title_generation.md", &ctx, true);
        assert!(result.is_ok(), "unexpected error: {:?}", result.err());
        assert!(result.unwrap().contains("Return only the title text"));
    }

    #[test]
    fn test_missing_disk_root_still_serves_embedded_title_generation() {
        let missing = PathBuf::from("/this/path/does/not/exist/templates");
        let mut ctx = Context::new();
        ctx.insert("part", "system");
        let result = render_with_root(Some(&missing), "history/title_generation.md", &ctx, true);
        assert!(
            result.is_ok(),
            "missing disk root should not hide embedded templates: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_partial_disk_root_falls_back_to_embedded_title_generation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), "disk overlay").unwrap();
        let mut ctx = Context::new();
        ctx.insert("part", "system");
        let result = render_with_root(Some(tmp.path()), "history/title_generation.md", &ctx, true);
        assert!(
            result.is_ok(),
            "partial disk root should still serve embedded title template: {:?}",
            result.err()
        );
        assert!(result.unwrap().contains("Return only the title text"));
    }

    #[test]
    fn test_disk_overlay_replaces_embedded_template() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("dream_phase1.md"), "disk-override-prompt").unwrap();
        let result = render_with_root(Some(tmp.path()), "agent/dream_phase1.md", &ctx(), true);
        assert_eq!(result.unwrap(), "disk-override-prompt");
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
        assert!(
            result.contains("42"),
            "expected iteration count in output: {}",
            result
        );
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
        assert!(
            !result.contains("POSIX"),
            "should not contain POSIX section"
        );
    }

    #[test]
    fn test_platform_policy_posix() {
        let mut ctx = Context::new();
        ctx.insert("system", "Linux");
        let result = render_template("agent/platform_policy.md", &ctx, true).unwrap();
        assert!(result.contains("POSIX"), "expected POSIX section");
        assert!(
            !result.contains("Windows"),
            "should not contain Windows section"
        );
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
        assert!(
            !result.contains("## Skills"),
            "skills block should be absent"
        );
    }

    #[test]
    fn test_subagent_system_with_skills() {
        let mut ctx = Context::new();
        ctx.insert("time_ctx", "2026-04-08 10:00");
        ctx.insert("workspace", "/tmp/ws");
        ctx.insert("skills_summary", "- /skills/search/SKILL.md");
        let result = render_template("agent/subagent_system.md", &ctx, true).unwrap();
        assert!(
            result.contains("## Skills"),
            "skills block should be present"
        );
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
