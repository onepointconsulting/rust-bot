use async_trait::async_trait;
use regex::Regex;
use std::{collections::{HashMap, HashSet}, path::PathBuf};

use crate::agent::tools::{
    base::Tool,
    filesystem::{FsToolConfig, ListDirTool},
};

const DEFAULT_HEAD_LIMIT: usize = 250;

fn type_globs(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "py" | "python" => &["*.py", "*.pyi"],
        "js" => &["*.js", "*.jsx", "*.mjs", "*.cjs"],
        "ts" => &["*.ts", "*.tsx", "*.mts", "*.cts"],
        "tsx" => &["*.tsx"],
        "jsx" => &["*.jsx"],
        "json" => &["*.json"],
        "md" | "markdown" => &["*.md", "*.mdx"],
        "go" => &["*.go"],
        "rs" | "rust" => &["*.rs"],
        "java" => &["*.java"],
        "sh" => &["*.sh", "*.bash"],
        "yaml" | "yml" => &["*.yaml", "*.yml"],
        "toml" => &["*.toml"],
        "sql" => &["*.sql"],
        "html" => &["*.html", "*.htm"],
        "css" => &["*.css", "*.scss", "*.sass"],
        _ => &[],
    }
}

fn normalize_pattern(pattern: &str) -> String {
    pattern.trim().replace("\\", "/").to_string()
}

fn match_glob(rel_path: &str, name: &str, pattern: &str) -> bool {
    let normalized = normalize_pattern(pattern);
    if normalized.is_empty() {
        return false;
    }
    if normalized.contains("/") || normalized.starts_with("**") {
        // Replicate Python's PurePosixPath.match() right-anchored semantics:
        // patterns without a leading ** are matched from anywhere in the path.
        let glob_pattern = if normalized.starts_with("**") {
            normalized.clone()
        } else {
            format!("**/{}", normalized)
        };
        return glob::Pattern::new(&glob_pattern)
            .map(|p| p.matches(rel_path))
            .unwrap_or(false);
    }
    glob::Pattern::new(&normalized)
        .map(|p| p.matches(name))
        .unwrap_or(false)
}

fn is_binary(raw: &[u8]) -> bool {
    if raw.contains(&0) {
        return true;
    }
    let sample = &raw[..raw.len().min(4096)];
    if sample.is_empty() {
        return false;
    }
    let non_text = sample
        .iter()
        .map(|b| if b < &9 || (b > &13 && b < &32) { 1 } else { 0 })
        .sum::<usize>();
    non_text as f64 / sample.len() as f64 > 0.2
}

fn paginate<T: Clone>(items: Vec<T>, limit: Option<usize>, offset: usize) -> (Vec<T>, bool) {
    // Clamp start so an offset past the end returns an empty result rather than panicking.
    let start = offset.min(items.len());
    match limit {
        None => (items[start..].to_vec(), false),
        Some(limit) => {
            // Use saturating_add to guard against overflow in release builds.
            let end = start.saturating_add(limit).min(items.len());
            let sliced = items[start..end].to_vec();
            // There are more items if the unclamped window end falls short of items.len().
            let truncate = start.saturating_add(limit) < items.len();
            (sliced, truncate)
        }
    }
}

fn pagination_note(limit: Option<usize>, offset: usize, truncated: bool) -> Option<String> {
    if truncated {
        match limit {
            None => {
                return Some(format!("(pagination: offset={offset})"));
            }
            Some(limit) => {
                return Some(format!("(pagination: limit={limit} offset={offset})"));
            }
        }
    }
    if offset > 0 {
        return Some(format!("(pagination: offset={offset})"));
    }
    None
}

fn matches_type(name: &str, file_type: Option<&str>) -> bool {
    match file_type {
        None => {
            return true;
        }
        Some(file_type) => {
            let lowered = file_type.trim().to_lowercase();
            if lowered.is_empty() {
                return true;
            }
            let fallback = format!("*.{lowered}");
            let patterns: &[_] = if !type_globs(&lowered).is_empty() {
                type_globs(&lowered)
            } else {
                &[fallback.as_str()]
            };
            return patterns.iter().any(|p| {
                glob::Pattern::new(&p.to_lowercase())
                    .map(|pat| pat.matches(&name.to_lowercase()))
                    .unwrap_or(false)
            });
        }
    }
}

struct SearchTool {
    fs: FsToolConfig,
}

