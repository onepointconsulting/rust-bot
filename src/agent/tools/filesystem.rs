use std::path::{Path, PathBuf};
use similar::TextDiff;
use super::base::Tool;
use globwalk::GlobWalkerBuilder;

#[derive(Debug)]
pub(crate) enum ResolvePathError {
    HomeDirUnavailable,
    NotUnderAllowedDir { path: PathBuf, allowed: PathBuf },
    NotUnderAnyAllowedDir { path: PathBuf }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join(path)
}

// Equivalent to the Python:
//   path.relative_to(directory.resolve())
// returning True when `path` is under `directory`.
//
// Notes:
// - Python's `resolve()` is non-strict by default; Rust's `canonicalize()` is strict (requires existence).
// - We attempt `canonicalize()`; if it fails, we fall back to absolute paths without dereferencing/symlink resolution.
fn _is_under(path: &Path, directory: &Path) -> bool {
    let resolved_dir = directory
        .canonicalize()
        .unwrap_or_else(|_| absolute_path(directory));
    let resolved_path = path.canonicalize().unwrap_or_else(|_| absolute_path(path));
    resolved_path.starts_with(&resolved_dir)
}

fn _resolve_path(
    path: &str,
    workspace: Option<PathBuf>,
    allowed_dir: Option<PathBuf>,
    extra_allowed_dirs: Option<Vec<PathBuf>>,
) -> Result<PathBuf, ResolvePathError> {
    let mut p = PathBuf::from(path);

    // Expand '~' to home directory if present at the start
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = home::home_dir() {
            p = home.join(stripped);
        } else {
            return Result::Err(ResolvePathError::HomeDirUnavailable);
        }
    }

    if !p.is_absolute() {
        if let Some(ref ws) = workspace {
            p = ws.join(&p);
        }
    }
    // Rust equivalent of Python's p.resolve():
    let resolved = p.canonicalize().unwrap_or_else(|_| absolute_path(&p));
    if let Some(ref ws) = allowed_dir {
        match extra_allowed_dirs {
            Some(dirs) => {
                for dir in dirs.iter().chain(std::iter::once(ws)) {
                    if _is_under(&resolved, &dir) {
                        return Result::Ok(resolved);
                    }
                }
                return Result::Err(ResolvePathError::NotUnderAnyAllowedDir { path: resolved });
            }
            None => {
                if !_is_under(&resolved, ws) {
                    return Result::Err(ResolvePathError::NotUnderAllowedDir { path: resolved, allowed: ws.clone() });
                }
                return Result::Ok(resolved);
            }
        }
    }
    Result::Ok(resolved)
}

fn _find_match(content: &str, old_text: &str) -> (Option<String>, usize) {
    if content.contains(old_text) {
        let count = content.matches(old_text).count();
        return (Some(old_text.to_string()), count);
    }

    let old_lines: Vec<&str> = old_text.lines().collect();
    if old_lines.is_empty() {
        return (None, 0);
    }
    let stripped_old: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();
    let content_lines: Vec<&str> = content.lines().collect();

    let mut candidates: Vec<String> = Vec::new();
    if content_lines.len() >= old_lines.len() {
        for i in 0..=(content_lines.len() - old_lines.len()) {
            let window = &content_lines[i..i + old_lines.len()];
            if window.iter().map(|l| l.trim()).collect::<Vec<_>>() == stripped_old {
                candidates.push(window.join("\n"));
            }
        }
    }

    if !candidates.is_empty() {
        return (Some(candidates[0].clone()), candidates.len());
    }
    (None, 0)
}

// ---------------------------------------------------------------------------
// FsToolConfig — shared config for filesystem tools.
//
// Rust doesn't have class inheritance, so the Python `_FsTool` base class is
// expressed as a plain struct that each FS tool embeds by composition.
// ---------------------------------------------------------------------------

pub struct FsToolConfig {
    workspace: Option<PathBuf>,
    allowed_dir: Option<PathBuf>,
    extra_allowed_dirs: Option<Vec<PathBuf>>,
}

