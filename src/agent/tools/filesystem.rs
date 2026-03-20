use std::path::{Path, PathBuf};
use super::base::Tool;

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
        let result = tool.execute(serde_json::json!({ "path": "notes.txt" }).to_string());
        assert!(result.contains("Error: File not found: notes.txt"));
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
        let tool = ReadFileTool::new(None, None, None);
        // Find the notes.txt file in the docs directory
        let notes_path = Path::new("docs").join("notes.txt");
        assert!(notes_path.exists());
        let result = tool.execute(serde_json::json!({ "path": notes_path.to_str().unwrap() }).to_string());
        println!("result: {}", result);
        assert!(result.contains("rust-bot is for educational, research, and technical exchange purposes only. It is unrelated to crypto and does not involve any official token or coin."));
    }
    
}