impl SearchTool {
    pub(crate) fn ignore_dirs() -> HashSet<&'static str> {
        ListDirTool::IGNORE_DIRS.iter().copied().collect()
    }

    fn display_path(&self, target: PathBuf, root: PathBuf) -> String {
        // Replicate Python's relative_to(root): root should always be an ancestor
        // of target in normal usage. Log a warning rather than silently returning
        // an absolute path (which would diverge from Python's ValueError).
        let fallback = match target.strip_prefix(&root) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => {
                log::warn!(
                    "display_path: target {:?} is not under root {:?}",
                    target,
                    root
                );
                target.to_string_lossy().replace('\\', "/")
            }
        };

        match self.fs.workspace.clone() {
            Some(ws) => {
                if target.starts_with(ws.clone()) {
                    target
                        .strip_prefix(ws)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                } else {
                    fallback
                }
            }
            None => fallback,
        }
    }

    /// Walk `root` and collect entries according to the inclusion flags.
    ///
    /// Mirrors Python's `_iter_entries`: ignored directories are pruned during
    /// traversal and results within each directory are alphabetically sorted,
    /// matching `os.walk` with `dirnames[:] = sorted(...)`.
    fn iter_entries(
        &self,
        root: &std::path::Path,
        include_files: bool,
        include_dirs: bool,
    ) -> Vec<PathBuf> {
        if root.is_file() {
            return if include_files {
                vec![root.to_path_buf()]
            } else {
                vec![]
            };
        }

        let ignore = Self::ignore_dirs();
        let mut results = Vec::new();

        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut subdirs: Vec<PathBuf> = Vec::new();
            let mut files: Vec<PathBuf> = Vec::new();

            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("iter_entries: cannot read {:?}: {}", dir, e);
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if path.is_dir() {
                    if !ignore.contains(name_str.as_ref()) {
                        subdirs.push(path);
                    }
                } else if include_files {
                    files.push(path);
                }
            }

            // Emit sorted dirs first (matching Python's os.walk order for include_dirs),
            // then sorted files. Push subdirs in reverse order so pop() is alphabetical.
            subdirs.sort();
            if include_dirs {
                results.extend(subdirs.iter().cloned());
            }
            files.sort();
            results.extend(files);

            subdirs.reverse();
            stack.extend(subdirs);
        }

        results
    }

    /// Iterate over all files under `root`, sorted with ignored directories pruned.
    fn iter_files(&self, root: &std::path::Path) -> Vec<PathBuf> {
        self.iter_entries(root, true, false)
    }

    pub fn new(fs: FsToolConfig) -> Self {
        Self { fs }
    }
}

pub struct GlobTool {
    search: SearchTool,
}

impl GlobTool {
    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self {
            search: SearchTool::new(FsToolConfig::new(
                workspace,
                allowed_dir,
                extra_allowed_dirs,
            )),
        }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> String {
        "glob".to_string()
    }

    fn description(&self) -> String {
        "Find files matching a glob pattern (e.g. '*.py', 'tests/**/test_*.py'). 
Results are sorted by modification time (newest first). 
Skips .git, node_modules, __pycache__, and other noise directories."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match, e.g. '*.py' or 'tests/**/test_*.py'",
                    "minLength": 1,
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search from (default '.')",
                },
                "max_results": {
                    "type": "integer",
                    "description": "Legacy alias for head_limit",
                    "minimum": 1,
                    "maximum": 1000,
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default 250)",
                    "minimum": 0,
                    "maximum": 1000,
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N matching entries before returning results",
                    "minimum": 0,
                    "maximum": 100000,
                },
                "entry_type": {
                    "type": "string",
                    "enum": ["files", "dirs", "both"],
                    "description": "Whether to match files, directories, or both (default files)",
                },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let max_results_option = params.get("max_results").and_then(|v| v.as_u64());
        let head_limit_option = params.get("head_limit").and_then(|v| v.as_u64());
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let entry_type = params
            .get("entry_type")
            .and_then(|v| v.as_str())
            .unwrap_or("files");

        let root = self.search.fs.resolve(path).unwrap_or(PathBuf::from("."));
        if !root.exists() {
            return format!("Error: Does not exist: {}", path);
        }
        if !root.is_dir() {
            return format!("Error: Not a directory: {}", path);
        }

        let limit = match head_limit_option {
            Some(head_limit) => {
                if head_limit == 0 {
                    None
                } else {
                    Some(head_limit as usize)
                }
            }
            None => {
                if let Some(max_results) = max_results_option {
                    Some(max_results as usize)
                } else {
                    Some(DEFAULT_HEAD_LIMIT)
                }
            }
        };
        let include_files = vec!["files", "both"].contains(&entry_type);
        let include_dirs = vec!["dirs", "both"].contains(&entry_type);
        let mut matches: Vec<(String, f64)> = Vec::new();
        for entry in self.search.iter_entries(&root, include_files, include_dirs) {
            let rel_path = self
                .search
                .display_path(entry.clone(), root.clone().to_path_buf());
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if match_glob(&rel_path, &name, pattern) {
                let mut display = rel_path.clone();
                if entry.is_dir() {
                    display.push('/');
                }
                let mtime = std::fs::metadata(&entry)
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64()
                    })
                    .unwrap_or(0.0);
                matches.push((display, mtime));
            }
        }

        if matches.is_empty() {
            return format!(
                "No matches found for pattern: {} in path: {}",
                pattern, path
            );
        }

        // Sort by descending mtime first, then ascending name (mirrors Python's
        // `matches.sort(key=lambda item: (-item[1], item[0]))`).
        matches.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        let ordered: Vec<String> = matches.into_iter().map(|(name, _)| name).collect();
        let (paged, truncated) = paginate(ordered, limit, offset);
        let mut result = paged.join("\n");
        if let Some(note) = pagination_note(limit, offset, truncated) {
            result.push_str(&format!("\n\n{note}"));
        }
        result
    }
}

/// Search file contents using a regex-like pattern.
pub struct GrepTool {
    search: SearchTool,
}