impl FsToolConfig {
    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self { workspace, allowed_dir, extra_allowed_dirs }
    }

    /// Equivalent to `self._resolve(path)` in the Python base class.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ResolvePathError> {
        _resolve_path(
            path,
            self.workspace.clone(),
            self.allowed_dir.clone(),
            self.extra_allowed_dirs.clone(),
        )
    }
}

// ---------------------------------------------------------------------------
// ReadFileTool
// ---------------------------------------------------------------------------

/// Read file contents with optional line-based pagination.
pub struct ReadFileTool {
    fs: FsToolConfig,
}

pub struct WriteFileTool {
    fs: FsToolConfig,
}

pub struct EditFileTool {
    fs: FsToolConfig,
}

pub struct ListDirTool {
    fs: FsToolConfig,
}

impl ReadFileTool {
    const MAX_CHARS: usize = 128_000;
    const DEFAULT_LIMIT: usize = 2_000;

    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self { fs: FsToolConfig::new(workspace, allowed_dir, extra_allowed_dirs) }
    }
}

impl WriteFileTool {

    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self { fs: FsToolConfig::new(workspace, allowed_dir, extra_allowed_dirs) }
    }
}

impl ListDirTool {
    const DEFAULT_MAX: usize = 200;
    const IGNORE_DIRS: &'static [&'static str] = &[
        ".git", "node_modules", "__pycache__", ".venv", "target",
    ];

    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self { fs: FsToolConfig::new(workspace, allowed_dir, extra_allowed_dirs) }
    }
}

impl EditFileTool {

    pub fn new(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self { fs: FsToolConfig::new(workspace, allowed_dir, extra_allowed_dirs) }
    }

    fn _not_found_msg(&self, old_text: &str, content: &str, path: &str) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let old_lines: Vec<&str> = old_text.lines().collect();
        let window = old_lines.len();

        let mut best_ratio = 0.0_f32;
        let mut best_start = 0_usize;

        let iterations = (lines.len() + 1).saturating_sub(window).max(1);
        for i in 0..iterations {
            let candidate = lines[i..(i + window).min(lines.len())].join("\n");
            let ratio = TextDiff::from_lines(old_text, &candidate).ratio();
            if ratio > best_ratio {
                best_ratio = ratio;
                best_start = i;
            }
        }

        if best_ratio > 0.5 {
            let actual_window = lines[best_start..(best_start + window).min(lines.len())].join("\n");
            let diff = TextDiff::from_lines(old_text, &actual_window);
            let unified = diff
                .unified_diff()
                .header(
                    "old_text (provided)",
                    &format!("{} (actual, line {})", path, best_start + 1),
                )
                .to_string();
            return format!(
                "Error: old_text not found in {}.\nBest match ({:.0}% similar) at line {}:\n{}",
                path,
                best_ratio * 100.0,
                best_start + 1,
                unified,
            );
        }
        format!(
            "Error: old_text not found in {}. No similar text found. Verify the file content.",
            path
        )
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> String {
        "read_file".to_string()
    }

