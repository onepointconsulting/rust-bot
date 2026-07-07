use regex::Regex;
use std::sync::LazyLock;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::utils::helpers::strip_surrounding_quotes;

// Default builtin skills directory (relative to this file)
pub static BUILTIN_SKILLS_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("skills")
});

static STRIP_SKILL_FRONTMATTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^---\s*\r?\n(.*?)\r?\n---\s*\r?\n").expect("STRIP_SKILL_FRONTMATTER")
});

/// Returns whether a JSON value is truthy (mirrors Python's `if value:` on dict values).
///
/// For strings, treats YAML/Python falsy literals (`"false"`, `"no"`, `"0"`, `""`) as falsy
/// so that frontmatter values like `always: false` behave the same as Python's YAML parser.
fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        serde_json::Value::String(s) => {
            !matches!(s.to_ascii_lowercase().as_str(), "" | "false" | "no" | "0")
        }
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Loader for agent skills.
/// Skills are markdown files (SKILL.md) that teach the agent how to use
/// specific tools or perform certain tasks.
pub struct SkillsLoader {
    workspace: PathBuf,
    workspace_skills: PathBuf,
    builtin_skills: PathBuf,
}

impl SkillsLoader {
    pub fn new(workspace: &PathBuf, builtin_skills_dir: Option<PathBuf>) -> Self {
        Self {
            workspace: workspace.clone(),
            workspace_skills: workspace.join("skills"),
            builtin_skills: builtin_skills_dir.unwrap_or(BUILTIN_SKILLS_DIR.clone()),
        }
    }

    fn skill_entries_from_dir(
        &self,
        base: &PathBuf,
        source: &str,
        skip_names_option: Option<&HashSet<String>>,
    ) -> Vec<serde_json::Value> {
        if !base.exists() {
            return vec![];
        }
        let mut entries = vec![];
        let read_dir = match base.read_dir() {
            Ok(rd) => rd,
            Err(e) => {
                log::error!("Failed to open skill directory {}: {}", base.display(), e);
                return vec![];
            }
        };
        for skill_result in read_dir {
            if let Err(e) = skill_result {
                log::error!("Failed to read skill directory: {}", e);
                continue;
            }
            let file = skill_result.unwrap().path();
            if !file.is_dir() {
                continue;
            }
            let skill_file = file.join("SKILL.md");
            if !skill_file.exists() {
                continue;
            }
            let file_name_result = file.file_name();
            if let None = file_name_result {
                log::error!("Failed to get file name.");
                continue;
            }
            let file_name = file_name_result.unwrap().to_string_lossy().into_owned();
            if let Some(skip_names) = skip_names_option
                && skip_names.contains(&file_name)
            {
                continue;
            }
            entries.push(serde_json::json!({
                "name": file_name,
                "path": skill_file.to_string_lossy().into_owned(),
                "source": source,
            }));
        }
        return entries;
    }