impl GrepTool {
    const MAX_RESULT_CHARS: usize = 128_000;
    const MAX_FILE_BYTES: usize = 2_000_000;

    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self {
            search: SearchTool::new(FsToolConfig::new(
                workspace,
                allowed_dir,
                extra_allowed_dirs,
            )),
        }
    }

    fn format_block(
        display_path: &str,
        lines: &[String],
        match_line: usize,
        before: usize,
        after: usize,
    ) -> String {
        let start = std::cmp::max(1, match_line.saturating_sub(before));
        let end = std::cmp::min(match_line.saturating_add(after), lines.len());
        let first_line = format!("{display_path}:{match_line}");
        let mut block = vec![first_line];
        for line_no in start..=end {
            let marker = if line_no == match_line { ">" } else { " " };
            block.push(format!("{marker} {line_no}| {}", lines[line_no - 1]));
        }
        block.join("\n")
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> String {
        "grep".to_string()
    }

    fn description(&self) -> String {
        "Search file contents with a regex pattern.
        Default output_mode is files_with_matches (file paths only); 
        use content mode for matching lines with context.
        Skips binary and files >2 MB. Supports glob/type filtering."
            .to_string()
    }

    fn read_only(&self) -> bool {
        true
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex or plain text pattern to search for",
                    "minLength": 1,
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default '.')",
                },
                "glob": {
                    "type": "string",
                    "description": "Optional file filter, e.g. '*.py' or 'tests/**/test_*.py'",
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type shorthand, e.g. 'py', 'ts', 'md', 'json'",
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default false)",
                },
                "fixed_strings": {
                    "type": "boolean",
                    "description": "Treat pattern as plain text instead of regex (default false)",
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": (
                        "content: matching lines with optional context; files_with_matches: only matching file paths; count: matching line counts per file. Default: files_with_matches"
                    ),
                },
                "context_before": {
                    "type": "integer",
                    "description": "Number of lines of context before each match",
                    "minimum": 0,
                    "maximum": 20,
                },
                "context_after": {
                    "type": "integer",
                    "description": "Number of lines of context after each match",
                    "minimum": 0,
                    "maximum": 20,
                },
                "max_matches": {
                    "type": "integer",
                    "description": (
                        "Legacy alias for head_limit in content mode"
                    ),
                    "minimum": 1,
                    "maximum": 1000,
                },
                "max_results": {
                    "type": "integer",
                    "description": "Legacy alias for head_limit in files_with_matches or count mode",
                    "minimum": 1,
                    "maximum": 1000,
                },
                "head_limit": {
                    "type": "integer",
                    "description":
                        "Maximum number of results to return. In content mode this limits matching line blocks; in other modes it limits file entries. Default 250"
                    ,
                    "minimum": 0,
                    "maximum": 1000,
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N results before applying head_limit",
                    "minimum": 0,
                    "maximum": 100000,
                },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        if pattern.is_empty() {
            return "Error: missing required parameter 'pattern'".to_string();
        }
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let glob = params.get("glob").and_then(|v| v.as_str()).unwrap_or("");
        let type_name = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let case_insensitive = params
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let fixed_strings = params
            .get("fixed_strings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let output_mode = params
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let context_before = params
            .get("context_before")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let context_after = params
            .get("context_after")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let max_matches = params
            .get("max_matches")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let max_results = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let head_limit = params
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let target = self.search.fs.resolve(path).unwrap_or(PathBuf::from("."));
        if !target.exists() {
            return format!("Error: Path not found: {}", path);
        }

        let needle = if fixed_strings {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let pattern_str = if case_insensitive {
            format!("(?i){needle}")
        } else {
            needle
        };
        let regex = match Regex::new(&pattern_str) {
            Ok(re) => re,
            Err(e) => return format!("Error: invalid regex pattern: {e}"),
        };

        let limit = if head_limit > 0 {
            Some(head_limit)
        } else if output_mode == "content" && max_matches > 0 {
            Some(max_matches)
        } else if output_mode != "content" && max_results > 0 {
            Some(max_results)
        } else if head_limit == 0 {
            None
        } else {
            Some(DEFAULT_HEAD_LIMIT)
        };

        let root = if target.is_dir() {
            target.clone()
        } else {
            target
                .parent()
                .unwrap_or(&target)
                .to_path_buf()
        };

        let type_filter = if type_name.is_empty() {
            None
        } else {
            Some(type_name)
        };

        let mut blocks: Vec<String> = Vec::new();
        let mut result_chars: usize = 0;
        let mut seen_content_matches: usize = 0;
        let mut truncated = false;
        let mut size_truncated = false;
        let mut skipped_binary: usize = 0;
        let mut skipped_large: usize = 0;
        let mut matching_files: Vec<String> = Vec::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut file_mtimes: HashMap<String, f64> = HashMap::new();

        'files: for file_path in self.search.iter_files(&target) {
            let rel_path = file_path
                .strip_prefix(&root)
                .unwrap_or(&file_path)
                .to_string_lossy()
                .replace('\\', "/");
            let name = file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            if !glob.is_empty() && !match_glob(&rel_path, &name, glob) {
                continue;
            }
            if !matches_type(&name, type_filter) {
                continue;
            }

            let raw = match std::fs::read(&file_path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if raw.len() > Self::MAX_FILE_BYTES {
                skipped_large += 1;
                continue;
            }
            if is_binary(&raw) {
                skipped_binary += 1;
                continue;
            }

            let mtime = std::fs::metadata(&file_path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64()
                })
                .unwrap_or(0.0);

            let content = match std::str::from_utf8(&raw) {
                Ok(text) => text,
                Err(_) => {
                    skipped_binary += 1;
                    continue;
                }
            };

            let lines: Vec<String> = content.lines().map(str::to_owned).collect();
            let display_path = self.search.display_path(file_path.clone(), root.clone());
            let mut file_had_match = false;

            for (idx, line) in lines.iter().enumerate() {
                let line_no = idx + 1;
                if !regex.is_match(line) {
                    continue;
                }
                file_had_match = true;

                if output_mode == "count" {
                    *counts.entry(display_path.clone()).or_insert(0) += 1;
                    continue;
                }
                if output_mode == "files_with_matches" {
                    if !matching_files.contains(&display_path) {
                        matching_files.push(display_path.clone());
                        file_mtimes.insert(display_path.clone(), mtime);
                    }
                    break;
                }

                seen_content_matches += 1;
                if seen_content_matches <= offset {
                    continue;
                }
                if limit.is_some_and(|lim| blocks.len() >= lim) {
                    truncated = true;
                    break;
                }

                let block = Self::format_block(
                    &display_path,
                    &lines,
                    line_no,
                    context_before,
                    context_after,
                );
                let extra_sep = if blocks.is_empty() { 0 } else { 2 };
                if result_chars + extra_sep + block.len() > Self::MAX_RESULT_CHARS {
                    size_truncated = true;
                    break;
                }
                result_chars += extra_sep + block.len();
                blocks.push(block);
            }

            if output_mode == "count" && file_had_match {
                if !matching_files.contains(&display_path) {
                    matching_files.push(display_path.clone());
                    file_mtimes.insert(display_path.clone(), mtime);
                }
            }
            if matches!(output_mode, "count" | "files_with_matches") && file_had_match {
                continue;
            }
            if truncated || size_truncated {
                break 'files;
            }
        }

        let mut result = if output_mode == "files_with_matches" {
            if matching_files.is_empty() {
                format!("No matches found for pattern '{pattern}' in {path}")
            } else {
                matching_files.sort_by(|a, b| {
                    file_mtimes
                        .get(b)
                        .partial_cmp(&file_mtimes.get(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.cmp(b))
                });
                let (paged, page_truncated) = paginate(matching_files, limit, offset);
                truncated = truncated || page_truncated;
                paged.join("\n")
            }
        } else if output_mode == "count" {
            if counts.is_empty() {
                format!("No matches found for pattern '{pattern}' in {path}")
            } else {
                matching_files.sort_by(|a, b| {
                    file_mtimes
                        .get(b)
                        .partial_cmp(&file_mtimes.get(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.cmp(b))
                });
                let (ordered, page_truncated) = paginate(matching_files, limit, offset);
                truncated = truncated || page_truncated;
                ordered
                    .iter()
                    .map(|name| format!("{}: {}", name, counts.get(name).copied().unwrap_or(0)))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else if blocks.is_empty() {
            format!("No matches found for pattern '{pattern}' in {path}")
        } else {
            blocks.join("\n\n")
        };

        let mut notes: Vec<String> = Vec::new();
        if output_mode == "content" && truncated {
            if let Some(lim) = limit {
                notes.push(format!("(pagination: limit={lim}, offset={offset})"));
            } else {
                notes.push(format!("(pagination: offset={offset})"));
            }
        } else if output_mode == "content" && size_truncated {
            notes.push("(output truncated due to size)".to_string());
        } else if truncated && matches!(output_mode, "count" | "files_with_matches") {
            if let Some(lim) = limit {
                notes.push(format!("(pagination: limit={lim}, offset={offset})"));
            } else {
                notes.push(format!("(pagination: offset={offset})"));
            }
        } else if matches!(output_mode, "count" | "files_with_matches") && offset > 0 {
            notes.push(format!("(pagination: offset={offset})"));
        } else if output_mode == "content" && offset > 0 && !blocks.is_empty() {
            notes.push(format!("(pagination: offset={offset})"));
        }
        if skipped_binary > 0 {
            notes.push(format!("(skipped {skipped_binary} binary/unreadable files)"));
        }
        if skipped_large > 0 {
            notes.push(format!("(skipped {skipped_large} large files)"));
        }
        if output_mode == "count" && !counts.is_empty() {
            let total_matches: usize = counts.values().sum();
            notes.push(format!(
                "(total matches: {total_matches} in {} files)",
                counts.len()
            ));
        }
        if !notes.is_empty() {
            result.push_str("\n\n");
            result.push_str(&notes.join("\n"));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_tool(workspace: Option<PathBuf>) -> SearchTool {
        SearchTool::new(FsToolConfig::new(workspace, None, None))
    }

    // ── display_path ─────────────────────────────────────────────────────────

    // H-C: target under workspace → workspace-relative POSIX path
    #[test]
    fn display_path_under_workspace_returns_workspace_relative() {
        let ws = PathBuf::from("/home/user/project");
        let tool = make_tool(Some(ws.clone()));
        let result = tool.display_path(
            PathBuf::from("/home/user/project/src/main.rs"),
            PathBuf::from("/home/user/project"),
        );
        assert_eq!(result, "src/main.rs");
    }

    // H-C: target NOT under workspace but under root → root-relative path
    #[test]
    fn display_path_outside_workspace_returns_root_relative() {
        let ws = PathBuf::from("/home/user/project");
        let tool = make_tool(Some(ws));
        let result = tool.display_path(
            PathBuf::from("/tmp/other/file.rs"),
            PathBuf::from("/tmp/other"),
        );
        assert_eq!(result, "file.rs");
    }

    // H-C: no workspace → always root-relative
    #[test]
    fn display_path_no_workspace_returns_root_relative() {
        let tool = make_tool(None);
        let result = tool.display_path(
            PathBuf::from("/home/user/project/src/main.rs"),
            PathBuf::from("/home/user/project"),
        );
        assert_eq!(result, "src/main.rs");
    }

    // H-A: target under NEITHER workspace nor root →
    // Python raises ValueError; Rust logs a warning and returns the absolute path.
    // This test documents the intentional divergence: we don't panic, but we
    // do warn rather than silently returning a misleading relative-looking path.
    #[test]
    fn display_path_outside_root_returns_absolute_with_forward_slashes() {
        let tool = make_tool(None);
        let result = tool.display_path(
            PathBuf::from("/unrelated/path/file.rs"),
            PathBuf::from("/home/user/project"),
        );
        // The absolute path is returned (divergence from Python's ValueError),
        // but it must at least have forward slashes (POSIX-normalised).
        assert!(!result.contains('\\'), "path should use forward slashes");
        assert!(
            result.contains("unrelated"),
            "full path should be preserved: {result}"
        );
    }

    // Backslash normalisation (Windows paths)
    #[test]
    fn display_path_normalises_backslashes() {
        let tool = make_tool(None);
        let result = tool.display_path(
            PathBuf::from("C:\\project\\src\\main.rs"),
            PathBuf::from("C:\\project"),
        );
        assert!(
            !result.contains('\\'),
            "backslashes should be replaced with forward slashes"
        );
    }

    // ── iter_entries / iter_files ────────────────────────────────────────────

    fn tool() -> SearchTool {
        make_tool(None)
    }

    #[test]
    fn iter_files_single_file_yields_itself() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("file.txt");
        fs::write(&f, "hello").unwrap();
        let result = tool().iter_files(&f);
        assert_eq!(result, vec![f]);
    }

    #[test]
    fn iter_files_flat_directory_sorted() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("b.txt"), "").unwrap();
        fs::write(tmp.path().join("a.txt"), "").unwrap();
        fs::write(tmp.path().join("c.txt"), "").unwrap();
        let result = tool().iter_files(tmp.path());
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn iter_files_recurses_into_subdirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("root.txt"), "").unwrap();
        fs::write(sub.join("child.txt"), "").unwrap();
        let result = tool().iter_files(tmp.path());
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"root.txt".to_string()));
        assert!(names.contains(&"child.txt".to_string()));
    }

    #[test]
    fn iter_files_skips_ignored_dirs() {
        let tmp = TempDir::new().unwrap();
        // node_modules is in IGNORE_DIRS
        let ignored = tmp.path().join("node_modules");
        fs::create_dir(&ignored).unwrap();
        fs::write(ignored.join("should_not_appear.js"), "").unwrap();
        fs::write(tmp.path().join("visible.rs"), "").unwrap();
        let result = tool().iter_files(tmp.path());
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"visible.rs".to_string()));
        assert!(
            !names.contains(&"should_not_appear.js".to_string()),
            "ignored dir contents must be excluded"
        );
    }

    #[test]
    fn iter_files_empty_directory_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let result = tool().iter_files(tmp.path());
        assert!(result.is_empty());
    }

    #[test]
    fn iter_files_nested_sorted_order() {
        let tmp = TempDir::new().unwrap();
        let b = tmp.path().join("b");
        let a = tmp.path().join("a");
        fs::create_dir(&b).unwrap();
        fs::create_dir(&a).unwrap();
        fs::write(b.join("file.txt"), "").unwrap();
        fs::write(a.join("file.txt"), "").unwrap();
        let result = tool().iter_files(tmp.path());
        // Files from "a/" should appear before files from "b/"
        let paths: Vec<_> = result
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        let a_idx = paths.iter().position(|p| p.contains("/a/")).unwrap();
        let b_idx = paths.iter().position(|p| p.contains("/b/")).unwrap();
        assert!(
            a_idx < b_idx,
            "subdirectory 'a' should be visited before 'b'"
        );
    }

    // iter_entries — include_dirs tests

    #[test]
    fn iter_entries_include_dirs_only() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();
        let result = tool().iter_entries(tmp.path(), false, true);
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"sub".to_string()), "dir should be included");
        assert!(
            !names.contains(&"file.txt".to_string()),
            "file should be excluded"
        );
    }

    #[test]
    fn iter_entries_include_both_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();
        let result = tool().iter_entries(tmp.path(), true, true);
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"sub".to_string()));
        assert!(names.contains(&"file.txt".to_string()));
    }

    #[test]
    fn iter_entries_include_neither_returns_empty() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let result = tool().iter_entries(tmp.path(), false, false);
        assert!(result.is_empty());
    }

    #[test]
    fn iter_entries_root_is_file_include_files_true() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("only.txt");
        fs::write(&f, "x").unwrap();
        let result = tool().iter_entries(&f, true, false);
        assert_eq!(result, vec![f]);
    }

    #[test]
    fn iter_entries_root_is_file_include_files_false() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("only.txt");
        fs::write(&f, "x").unwrap();
        let result = tool().iter_entries(&f, false, true);
        assert!(
            result.is_empty(),
            "file root with include_files=false should return empty"
        );
    }

    #[test]
    fn iter_entries_ignored_dirs_excluded_from_dirs_output() {
        let tmp = TempDir::new().unwrap();
        let ignored = tmp.path().join("node_modules");
        fs::create_dir(&ignored).unwrap();
        let visible = tmp.path().join("src");
        fs::create_dir(&visible).unwrap();
        let result = tool().iter_entries(tmp.path(), false, true);
        let names: Vec<_> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"src".to_string()));
        assert!(
            !names.contains(&"node_modules".to_string()),
            "ignored dir should not appear"
        );
    }

    #[test]
    fn paginate_normal_no_limit() {
        let (v, trunc) = paginate(vec![1, 2, 3, 4, 5], None, 0);
        assert_eq!(v, vec![1, 2, 3, 4, 5]);
        assert!(!trunc);
    }

    #[test]
    fn paginate_normal_with_limit() {
        let (v, trunc) = paginate(vec![1, 2, 3, 4, 5], Some(2), 0);
        assert_eq!(v, vec![1, 2]);
        assert!(trunc);
    }

    #[test]
    fn paginate_offset_within_bounds() {
        let (v, trunc) = paginate(vec![1, 2, 3, 4, 5], Some(2), 2);
        assert_eq!(v, vec![3, 4]);
        assert!(trunc);
    }

    // H-A: offset beyond length, None branch — returns empty, no panic
    #[test]
    fn paginate_offset_beyond_len_no_limit_returns_empty() {
        let (v, trunc) = paginate(vec![1, 2, 3], None, 10);
        assert!(v.is_empty());
        assert!(!trunc);
    }

    // H-B: limit extends beyond end — clamps to available items, no panic
    #[test]
    fn paginate_limit_beyond_len_clamps() {
        let (v, trunc) = paginate(vec![1, 2, 3], Some(10), 0);
        assert_eq!(v, vec![1, 2, 3]);
        assert!(!trunc);
    }

    // H-B + offset: offset + limit both exceed length
    #[test]
    fn paginate_offset_and_limit_both_beyond_len_returns_empty() {
        let (v, trunc) = paginate(vec![1, 2, 3], Some(5), 10);
        assert!(v.is_empty());
        assert!(!trunc);
    }

    // H-D: offset == len, None branch — empty slice, no panic
    #[test]
    fn paginate_offset_equals_len_returns_empty() {
        let (v, trunc) = paginate(vec![1, 2, 3], None, 3);
        assert!(v.is_empty());
        assert!(!trunc);
    }

    // Truncate flag: limit ends exactly at the last item (nothing beyond)
    #[test]
    fn paginate_limit_reaches_end_no_truncation() {
        let (v, trunc) = paginate(vec![1, 2, 3, 4, 5], Some(5), 0);
        assert_eq!(v, vec![1, 2, 3, 4, 5]);
        assert!(!trunc);
    }

    #[test]
    fn test_normalize_pattern() {
        assert_eq!(normalize_pattern("c:\\test\\*.py"), "c:/test/*.py");
    }

    // H-C baseline: simple glob on name — should work like fnmatch
    #[test]
    fn simple_glob_matches_name() {
        assert!(match_glob("src/foo/bar.py", "bar.py", "*.py"));
    }
    #[test]
    fn simple_glob_rejects_wrong_ext() {
        assert!(!match_glob("src/foo/bar.rs", "bar.rs", "*.py"));
    }

    // H-B: right-anchored path matching — Python returns True, does Rust?
    #[test]
    fn path_pattern_right_anchored_should_match() {
        // Python: PurePosixPath("src/foo/bar.py").match("foo/*.py") -> True
        assert!(
            match_glob("src/foo/bar.py", "bar.py", "foo/*.py"),
            "H-B: right-anchored path pattern should match"
        );
    }
    #[test]
    fn path_pattern_full_prefix_matches() {
        assert!(
            match_glob("src/foo/bar.py", "bar.py", "src/foo/*.py"),
            "full prefix pattern should match"
        );
    }
    #[test]
    fn path_pattern_wrong_prefix_should_not_match() {
        assert!(
            !match_glob("src/foo/bar.py", "bar.py", "other/foo/*.py"),
            "wrong prefix should not match"
        );
    }

    // H-A: ** without / should use full rel_path, not just filename
    #[test]
    fn double_star_slash_pattern_matches_anywhere() {
        // Python: PurePosixPath("src/foo/bar.py").match("**/*.py") -> True
        assert!(
            match_glob("src/foo/bar.py", "bar.py", "**/*.py"),
            "H-A: **/*.py should match any py file in any directory"
        );
    }
    #[test]
    fn double_star_pattern_matches_nested() {
        assert!(
            match_glob("a/b/c/d.rs", "d.rs", "**/c/*.rs"),
            "H-A: **/c/*.rs should match file inside c/"
        );
    }

    // ── is_binary ────────────────────────────────────────────────────────────

    #[test]
    fn is_binary_empty_input_returns_false() {
        assert!(!is_binary(b""));
    }

    #[test]
    fn is_binary_null_byte_returns_true() {
        assert!(is_binary(b"hello\x00world"));
    }

    #[test]
    fn is_binary_null_byte_at_start_returns_true() {
        assert!(is_binary(b"\x00rest of data"));
    }

    #[test]
    fn is_binary_plain_text_returns_false() {
        assert!(!is_binary(b"Hello, world!\nThis is plain text.\n"));
    }

    #[test]
    fn is_binary_rust_source_returns_false() {
        let src = b"fn main() {\n    println!(\"hello\");\n}\n";
        assert!(!is_binary(src));
    }

    #[test]
    fn is_binary_high_non_text_ratio_returns_true() {
        // More than 20% of bytes are in the non-text control range (< 9 or 13 < b < 32)
        let mut data = vec![0x01u8; 300]; // all non-text (value 1, which is < 9)
        data.extend_from_slice(b"a".repeat(700).as_slice()); // 70% printable
        // 300/1000 = 30% non-text → binary
        assert!(is_binary(&data));
    }

    #[test]
    fn is_binary_low_non_text_ratio_returns_false() {
        // Less than 20% non-text control bytes
        let mut data = vec![b'a'; 900];
        data.extend_from_slice(&[0x01u8; 100]); // 10% non-text
        assert!(!is_binary(&data));
    }

    #[test]
    fn is_binary_exactly_at_threshold_returns_false() {
        // Exactly 20% non-text: ratio is not strictly > 0.2
        let mut data = vec![b'a'; 800];
        data.extend_from_slice(&[0x01u8; 200]); // exactly 20%
        assert!(!is_binary(&data)); // > 0.2 is strict, so exactly 0.2 is false
    }

    #[test]
    fn is_binary_input_shorter_than_4096_no_panic() {
        // Previously would panic due to raw[..4096] on short input
        let data = b"short file content";
        assert!(!is_binary(data));
    }

    #[test]
    fn is_binary_input_longer_than_4096_uses_sample_only() {
        // First 4096 bytes are clean text; remainder contains null bytes.
        // Since null-byte check runs on the full input first, this will be true.
        // Verify separately that the 4096-byte sampling logic doesn't panic.
        let mut data = vec![b'a'; 4096];
        data.extend_from_slice(b"\x00\x00\x00");
        // null check fires on full raw, so this returns true
        assert!(is_binary(&data));
    }

    #[test]
    fn is_binary_4096_clean_bytes_then_control_bytes_text_wins() {
        // First 4096 bytes are clean; no null bytes in entire input.
        // The sample is entirely text, so result is false even if later bytes are controls.
        let mut data = vec![b'a'; 4096];
        data.extend_from_slice(&[0x01u8; 1000]); // non-text, but outside sample window
        assert!(!is_binary(&data));
    }

    #[test]
    fn is_binary_tab_and_newline_are_not_counted_as_non_text() {
        // \t = 9, \n = 10, \r = 13 — all within the allowed range (not < 9 and not 13 < b < 32)
        let data = b"col1\tcol2\tcol3\nval1\tval2\tval3\n";
        assert!(!is_binary(data));
    }

    #[test]
    fn pagination_note_returns_none_for_zero_offset() {
        assert!(pagination_note(None, 0, false).is_none());
    }

    #[test]
    fn pagination_note_returns_some_for_non_zero_offset() {
        let note = pagination_note(None, 1, false);
        assert!(note.is_some());
        println!("note: {:?}", note.unwrap());
    }

    #[test]
    fn pagination_note_returns_some_for_non_zero_offset_and_limit() {
        assert!(pagination_note(Some(10), 1, false).is_some());
    }

    // ── matches_type ─────────────────────────────────────────────────────────

    #[test]
    fn matches_type_none_always_true() {
        assert!(matches_type("anything.xyz", None));
    }

    #[test]
    fn matches_type_empty_string_always_true() {
        assert!(matches_type("file.rs", Some("")));
    }

    #[test]
    fn matches_type_whitespace_only_always_true() {
        assert!(matches_type("file.rs", Some("   ")));
    }

    #[test]
    fn matches_type_known_type_matches_primary_extension() {
        assert!(matches_type("main.py", Some("py")));
    }

    #[test]
    fn matches_type_known_type_matches_alternate_extension() {
        // "py" maps to ["*.py", "*.pyi"]
        assert!(matches_type("types.pyi", Some("py")));
    }

    #[test]
    fn matches_type_known_type_alias_matches() {
        // "python" is an alias for "py"
        assert!(matches_type("script.py", Some("python")));
    }

    #[test]
    fn matches_type_known_type_rejects_wrong_extension() {
        assert!(!matches_type("main.rs", Some("py")));
    }

    #[test]
    fn matches_type_js_matches_jsx() {
        assert!(matches_type("component.jsx", Some("js")));
    }

    #[test]
    fn matches_type_ts_matches_tsx() {
        assert!(matches_type("app.tsx", Some("ts")));
    }

    #[test]
    fn matches_type_yaml_alias_yml() {
        // "yml" maps to ["*.yaml", "*.yml"]
        assert!(matches_type("config.yaml", Some("yml")));
        assert!(matches_type("config.yml", Some("yml")));
    }

    #[test]
    fn matches_type_unknown_type_falls_back_to_extension() {
        // "xyz" is not in the map, so falls back to "*.xyz"
        assert!(matches_type("data.xyz", Some("xyz")));
    }

    #[test]
    fn matches_type_unknown_type_rejects_different_extension() {
        assert!(!matches_type("data.abc", Some("xyz")));
    }

    #[test]
    fn matches_type_case_insensitive_type_name() {
        assert!(matches_type("main.py", Some("PY")));
        assert!(matches_type("main.py", Some("Python")));
    }

    #[test]
    fn matches_type_case_insensitive_filename() {
        // Filename case should not matter
        assert!(matches_type("MAIN.PY", Some("py")));
    }

    #[test]
    fn matches_type_type_with_surrounding_whitespace() {
        assert!(matches_type("app.rs", Some("  rs  ")));
    }

    // ── GlobTool::execute ────────────────────────────────────────────────────

    fn make_glob_tool(tmp: &TempDir) -> GlobTool {
        GlobTool::new(Some(tmp.path().to_path_buf()), None, None)
    }

    // H-A: simple pattern like "*.py" must match .py files and reject .rs files
    #[tokio::test]
    async fn execute_simple_pattern_matches_correct_extension() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.py"), "").unwrap();
        fs::write(tmp.path().join("lib.rs"), "").unwrap();
        let tool = make_glob_tool(&tmp);
        let result = tool.execute(&serde_json::json!({"pattern": "*.py"})).await;
        assert!(
            result.contains("main.py"),
            "*.py should match main.py; got: {result}"
        );
        assert!(
            !result.contains("lib.rs"),
            "*.py should not match lib.rs; got: {result}"
        );
    }

    #[tokio::test]
    async fn execute_pattern_with_no_matches_returns_message() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "").unwrap();
        let tool = make_glob_tool(&tmp);
        let result = tool.execute(&serde_json::json!({"pattern": "*.py"})).await;
        assert!(result.contains("No matches"), "got: {result}");
    }

    #[tokio::test]
    async fn execute_subdirectory_pattern_matches_nested_file() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("src");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("main.py"), "").unwrap();
        let tool = make_glob_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "src/*.py"}))
            .await;
        assert!(
            result.contains("main.py"),
            "path pattern should match nested file; got: {result}"
        );
    }

    #[tokio::test]
    async fn execute_double_star_pattern_matches_deeply_nested() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.rs"), "").unwrap();
        let tool = make_glob_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "**/*.rs"}))
            .await;
        assert!(
            result.contains("deep.rs"),
            "**/*.rs should match deeply nested file; got: {result}"
        );
    }

    #[tokio::test]
    async fn execute_head_limit_truncates_results() {
        let tmp = TempDir::new().unwrap();
        for i in 0..10 {
            fs::write(tmp.path().join(format!("file{i}.py")), "").unwrap();
        }
        let tool = make_glob_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "*.py", "head_limit": 3}))
            .await;
        let lines: Vec<_> = result.lines().filter(|l| l.ends_with(".py")).collect();
        assert_eq!(
            lines.len(),
            3,
            "head_limit=3 should return 3 results; got: {result}"
        );
        assert!(
            result.contains("pagination"),
            "truncated result should contain pagination note"
        );
    }

    #[tokio::test]
    async fn execute_offset_skips_first_n_results() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            fs::write(tmp.path().join(format!("file{i}.py")), "").unwrap();
        }
        let tool = make_glob_tool(&tmp);
        let all = tool.execute(&serde_json::json!({"pattern": "*.py"})).await;
        let paged = tool
            .execute(&serde_json::json!({"pattern": "*.py", "offset": 2, "head_limit": 10}))
            .await;
        let all_count = all.lines().filter(|l| l.ends_with(".py")).count();
        let paged_count = paged.lines().filter(|l| l.ends_with(".py")).count();
        assert_eq!(paged_count, all_count - 2, "offset=2 should skip 2 results");
    }

    #[tokio::test]
    async fn execute_entry_type_dirs_returns_only_dirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("mydir");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();
        let tool = make_glob_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "*", "entry_type": "dirs"}))
            .await;
        assert!(
            result.contains("mydir"),
            "dirs mode should include directories; got: {result}"
        );
        assert!(
            !result.contains("file.txt"),
            "dirs mode should exclude files; got: {result}"
        );
    }

    // ── GrepTool::execute ────────────────────────────────────────────────────

    fn make_grep_tool(tmp: &TempDir) -> GrepTool {
        GrepTool::new(Some(tmp.path().to_path_buf()), None, None)
    }

    #[tokio::test]
    async fn grep_files_with_matches_finds_pattern() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.py"), "hello world\n").unwrap();
        fs::write(tmp.path().join("other.rs"), "hello world\n").unwrap();
        let tool = make_grep_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello", "glob": "*.py"}))
            .await;
        assert!(result.contains("main.py"), "got: {result}");
        assert!(!result.contains("other.rs"), "got: {result}");
    }

    #[tokio::test]
    async fn grep_count_mode_returns_per_file_totals() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "foo\nfoo\nbar\n").unwrap();
        let tool = make_grep_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "foo", "output_mode": "count"}))
            .await;
        assert!(result.contains("a.txt: 2"), "got: {result}");
        assert!(result.contains("total matches: 2"), "got: {result}");
    }

    #[tokio::test]
    async fn grep_content_mode_includes_match_line() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = make_grep_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "beta", "output_mode": "content"}))
            .await;
        assert!(result.contains("> 2| beta"), "got: {result}");
    }

    #[tokio::test]
    async fn grep_skips_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("text.txt"), "needle\n").unwrap();
        fs::write(tmp.path().join("binary.bin"), b"\x00needle\x00").unwrap();
        let tool = make_grep_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "needle"}))
            .await;
        assert!(result.contains("text.txt"), "got: {result}");
        assert!(result.contains("skipped 1 binary"), "got: {result}");
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = make_grep_tool(&tmp);
        let result = tool
            .execute(&serde_json::json!({"pattern": "[unclosed"}))
            .await;
        assert!(result.starts_with("Error: invalid regex pattern"), "got: {result}");
    }
}