    fn description(&self) -> String {
        "Read the contents of a file. Returns numbered lines. \
         Use offset and limit to paginate through large files."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The file path to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed, default 1)",
                    "minimum": 1
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default 2000)",
                    "minimum": 1
                }
            },
            "required": ["path"]
        })
    }

    /// Input is a JSON object string: `{"path": "...", "offset": 1, "limit": 100}`.
    fn execute(&self, input: String) -> String {
        let args: serde_json::Value = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(_) => return "Error: invalid JSON input".to_string(),
        };

        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: missing required parameter 'path'".to_string(),
        };

        let mut offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

        let fp = match self.fs.resolve(path) {
            Ok(p) => p,
            Err(e) => return format!("Error: {:?}", e),
        };

        if !fp.exists() {
            return format!("Error: File not found: {}", path);
        }
        if !fp.is_file() {
            return format!("Error: Not a file: {}", path);
        }

        let content = match std::fs::read_to_string(&fp) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {}", e),
        };

        let all_lines: Vec<&str> = content.lines().collect();
        let total = all_lines.len();

        offset = offset.max(1);

        if total == 0 {
            return format!("(Empty file: {})", path);
        }
        if offset > total {
            return format!(
                "Error: offset {} is beyond end of file ({} lines)",
                offset, total
            );
        }

        let start = offset - 1;
        let end = (start + limit.unwrap_or(Self::DEFAULT_LIMIT)).min(total);

        let numbered: Vec<String> = all_lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}| {}", start + i + 1, line))
            .collect();

        let mut result = numbered.join("\n");

        // Trim to MAX_CHARS if the result is too large.
        if result.len() > Self::MAX_CHARS {
            let mut trimmed: Vec<&str> = Vec::new();
            let mut chars = 0usize;
            for line in &numbered {
                chars += line.len() + 1;
                if chars > Self::MAX_CHARS {
                    break;
                }
                trimmed.push(line);
            }
            let end_trimmed = start + trimmed.len();
            result = trimmed.join("\n");
            result += &format!(
                "\n\n(Showing lines {}-{} of {}. Use offset={} to continue.)",
                offset, end_trimmed, total, end_trimmed + 1
            );
            return result;
        }

        if end < total {
            result += &format!(
                "\n\n(Showing lines {}-{} of {}. Use offset={} to continue.)",
                offset, end, total, end + 1
            );
        } else {
            result += &format!("\n\n(End of file — {} lines total)", total);
        }
        result
    }
}

impl Tool for WriteFileTool {

    fn name(&self) -> String {
        "write_file".to_string()
    }
    
    fn description(&self) -> String {
        "Write content to a file at the given path. Creates parent directories if needed."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file path to write to"},
                "content": {"type": "string", "description": "The content to write"},
            },
            "required": ["path", "content"],
        })
    }

    fn execute(&self, input: String) -> String {
        let args: serde_json::Value = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(_) => return "Error: invalid JSON input".to_string(),
        };

        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: missing required parameter 'path'".to_string(),
        };

        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: missing required parameter 'content'".to_string(),
        };

        let fp = match self.fs.resolve(path) {
            Ok(p) => p,
            Err(e) => return format!("Error: {:?}", e),
        };

        if let Some(parent) = fp.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("Error writing file: {}", e);
            }
        }

        match std::fs::write(&fp, content.as_bytes()) {
            Ok(_) => format!("Successfully wrote {} bytes to {}", content.len(), fp.display()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                format!("Error: {}", e)
            }
            Err(e) => format!("Error writing file: {}", e),
        }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> String {
        "edit_file".to_string()
    }

    fn description(&self) -> String {
        "Edit the contents of a file".to_string()
    }
    
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file path to edit"},
                "old_text": {"type": "string", "description": "The text to find and replace"},
                "new_text": {"type": "string", "description": "The text to replace with"},
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default false)",
                },
            },
            "required": ["path", "old_text", "new_text"],
        })
    }

    fn execute(&self, input: String) -> String {
        let args: serde_json::Value = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(_) => return "Error: invalid JSON input".to_string(),
        };

        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Error: missing required parameter 'path'".to_string(),
        };

        let old_text = match args.get("old_text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return "Error: missing required parameter 'old_text'".to_string(),
        };

        let new_text = match args.get("new_text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return "Error: missing required parameter 'new_text'".to_string(),
        };

        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

        let fp = match self.fs.resolve(path) {
            Ok(p) => p,
            Err(e) => return format!("Error: {:?}", e),
        };

        if !fp.exists() {
            return format!("Error: File not found: {}", path);
        }

        let raw = match std::fs::read(&fp) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return format!("Error: {}", e);
            }
            Err(e) => return format!("Error editing file: {}", e),
        };

        let uses_crlf = raw.windows(2).any(|w| w == b"\r\n");

        let content = match String::from_utf8(raw) {
            Ok(s) => s,
            Err(e) => return format!("Error editing file: {}", e),
        };
        let content = content.replace("\r\n", "\n");

        let norm_old = old_text.replace("\r\n", "\n");
        let (matched, count) = _find_match(&content, &norm_old);

        let matched = match matched {
            Some(m) => m,
            None => return self._not_found_msg(old_text, &content, path),
        };

        if count > 1 && !replace_all {
            return format!(
                "Warning: old_text appears {} times. \
                 Provide more context to make it unique, or set replace_all=true.",
                count
            );
        }

        let norm_new = new_text.replace("\r\n", "\n");
        let mut new_content = if replace_all {
            content.replace(&matched, &norm_new)
        } else {
            match content.find(&matched) {
                Some(pos) => {
                    let mut s = content[..pos].to_string();
                    s.push_str(&norm_new);
                    s.push_str(&content[pos + matched.len()..]);
                    s
                }
                None => content,
            }
        };

        if uses_crlf {
            new_content = new_content.replace('\n', "\r\n");
        }

        match std::fs::write(&fp, new_content.as_bytes()) {
            Ok(_) => format!("Successfully edited {}", fp.display()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                format!("Error: {}", e)
            }
            Err(e) => format!("Error editing file: {}", e),
        }
    }

}