    /// List all available skills.
    ///
    /// Collects entries from `workspace_skills` and, when the builtin skills path exists,
    /// from `builtin_skills` (skipping names that already appear in the workspace listing).
    ///
    /// When `filter_unavailable` is `true`, only skills whose [`Self::check_requirements`] pass
    /// for [`Self::get_skill_meta`] are returned.
    pub fn list_skills(&self, filter_unavailable: bool) -> Vec<serde_json::Value> {
        let mut skills = self.skill_entries_from_dir(&self.workspace_skills, "workspace", None);
        let workspace_names: HashSet<String> = skills
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(str::to_owned))
            .collect();
        if self.builtin_skills.exists() {
            skills.extend(self.skill_entries_from_dir(
                &self.builtin_skills,
                "builtin",
                Some(&workspace_names),
            ));
        }
        if filter_unavailable {
            skills
                .into_iter()
                .filter(|skill| {
                    let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let meta = self.get_skill_meta(name);
                    self.check_requirements(&meta)
                })
                .collect()
        } else {
            skills
        }
    }

    /// Load a skill's content by name.
    ///
    /// Searches `workspace_skills` first, then `builtin_skills`. Returns the
    /// UTF-8 text of the first `<name>/SKILL.md` found, or `None` if absent.
    pub fn load_skill(&self, name: &str) -> Option<String> {
        let roots = [&self.workspace_skills, &self.builtin_skills];
        for root in roots {
            let path = root.join(name).join("SKILL.md");
            if path.exists() {
                return std::fs::read_to_string(&path).ok();
            }
        }
        None
    }

    /// Load specific skills for inclusion in agent context.
    ///
    /// For each name in `skill_names` (in order), loads the skill when present and appends a
    /// `### Skill: …` block with YAML frontmatter removed. Names without a resolvable `SKILL.md`
    /// are skipped. Blocks are separated by `\n\n---\n\n`.
    pub fn load_skills_for_context<S: AsRef<str>>(&self, skill_names: &[S]) -> String {
        skill_names
            .iter()
            .filter_map(|name| {
                let name = name.as_ref();
                self.load_skill(name).map(|markdown| {
                    format!("### Skill: {name}\n\n{}", self.strip_frontmatter(&markdown))
                })
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }

    /// Get skills marked as `always=true` that also meet their requirements.
    ///
    /// An "always" skill is one where either:
    /// - the parsed rust-bot/openclaw JSON metadata (in the `metadata` frontmatter field)
    ///   contains a truthy `always` value, or
    /// - the raw frontmatter has a truthy top-level `always` key.
    pub fn get_always_skills(&self) -> Vec<String> {
        self.list_skills(true)
            .into_iter()
            .filter(|entry| {
                let name = entry["name"].as_str().unwrap_or("");
                let meta = self.get_skill_metadata(name).unwrap_or(serde_json::json!({}));

                // Check parsed metadata JSON for "always"
                let meta_str = meta.get("metadata").and_then(|v| v.as_str()).unwrap_or("");
                let always_in_parsed_meta = self
                    .parse_rustbot_metadata(meta_str)
                    .get("always")
                    .map(is_truthy)
                    .unwrap_or(false);

                // Fall back to raw frontmatter "always" key
                let always_in_frontmatter = meta.get("always").map(is_truthy).unwrap_or(false);

                always_in_parsed_meta || always_in_frontmatter
            })
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect()
    }

    /// Build a summary of all skills (name, description, path, availability).
    ///
    /// Used for progressive loading: the agent reads the full skill content
    /// via `read_file` when needed. Returns an XML-formatted string, or an
    /// empty string when no skills are found.
    pub fn build_skills_summary(&self) -> String {
        let all_skills = self.list_skills(false);
        if all_skills.is_empty() {
            return String::new();
        }

        let mut lines: Vec<String> = vec!["<skills>".to_owned()];
        for entry in &all_skills {
            let skill_name = entry["name"].as_str().unwrap_or("");
            let meta = self.get_skill_meta(skill_name);
            let available = self.check_requirements(&meta);
            lines.push(format!(
                r#"  <skill available="{}">"#,
                if available { "true" } else { "false" }
            ));
            lines.push(format!("    <name>{}</name>", escape_xml(skill_name)));
            lines.push(format!(
                "    <description>{}</description>",
                escape_xml(&self.get_skill_description(skill_name))
            ));
            lines.push(format!(
                "    <location>{}</location>",
                entry["path"].as_str().unwrap_or("")
            ));
            if !available {
                let missing = self.get_missing_requirements(&meta);
                if !missing.is_empty() {
                    lines.push(format!("    <requires>{}</requires>", escape_xml(&missing)));
                }
            }
            lines.push("  </skill>".to_owned());
        }
        lines.push("</skills>".to_owned());
        lines.join("\n")
    }

    



    /// Get the description of a skill from its frontmatter.
    fn get_skill_description(&self, name: &str) -> String {
        let Some(meta) = self.get_skill_metadata(name) else {
            return name.to_string();
        };
        match meta.get("description").and_then(|v| v.as_str()) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => name.to_string(),
        }
    }

    /// Remove YAML frontmatter from markdown content.
    fn strip_frontmatter(&self, content: &str) -> String {
        if !content.starts_with("---") {
            return content.to_string();
        }
        let matched_option = STRIP_SKILL_FRONTMATTER.captures(content);
        match matched_option {
            Some(captures) => {
                return content[captures.get(0).unwrap().end()..].trim().to_string();
            }
            None => return content.to_string(),
        }
    }

    /// Parse skill metadata JSON from frontmatter (supports nanobot and openclaw keys).
    fn parse_rustbot_metadata(&self, raw: &str) -> serde_json::Value {
        let parsed: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("Failed to parse skill metadata: {}", e);
                return serde_json::json!({});
            }
        };
        if let Some(obj) = parsed.as_object() {
            let keys = vec!["rust-bot", "nanobot", "openclaw"];
            for key in keys {
                let payload_option = obj.get(key);
                if let Some(payload) = payload_option {
                    if payload.is_object() {
                        return payload.clone();
                    } else {
                        return serde_json::json!({});
                    }
                }
            }
        }
        return serde_json::json!({});
    }

        /// Returns whether skill requirements are met (executables on `PATH`, non-empty env vars).
    ///
    /// Expects `skill_meta["requires"]` shaped like `{ "bins": [...], "env": [...] }`, matching
    /// Python's `skill_meta.get("requires", {})` with list defaults for `bins` and `env`.
    fn check_requirements(&self, skill_meta: &serde_json::Value) -> bool {
        let requires_obj = skill_meta
            .get("requires")
            .filter(|v| !v.is_null())
            .and_then(|v| v.as_object());

        let (required_bins, required_env_vars): (Vec<&str>, Vec<&str>) = match requires_obj {
            None => (Vec::new(), Vec::new()),
            Some(obj) => {
                let bins = obj
                    .get("bins")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                let env_names = obj
                    .get("env")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                (bins, env_names)
            }
        };

        let bins_ok = required_bins.iter().all(|cmd| which::which(cmd).is_ok());

        let env_ok = required_env_vars
            .iter()
            .all(|var| std::env::var(var).map(|s| !s.is_empty()).unwrap_or(false));

        bins_ok && env_ok
    }

    
    /// Get rust-bot metadata for a skill (cached in frontmatter).
    fn get_skill_meta(&self, name: &str) -> serde_json::Value {
        let meta = self
            .get_skill_metadata(name)
            .unwrap_or(serde_json::json!({}));
        let empty_metadata = &serde_json::json!("");
        let metadata = meta.get("metadata").unwrap_or(empty_metadata);
        self.parse_rustbot_metadata(metadata.as_str().unwrap_or(""))
    }

    /// Get a description of missing requirements.
    fn get_missing_requirements(&self, skill_meta: &serde_json::Value) -> String {
        let default_requires = &serde_json::json!({});
        let default_list = &serde_json::json!([]);
        let requires = skill_meta.get("requires").unwrap_or(default_requires);
        let required_bins = requires.get("bins").unwrap_or(default_list);
        let required_env_vars = requires.get("env").unwrap_or(default_list);

        let mut missing: Vec<String> = Vec::new();

        // Check for missing CLI binaries
        if let Some(bins) = required_bins.as_array() {
            for bin in bins {
                if let Some(command_name) = bin.as_str() {
                    if which::which(command_name).is_err() {
                        missing.push(format!("CLI: {}", command_name));
                    }
                }
            }
        }

        // Check for missing environment variables (unset or empty, like Python truthiness)
        if let Some(env_vars) = required_env_vars.as_array() {
            for env_var in env_vars {
                if let Some(env_name) = env_var.as_str() {
                    if std::env::var(env_name)
                        .map(|v| v.is_empty())
                        .unwrap_or(true)
                    {
                        missing.push(format!("ENV: {}", env_name));
                    }
                }
            }
        }

        missing.join(", ")
    }

    /// Get metadata from a skill's frontmatter.
    ///
    /// # Arguments
    /// * `name` - Skill name.
    ///
    /// # Returns
    /// Metadata object or `None`.
    fn get_skill_metadata(&self, name: &str) -> Option<serde_json::Value> {
        let content = self.load_skill(name);
        if let Some(content) = content {
            if !content.starts_with("---") {
                return None;
            }
            let matched_option = STRIP_SKILL_FRONTMATTER.captures(content.as_str());
            match matched_option {
                Some(captures) => {
                    let group1 = captures.get(1).map(|m| m.as_str());
                    let mut metadata: HashMap<String, String> = HashMap::new();
                    if let Some(group1) = group1 {
                        for line in group1.lines() {
                            if !line.contains(":") {
                                continue;
                            }
                            let (key, value) = line.split_once(':').unwrap();
                            metadata.insert(
                                key.trim().to_string(),
                                strip_surrounding_quotes(value),
                            );
                        }
                    }
                    return Some(serde_json::json!(metadata));
                }
                None => return None,
            }
        } else {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn loader() -> SkillsLoader {
        SkillsLoader::new(&PathBuf::from("test-workspace"), None)
    }

    #[test]
    fn skill_entries_missing_base_is_empty() {
        let base = PathBuf::from("nonexistent_skills_dir_xyz");
        assert!(!base.exists());
        let entries = loader().skill_entries_from_dir(&base, "src", None);
        assert!(entries.is_empty());
    }

    #[test]
    fn skill_entries_empty_directory_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = loader().skill_entries_from_dir(&dir.path().to_path_buf(), "ws", None);
        assert!(entries.is_empty());
    }

    #[test]
    fn skill_entries_collects_subdirs_with_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        fs::create_dir(&alpha).unwrap();
        fs::create_dir(&beta).unwrap();
        fs::write(alpha.join("SKILL.md"), b"# Alpha").unwrap();
        fs::write(beta.join("SKILL.md"), b"# Beta").unwrap();

        let mut entries =
            loader().skill_entries_from_dir(&dir.path().to_path_buf(), "builtin", None);
        entries.sort_by(|a, b| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "alpha");
        assert_eq!(entries[0]["source"], "builtin");
        assert_eq!(
            PathBuf::from(entries[0]["path"].as_str().unwrap()),
            alpha.join("SKILL.md")
        );
        assert_eq!(entries[1]["name"], "beta");
        assert_eq!(
            PathBuf::from(entries[1]["path"].as_str().unwrap()),
            beta.join("SKILL.md")
        );
    }

    #[test]
    fn skill_entries_skips_subdirectory_without_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let no_md = dir.path().join("no-md");
        let ok = dir.path().join("ok");
        fs::create_dir(&no_md).unwrap();
        fs::create_dir(&ok).unwrap();
        fs::write(ok.join("SKILL.md"), b"#").unwrap();

        let entries = loader().skill_entries_from_dir(&dir.path().to_path_buf(), "ws", None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "ok");
    }

    #[test]
    fn skill_entries_skip_names_filters_matching_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["keep", "drop"] {
            let p = dir.path().join(name);
            fs::create_dir(&p).unwrap();
            fs::write(p.join("SKILL.md"), b"#").unwrap();
        }
        let mut skip = HashSet::new();
        skip.insert("drop".to_string());

        let entries = loader().skill_entries_from_dir(&dir.path().to_path_buf(), "ws", Some(&skip));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "keep");
    }

    #[test]
    fn skill_entries_requires_skill_md_at_subdirectory_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path().join("outer");
        fs::create_dir(&outer).unwrap();
        fs::create_dir(outer.join("inner")).unwrap();
        fs::write(outer.join("inner").join("SKILL.md"), b"#").unwrap();

        let entries = loader().skill_entries_from_dir(&dir.path().to_path_buf(), "ws", None);
        assert!(entries.is_empty());
    }

    #[test]
    fn load_skill_returns_none_when_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(dir.path().to_path_buf()));
        assert!(loader.load_skill("nonexistent").is_none());
    }

    #[test]
    fn load_skill_finds_skill_in_workspace_skills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), b"workspace content").unwrap();

        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(
            loader.load_skill("my-skill").as_deref(),
            Some("workspace content")
        );
    }

    #[test]
    fn load_skill_falls_back_to_builtin_skills() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let builtins = tempfile::tempdir().expect("builtins tempdir");
        let skill_dir = builtins.path().join("core-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), b"builtin content").unwrap();

        let loader = SkillsLoader::new(
            &workspace.path().to_path_buf(),
            Some(builtins.path().to_path_buf()),
        );
        assert_eq!(
            loader.load_skill("core-skill").as_deref(),
            Some("builtin content")
        );
    }

    #[test]
    fn load_skill_workspace_takes_priority_over_builtin() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let builtins = tempfile::tempdir().expect("builtins tempdir");

        for (root, content) in [
            (workspace.path().join("skills"), "workspace version"),
            (builtins.path().to_path_buf(), "builtin version"),
        ] {
            let skill_dir = root.join("shared-skill");
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(skill_dir.join("SKILL.md"), content.as_bytes()).unwrap();
        }

        let loader = SkillsLoader::new(
            &workspace.path().to_path_buf(),
            Some(builtins.path().to_path_buf()),
        );
        assert_eq!(
            loader.load_skill("shared-skill").as_deref(),
            Some("workspace version")
        );
    }

    #[test]
    fn load_skill_returns_none_when_skill_md_missing_from_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Directory exists but has no SKILL.md inside it.
        fs::create_dir_all(dir.path().join("skills").join("incomplete-skill")).unwrap();

        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert!(loader.load_skill("incomplete-skill").is_none());
    }

    // ── list_skills ───────────────────────────────────────────────────────────

    /// Skill directory under `base` (not under `base/skills`), used for builtin roots.
    fn write_skill_under_base(base: &std::path::Path, name: &str, content: &str) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn list_skills_empty_workspace_and_builtin_dirs() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let builtins = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(
            &workspace.path().to_path_buf(),
            Some(builtins.path().to_path_buf()),
        );
        assert!(loader.list_skills(false).is_empty());
        assert!(loader.list_skills(true).is_empty());
    }

    #[test]
    fn list_skills_collects_workspace_entries_with_source() {
        let workspace = tempfile::tempdir().expect("tempdir");
        write_skill(workspace.path(), "gamma", "---\nt: 1\n---\n");
        write_skill(workspace.path(), "delta", "---\nt: 2\n---\n");
        let missing = workspace.path().join("_no_builtin_dir_");
        let loader = SkillsLoader::new(&workspace.path().to_path_buf(), Some(missing));

        let mut list = loader.list_skills(false);
        list.sort_by(|a, b| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "delta");
        assert_eq!(list[0]["source"], "workspace");
        assert_eq!(list[1]["name"], "gamma");
        assert_eq!(list[1]["source"], "workspace");
    }

    #[test]
    fn list_skills_merges_builtin_skipping_names_present_in_workspace() {
        let workspace = tempfile::tempdir().expect("ws");
        let builtins = tempfile::tempdir().expect("bi");
        write_skill(workspace.path(), "overlap", "# ws");
        write_skill(workspace.path(), "ws-only", "# ws2");
        write_skill_under_base(builtins.path(), "overlap", "# bi overlap — skipped");
        write_skill_under_base(builtins.path(), "bi-only", "# bi");

        let loader = SkillsLoader::new(
            &workspace.path().to_path_buf(),
            Some(builtins.path().to_path_buf()),
        );
        let mut list = loader.list_skills(false);
        list.sort_by(|a, b| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()));
        assert_eq!(list.len(), 3);

        let overlap: Vec<_> = list.iter().filter(|e| e["name"] == "overlap").collect();
        assert_eq!(overlap.len(), 1);
        assert_eq!(overlap[0]["source"], "workspace");

        let bi = list.iter().find(|e| e["name"] == "bi-only").unwrap();
        assert_eq!(bi["source"], "builtin");
    }

    #[test]
    fn list_skills_filter_unavailable_excludes_unmet_requirements() {
        let workspace = tempfile::tempdir().expect("ws");
        let missing = workspace.path().join("_no_builtin_");
        let bad_json = r#"{"rust-bot": {"requires": {"bins": ["__nonexistent_bin_list_skills_xyz__"], "env": []}}}"#;
        let bad_front = format!("---\nmetadata: '{}'\n---\n# x", bad_json);
        write_skill(workspace.path(), "bad-req", &bad_front);
        write_skill(workspace.path(), "plain", "# no frontmatter");

        let loader = SkillsLoader::new(&workspace.path().to_path_buf(), Some(missing));
        let filtered = loader.list_skills(true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["name"], "plain");

        let unfiltered = loader.list_skills(false);
        assert_eq!(unfiltered.len(), 2);
    }

    #[test]
    fn list_skills_filter_unavailable_keeps_skill_with_empty_requires_in_metadata() {
        let workspace = tempfile::tempdir().expect("ws");
        let missing = workspace.path().join("_no_builtin_");
        let ok_json = r#"{"rust-bot": {"requires": {"bins": [], "env": []}}}"#;
        let body = format!("---\nmetadata: '{}'\n---\n#", ok_json);
        write_skill(workspace.path(), "ok-meta", &body);

        let loader = SkillsLoader::new(&workspace.path().to_path_buf(), Some(missing));
        let filtered = loader.list_skills(true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["name"], "ok-meta");
    }

    #[test]
    fn check_requirements_no_requires_key_passes() {
        let meta = serde_json::json!({ "name": "my-skill" });
        assert!(loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_null_requires_passes() {
        let meta = serde_json::json!({ "requires": null });
        assert!(loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_empty_lists_pass() {
        let meta = serde_json::json!({ "requires": { "bins": [], "env": [] } });
        assert!(loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_missing_bin_fails() {
        let meta = serde_json::json!({
            "requires": { "bins": ["__nonexistent_bin_xyz__"], "env": [] }
        });
        assert!(!loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_present_bin_passes() {
        // Pick an executable guaranteed to exist on both Windows and Unix.
        #[cfg(windows)]
        let bin = "cmd";
        #[cfg(not(windows))]
        let bin = "sh";

        let meta = serde_json::json!({ "requires": { "bins": [bin], "env": [] } });
        assert!(loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_unset_env_var_fails() {
        let var = "__SKILL_TEST_UNSET_VAR_XYZ__";
        // SAFETY: single-threaded test binary; no other thread reads this var.
        unsafe { std::env::remove_var(var) };
        let meta = serde_json::json!({ "requires": { "bins": [], "env": [var] } });
        assert!(!loader().check_requirements(&meta));
    }

    #[test]
    fn check_requirements_set_env_var_passes() {
        let var = "__SKILL_TEST_SET_VAR_XYZ__";
        // SAFETY: single-threaded test binary; no other thread reads this var.
        unsafe { std::env::set_var(var, "1") };
        let result = loader()
            .check_requirements(&serde_json::json!({ "requires": { "bins": [], "env": [var] } }));
        unsafe { std::env::remove_var(var) };
        assert!(result);
    }

    #[test]
    fn check_requirements_empty_env_var_fails() {
        let var = "__SKILL_TEST_EMPTY_VAR_XYZ__";
        // SAFETY: single-threaded test binary; no other thread reads this var.
        unsafe { std::env::set_var(var, "") };
        let result = loader()
            .check_requirements(&serde_json::json!({ "requires": { "bins": [], "env": [var] } }));
        unsafe { std::env::remove_var(var) };
        assert!(!result);
    }

    // ── get_skill_metadata ────────────────────────────────────────────────────

    fn write_skill(root: &std::path::Path, name: &str, content: &str) {
        let dir = root.join("skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn get_skill_metadata_returns_none_when_skill_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert!(loader.get_skill_metadata("nonexistent").is_none());
    }

    #[test]
    fn get_skill_metadata_returns_none_when_no_frontmatter_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "plain", "# Just markdown\nno frontmatter here");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert!(loader.get_skill_metadata("plain").is_none());
    }

    #[test]
    fn get_skill_metadata_returns_none_when_frontmatter_unclosed() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "unclosed", "---\nkey: value\n# no closing ---");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert!(loader.get_skill_metadata("unclosed").is_none());
    }

    #[test]
    fn get_skill_metadata_parses_simple_key_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "simple", "---\ntitle: My Skill\n---\n# Body");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let meta = loader.get_skill_metadata("simple").expect("Some");
        assert_eq!(meta["title"], "My Skill");
    }

    #[test]
    fn get_skill_metadata_strips_surrounding_quotes_from_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "quoted",
            "---\ntitle: \"Quoted Title\"\nauthor: 'Alice'\n---\n",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let meta = loader.get_skill_metadata("quoted").expect("Some");
        assert_eq!(meta["title"], "Quoted Title");
        assert_eq!(meta["author"], "Alice");
    }

    #[test]
    fn get_skill_metadata_skips_lines_without_colon() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "nocokon",
            "---\ntitle: Real\njust a line\n---\n",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let meta = loader.get_skill_metadata("nocokon").expect("Some");
        assert_eq!(meta["title"], "Real");
        assert!(!meta.as_object().unwrap().contains_key("just a line"));
    }

    #[test]
    fn get_skill_metadata_preserves_colons_in_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "url-skill",
            "---\nurl: https://example.com\n---\n",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let meta = loader.get_skill_metadata("url-skill").expect("Some");
        assert_eq!(meta["url"], "https://example.com");
    }

    #[test]
    fn get_skill_metadata_parses_multiple_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "multi",
            "---\ntitle: A\nauthor: B\nversion: 1\n---\n# Body",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let meta = loader.get_skill_metadata("multi").expect("Some");
        assert_eq!(meta["title"], "A");
        assert_eq!(meta["author"], "B");
        assert_eq!(meta["version"], "1");
    }

    // ── get_skill_meta ────────────────────────────────────────────────────────

    #[test]
    fn get_skill_meta_returns_empty_when_skill_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_meta("nonexistent"), serde_json::json!({}));
    }

    #[test]
    fn get_skill_meta_returns_empty_when_no_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "plain", "# Just markdown");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_meta("plain"), serde_json::json!({}));
    }

    #[test]
    fn get_skill_meta_returns_empty_when_no_metadata_key_in_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "no-meta", "---\ntitle: My Skill\n---\n# Body");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        // "metadata" key absent → parse_rustbot_metadata("") → {}
        assert_eq!(loader.get_skill_meta("no-meta"), serde_json::json!({}));
    }

    #[test]
    fn get_skill_meta_parses_rust_bot_key_from_metadata_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = r#"{"rust-bot": {"requires": {"bins": ["git"]}}}"#;
        let content = format!("---\nmetadata: '{}'\n---\n# Body", json);
        write_skill(dir.path(), "with-meta", &content);
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(
            loader.get_skill_meta("with-meta"),
            serde_json::json!({"requires": {"bins": ["git"]}})
        );
    }

    #[test]
    fn get_skill_meta_returns_empty_when_metadata_is_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "bad-json",
            "---\nmetadata: not-json\n---\n# Body",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_meta("bad-json"), serde_json::json!({}));
    }

    #[test]
    fn get_skill_meta_returns_empty_when_metadata_json_has_no_known_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = r#"{"other": {"x": 1}}"#;
        let content = format!("---\nmetadata: '{}'\n---\n# Body", json);
        write_skill(dir.path(), "unknown-key", &content);
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_meta("unknown-key"), serde_json::json!({}));
    }

    #[test]
    fn parse_rustbot_metadata_invalid_json_returns_empty() {
        assert_eq!(
            loader().parse_rustbot_metadata("not json"),
            serde_json::json!({})
        );
    }

    #[test]
    fn parse_rustbot_metadata_non_object_root_returns_empty() {
        assert_eq!(
            loader().parse_rustbot_metadata("[1, 2, 3]"),
            serde_json::json!({})
        );
    }

    #[test]
    fn parse_rustbot_metadata_no_known_key_returns_empty() {
        let raw = r#"{"other": {"x": 1}}"#;
        assert_eq!(loader().parse_rustbot_metadata(raw), serde_json::json!({}));
    }

    #[test]
    fn parse_rustbot_metadata_rust_bot_key_returned() {
        let raw = r#"{"rust-bot": {"requires": {"bins": ["git"]}}}"#;
        assert_eq!(
            loader().parse_rustbot_metadata(raw),
            serde_json::json!({"requires": {"bins": ["git"]}})
        );
    }

    #[test]
    fn parse_rustbot_metadata_nanobot_key_returned() {
        let raw = r#"{"nanobot": {"requires": {"env": ["TOKEN"]}}}"#;
        assert_eq!(
            loader().parse_rustbot_metadata(raw),
            serde_json::json!({"requires": {"env": ["TOKEN"]}})
        );
    }

    #[test]
    fn parse_rustbot_metadata_openclaw_key_returned() {
        let raw = r#"{"openclaw": {"requires": {}}}"#;
        assert_eq!(
            loader().parse_rustbot_metadata(raw),
            serde_json::json!({"requires": {}})
        );
    }

    #[test]
    fn parse_rustbot_metadata_rust_bot_takes_priority_over_nanobot() {
        let raw = r#"{"rust-bot": {"source": "rust"}, "nanobot": {"source": "nano"}}"#;
        assert_eq!(
            loader().parse_rustbot_metadata(raw),
            serde_json::json!({"source": "rust"})
        );
    }

    #[test]
    fn parse_rustbot_metadata_nanobot_takes_priority_over_openclaw() {
        let raw = r#"{"nanobot": {"source": "nano"}, "openclaw": {"source": "claw"}}"#;
        assert_eq!(
            loader().parse_rustbot_metadata(raw),
            serde_json::json!({"source": "nano"})
        );
    }

    #[test]
    fn parse_rustbot_metadata_non_object_value_returns_empty_without_fallthrough() {
        // "rust-bot" exists but is null — should return {} immediately,
        // NOT fall through to "openclaw".
        let raw = r#"{"rust-bot": null, "openclaw": {"source": "claw"}}"#;
        assert_eq!(loader().parse_rustbot_metadata(raw), serde_json::json!({}));
    }

    #[test]
    fn strip_frontmatter_returns_content_when_no_frontmatter() {
        let content = "# Just markdown\nno frontmatter here";
        assert_eq!(loader().strip_frontmatter(content), content);
    }

    #[test]
    fn strip_frontmatter_returns_content_when_regular_frontmatter() {
        let markdown_content = "# Python Code Quality

## Quick Reference

| Tool | Purpose | Command |
|------|---------|---------|
| ruff | Lint + format | `ruff check src && ruff format src` |
| mypy | Type check | `mypy src` |";
        let content = format!("---
name: improving-python-code-quality
description: Improves Python library code quality through ruff linting, mypy type checking, Pythonic idioms, and refactoring. Use when reviewing code for quality issues, adding type hints, configuring static analysis tools, or refactoring Python library code.
allowed-tools: Read Grep Glob Bash
metadata:
  model: sonnet
---
{}", markdown_content);
        assert_eq!(
            loader().strip_frontmatter(content.as_str()),
            markdown_content
        );
    }

    #[test]
    fn load_skills_for_context_empty_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.load_skills_for_context(&[] as &[&str]), "");
    }

    #[test]
    fn load_skills_for_context_formats_one_skill_and_strips_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "# Hello\nBody here.";
        let skill_md = format!("---\ntitle: t\n---\n{body}");
        write_skill(dir.path(), "alpha", &skill_md);

        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let out = loader.load_skills_for_context(&["alpha"]);
        assert_eq!(out, format!("### Skill: alpha\n\n{body}"));
    }

    #[test]
    fn load_skills_for_context_joins_two_with_separator() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "a", "# A");
        write_skill(dir.path(), "b", "# B");

        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let out = loader.load_skills_for_context(&["a", "b"]);
        assert_eq!(out, "### Skill: a\n\n# A\n\n---\n\n### Skill: b\n\n# B");
    }

    #[test]
    fn load_skills_for_context_skips_missing_preserves_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "only", "# X");

        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        let out = loader.load_skills_for_context(&["missing", "only", "also-missing"]);
        assert_eq!(out, "### Skill: only\n\n# X");
    }

    #[test]
    fn get_skill_description_missing_skill_returns_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_description("nope"), "nope");
    }

    #[test]
    fn get_skill_description_no_frontmatter_returns_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "plain", "# Just markdown");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_description("plain"), "plain");
    }

    #[test]
    fn get_skill_description_returns_value_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "with-desc",
            "---\ndescription: Does useful things\ntitle: t\n---\n# Body",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(
            loader.get_skill_description("with-desc"),
            "Does useful things"
        );
    }

    #[test]
    fn get_skill_description_empty_string_falls_back_to_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "empty-desc",
            "---\ndescription: \"\"\ntitle: t\n---\n# Body",
        );
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_description("empty-desc"), "empty-desc");
    }

    #[test]
    fn get_skill_description_missing_key_falls_back_to_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "no-desc", "---\ntitle: only title\n---\n# Body");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), None);
        assert_eq!(loader.get_skill_description("no-desc"), "no-desc");
    }

    #[test]
    fn get_missing_requirements_empty_returns_empty_string() {
        assert_eq!(
            loader().get_missing_requirements(&serde_json::json!({})),
            ""
        );
    }

    #[test]
    fn get_missing_requirements_reports_missing_bin() {
        let meta = serde_json::json!({
            "requires": { "bins": ["__no_such_bin_missing_req_xyz__"], "env": [] }
        });
        let s = loader().get_missing_requirements(&meta);
        assert_eq!(s, "CLI: __no_such_bin_missing_req_xyz__");
    }

    #[test]
    fn get_missing_requirements_reports_unset_env() {
        let var = "__MISSING_REQ_ENV_XYZ__";
        // SAFETY: tests run single-threaded for env var isolation.
        unsafe { std::env::remove_var(var) };
        let meta = serde_json::json!({
            "requires": { "bins": [], "env": [var] }
        });
        let s = loader().get_missing_requirements(&meta);
        assert_eq!(s, format!("ENV: {var}"));
    }

    #[test]
    fn get_missing_requirements_treats_empty_env_as_missing() {
        let var = "__EMPTY_REQ_ENV_XYZ__";
        // SAFETY: tests run single-threaded for env var isolation.
        unsafe { std::env::set_var(var, "") };
        let s = loader().get_missing_requirements(&serde_json::json!({
            "requires": { "bins": [], "env": [var] }
        }));
        unsafe { std::env::remove_var(var) };
        assert_eq!(s, format!("ENV: {var}"));
    }

    #[test]
    fn get_missing_requirements_lists_cli_then_env() {
        let var = "__ORDER_REQ_ENV_XYZ__";
        // SAFETY: tests run single-threaded for env var isolation.
        unsafe { std::env::remove_var(var) };
        let meta = serde_json::json!({
            "requires": {
                "bins": ["__no_such_bin_order_xyz__"],
                "env": [var]
            }
        });
        let s = loader().get_missing_requirements(&meta);
        let cli_pos = s.find("CLI:").expect("CLI");
        let env_pos = s.find("ENV:").expect("ENV");
        assert!(cli_pos < env_pos);
    }

    // ── build_skills_summary ─────────────────────────────────────────────────

    #[test]
    fn build_skills_summary_contains_skill_tags() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "alpha",
            "---\ndescription: Does alpha things\n---\n# Alpha",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));

        let summary = loader.build_skills_summary();
        assert!(
            summary.starts_with("<skills>"),
            "should start with <skills>"
        );
        assert!(summary.ends_with("</skills>"), "should end with </skills>");
        assert!(
            summary.contains("<skill available="),
            "should include skill element"
        );
        assert!(summary.contains("<name>alpha</name>"));
        assert!(summary.contains("<description>Does alpha things</description>"));
        assert!(summary.contains("<location>"));
    }

    #[test]
    fn build_skills_summary_available_true_when_no_requirements() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "free",
            "---\ndescription: Free skill\n---\n# Free",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));

        let summary = loader.build_skills_summary();
        assert!(summary.contains(r#"<skill available="true">"#));
    }

    #[test]
    fn build_skills_summary_available_false_and_requires_for_missing_bin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json =
            r#"{"rust-bot": {"requires": {"bins": ["__no_such_bin_summary_xyz__"], "env": []}}}"#;
        let content = format!("---\nmetadata: '{}'\n---\n# Needs it", json);
        write_skill(dir.path(), "needs-bin", &content);
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));

        let summary = loader.build_skills_summary();
        assert!(summary.contains(r#"<skill available="false">"#));
        assert!(summary.contains("<requires>"));
        assert!(summary.contains("__no_such_bin_summary_xyz__"));
    }

    #[test]
    fn build_skills_summary_escapes_xml_in_description() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "xml-skill",
            "---\ndescription: A & <B>\n---\n# XML",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));

        let summary = loader.build_skills_summary();
        assert!(summary.contains("A &amp; &lt;B&gt;"));
    }

    #[test]
    fn build_skills_summary_no_requires_tag_when_all_met() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "ok", "---\ndescription: OK\n---\n# OK");
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));

        let summary = loader.build_skills_summary();
        assert!(
            !summary.contains("<requires>"),
            "no <requires> when all met"
        );
    }

    // ── get_always_skills ────────────────────────────────────────────────

    #[test]
    fn get_always_skills_empty_when_no_skills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert!(loader.get_always_skills().is_empty());
    }

    #[test]
    fn get_always_skills_excludes_skills_without_always_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "normal", "---\ndescription: Normal\n---\n# Normal");
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert!(loader.get_always_skills().is_empty());
    }

    #[test]
    fn get_always_skills_includes_skill_with_frontmatter_always_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "eager",
            "---\ndescription: Eager\nalways: true\n---\n# Eager",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert_eq!(loader.get_always_skills(), vec!["eager"]);
    }

    #[test]
    fn get_always_skills_includes_skill_with_parsed_metadata_always() {
        let dir = tempfile::tempdir().expect("tempdir");
        // "metadata" frontmatter key holds a JSON blob with "rust-bot"."always": true
        write_skill(
            dir.path(),
            "meta-eager",
            "---\ndescription: Meta Eager\nmetadata: {\"rust-bot\":{\"always\":true}}\n---\n# Meta Eager",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert_eq!(loader.get_always_skills(), vec!["meta-eager"]);
    }

    #[test]
    fn get_always_skills_excludes_skill_with_always_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "lazy",
            "---\ndescription: Lazy\nalways: false\n---\n# Lazy",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert!(loader.get_always_skills().is_empty());
    }

    #[test]
    fn get_always_skills_excludes_skill_with_always_empty_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(
            dir.path(),
            "empty-always",
            "---\ndescription: Empty\nalways: \n---\n# Empty",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        assert!(loader.get_always_skills().is_empty());
    }

    #[test]
    fn get_always_skills_returns_only_those_with_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "normal", "---\ndescription: Normal\n---\n# Normal");
        write_skill(
            dir.path(),
            "always-one",
            "---\ndescription: Always\nalways: true\n---\n# Always",
        );
        let missing = dir.path().join("_no_builtins_");
        let loader = SkillsLoader::new(&dir.path().to_path_buf(), Some(missing));
        let result = loader.get_always_skills();
        assert_eq!(result, vec!["always-one"]);
    }
}
