use std::path::{Path, PathBuf};

#[derive(Debug)]
enum ResolvePathError {
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
    
}