impl Tool for ListDirTool {

    fn name(&self) -> String {
        "list_dir".to_string()
    }

    fn description(&self) -> String {
        "List the contents of a directory".to_string()
    }
    
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The directory path to list"},
                "recursive": {
                    "type": "boolean",
                    "description": "Recursively list all files (default false)",
                },
                "max_entries": {
                    "type": "integer",
                    "description": "Maximum entries to return (default 200)",
                    "minimum": 1,
                },
            },
            "required": ["path"],
        })
    }

    fn execute(&self, input: String) -> String {
        let args: serde_json::Value = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(_) => return "Error: invalid JSON input".to_string(),
        };

        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: missing required parameter 'path'".to_string(),
        };

        let recursive = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
        let cap = args
            .get("max_entries")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(Self::DEFAULT_MAX);

        let dp = match self.fs.resolve(path) {
            Ok(p) => p,
            Err(e) => return format!("Error: {:?}", e),
        };

        if !dp.exists() {
            return format!("Error: Directory not found: {}", path);
        }
        if !dp.is_dir() {
            return format!("Error: Not a directory: {}", path);
        }

        let mut items: Vec<String> = Vec::new();
        let mut total = 0_usize;

        if recursive {
            let walker = match GlobWalkerBuilder::from_patterns(&dp, &["**/*"]).build() {
                Ok(w) => w,
                Err(e) => return format!("Error listing directory: {}", e),
            };

            let mut paths: Vec<PathBuf> = walker
                .filter_map(|e| e.ok())
                .map(|e| e.path().to_path_buf())
                .collect();
            paths.sort();

            for item in paths {
                let rel = match item.strip_prefix(&dp) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                let ignored = rel.components().any(|c| {
                    if let std::path::Component::Normal(s) = c {
                        Self::IGNORE_DIRS.contains(&s.to_str().unwrap_or(""))
                    } else {
                        false
                    }
                });
                if ignored {
                    continue;
                }
                total += 1;
                if items.len() < cap {
                    let posix_rel = rel.components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");

                    if item.is_dir() {
                        items.push(format!("{}/", posix_rel));
                    } else {
                        items.push(format!("{}", posix_rel));
                    }
                }
            }
        } else {
            let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&dp) {
                Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return format!("Error: {}", e);
                }
                Err(e) => return format!("Error listing directory: {}", e),
            };
            entries.sort_by_key(|e| e.path());

            for entry in entries {
                total += 1;
                if items.len() < cap {
                    let entry_path = entry.path();
                    let rel = entry_path.strip_prefix(&dp).unwrap_or(entry_path.as_path());
                    items.push(format!("{}", rel.display()));
                }
            }
        }

        if items.is_empty() && total == 0 {
            return format!("Directory {} is empty", path);
        }

        let mut result = items.join("\n");
        if total > cap {
            result += &format!(
                "\n\n(truncated, showing first {} of {} entries)",
                cap, total
            );
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_temp_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rust-bot-test-{}", nanos))
    }

    fn sample_file() -> PathBuf {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let content: String = (1..=20)
            .map(|i| format!("{}| line {}", i, i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_is_under_true_and_false() {
        let base = unique_temp_dir();
        let allowed_dir = base.join("allowed");
        let other = base.join("other");

        fs::create_dir_all(allowed_dir.join("sub")).unwrap();
        fs::create_dir_all(&other).unwrap();

        let file_ok = allowed_dir.join("sub").join("file.txt");
        let file_no = other.join("file.txt");
        fs::write(&file_ok, b"ok").unwrap();
        fs::write(&file_no, b"no").unwrap();

        assert!(_is_under(&file_ok, &allowed_dir));
        assert!(!_is_under(&file_no, &allowed_dir));
        assert!(_is_under(&allowed_dir, &allowed_dir));

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn test_resolve_home_dir() {
        let path = "~/Documents/notes.txt";
        let resolved = _resolve_path(path, None, None, None).unwrap();
        assert!(resolved.starts_with(home::home_dir().unwrap()));
        assert!(resolved.ends_with("Documents/notes.txt"));
    }

    #[test]
    fn test_resolve_workspace_dir() {
        let workspace = unique_temp_dir();
        let path = "notes.txt";
        let resolved = _resolve_path(path, Some(workspace.clone()), None, None).unwrap();
        assert!(resolved.starts_with(workspace));
        assert!(resolved.ends_with("notes.txt"));
    }


    #[test]
    fn test_resolve_allowed_dir() {
        let allowed_dir = unique_temp_dir();
        let path = "notes.txt";
        let allowed_path = allowed_dir.join(path);
        let resolved = _resolve_path(allowed_path.to_str().unwrap(), None, Some(allowed_dir.clone()), None).unwrap();
        assert!(resolved.starts_with(allowed_dir));
        assert!(resolved.ends_with("notes.txt"));
    }

    #[test]
    fn test_resolve_allowed_dir_with_extra_allowed_dirs() {
        let allowed_dir = unique_temp_dir();
        let extra_allowed_dir = unique_temp_dir().join("allowed");
        let path = "notes.txt";
        let allowed_path = allowed_dir.join(path);
        let resolved = _resolve_path(allowed_path.to_str().unwrap(), None, Some(allowed_dir.clone()), Some(vec![extra_allowed_dir.clone()])).unwrap();
        assert!(resolved.starts_with(allowed_dir));
        assert!(resolved.ends_with("notes.txt"));
    }

    #[test]
    fn test_read_missing_file_tool() {
        let tool = ReadFileTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "path": "missing.txt" }).to_string());
        assert!(result.contains("Error: File not found: missing.txt"));
    }

    #[test]
    fn test_read_missing_path_tool() {
        let tool = ReadFileTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ }).to_string());
        println!("result: {}", result);
        assert!(result.contains("Error: missing required parameter 'path'"));
    }

    #[test]
    fn test_read_content_tool() {
        let docs_path = Path::new("docs");
        let tool = ReadFileTool::new(None, Some(docs_path.to_path_buf()), None);
        // Find the notes.txt file in the docs directory
        let notes_path = docs_path.join("notes.txt");
        assert!(notes_path.exists());
        let result = tool.execute(serde_json::json!({ "path": notes_path.to_str().unwrap() }).to_string());
        println!("result: {}", result);
        assert!(result.contains("rust-bot is for educational, research, and technical exchange purposes only. It is unrelated to crypto and does not involve any official token or coin."));
    }

    #[test]
    fn test_write_missing_path_tool() {
        let tool = WriteFileTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "content": "Hello, world!" }).to_string());
        println!("result: {}", result);
        assert!(result.contains("Error: missing required parameter 'path'"));
    }

    #[test]
    fn test_write_missing_content() {
        let tool = WriteFileTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "path": "notes.txt" }).to_string());
        println!("result: {}", result);
        assert!(result.contains("Error: missing required parameter 'content'"));
    }

    #[test]
    fn test_write_tool_success() {
        let tool = WriteFileTool::new(None, None, None);
        let notes_text = "notes.txt";
        let result = tool.execute(serde_json::json!({ "path": notes_text, "content": "Hello, world!" }).to_string());
        println!("result: {}", result);
        assert!(result.contains(format!("Successfully wrote").as_str()));
        assert!(Path::new(notes_text).exists());
    }
    
    #[test]
    fn test_find_match() {
        let content = "Hello, world!\nHello, rust!\nHello, world!";
        let old_text = "Hello, world!";
        let (matched_fragment, count) = _find_match(content, old_text);
        assert!(matched_fragment.is_some());
        assert!(count == 2);
    }

    #[test]
    fn test_find_match_count_1() {
        let sample_file = sample_file();
        let content = std::fs::read_to_string(&sample_file).unwrap();
        assert!(content.contains("1| line 1"));
        let (matched_fragment, count) = _find_match(content.as_str(), "1| line 1");
        assert!(matched_fragment.is_some());
        assert!(count == 2); // "1| line 1" and "11| line 11" both contain the expression "1| line 1"
    }

    #[test]
    fn test_find_match_indented() {
        let content = "Hello, world!     \n     Hello, rust!     \n         Hello, world!";
        let old_text = "Hello, world!\nHello, rust!";
        let (matched_fragment, count) = _find_match(content, old_text);
        assert!(matched_fragment.is_some());
        assert!(count == 1);
    }

    #[test]
    fn test_edit_simple_match() {
        let tool: EditFileTool = EditFileTool::new(None, None, None);
        let sample_file = sample_file();
        let content = std::fs::read_to_string(&sample_file).unwrap();
        assert!(content.contains("1| line 1"));
        let result = tool.execute(serde_json::json!(
            { 
                "path": sample_file.to_str().unwrap(),
                "old_text": "1| line 1",
                "new_text": "---1| line 1---",
                "replace_all": true
            }).to_string());
        println!("result: {}", result);
        assert!(result.contains(format!("Successfully edited").as_str()));
        assert!(sample_file.exists());

        let content = std::fs::read_to_string(&sample_file).unwrap();
        assert!(content.contains("---1| line 1---"));
    }

    #[test]
    fn test_edit_replace_all_false() {
        let tool: EditFileTool = EditFileTool::new(None, None, None);
        let sample_file = sample_file();
        let content = std::fs::read_to_string(&sample_file).unwrap();
        assert!(content.contains("1| line 1"));
        let result = tool.execute(serde_json::json!(
            { 
                "path": sample_file.to_str().unwrap(),
                "old_text": "1| line 1",
                "new_text": "---1| line 1---, but only the first occurrence",
                "replace_all": false
            }).to_string());
        println!("result: {}", result);
        assert!(result.contains(format!("Warning: old_text appears 2 times. Provide more context to make it unique, or set replace_all=true.").as_str()));
    }

    #[test]
    fn test_list_dir_tool() {
        let tool = ListDirTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "path": "docs", "recursive": false }).to_string());
        println!("result: {}", result);
        assert!(result.contains("notes.txt"));
        assert!(result.contains("credits"));
    }

    #[test]
    fn test_list_dir_tool_recursive() {
        let tool = ListDirTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "path": "docs", "recursive": true }).to_string());
        println!("result: {}", result);
        assert!(result.contains("notes.txt"));
        assert!(result.contains("credits/"));
        assert!(result.contains("credits/nanobot.txt"));
    }

    #[test]
    fn test_list_dir_tool_expression() {
        let tool = ListDirTool::new(None, None, None);
        let result = tool.execute(serde_json::json!({ "path": "src", "recursive": true }).to_string());
        println!("result: {}", result);
        assert!(result.contains("agent/mod.rs"));
        assert!(result.contains("agent/tools/mod.rs"));
    }
    
}

