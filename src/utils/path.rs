//! Path abbreviation utilities for display.

use std::path::Path;
use std::sync::LazyLock;

use home::home_dir;
use regex::Regex;
use url::Url;

static HTTP_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://").unwrap());

const ELLIPSIS: &str = "\u{2026}";

/// Normalize path separators for the current platform.
///
/// On Windows, replaces `/` with `\`. On other platforms, returns the path unchanged.
pub fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// Format a path for display with platform-native separators.
pub fn display_path(path: &Path) -> String {
    normalize_path_separators(&path.to_string_lossy())
}

/// Abbreviate a file path or URL, preserving basename and key directories.
///
/// Default `max_len` in the Python API is 40; pass that value at call sites.
pub fn abbreviate_path(path: &str, max_len: usize) -> String {
    if path.is_empty() {
        return String::from(path);
    }

    if HTTP_PREFIX.is_match(path) {
        return abbreviate_url(path, max_len);
    }

    let mut normalized = path.replace('\\', "/");

    if let Some(home) = home_dir() {
        let home = home.to_string_lossy().replace('\\', "/");
        if normalized.starts_with(&(home.clone() + "/")) {
            normalized = format!("~{}", &normalized[home.len()..]);
        } else if normalized == home {
            normalized = "~".to_string();
        }
    }

    if char_len(&normalized) <= max_len {
        return normalized;
    }

    let parts: Vec<&str> = normalized.trim_end_matches('/').split('/').collect();
    if parts.len() <= 1 {
        return truncate_chars(&normalized, max_len);
    }

    let basename = parts[parts.len() - 1];
    let mut budget = max_len as isize - char_len(basename) as isize - 3; // "…/" + final "/"

    let mut kept: Vec<&str> = Vec::new();
    for seg in parts[..parts.len() - 1].iter().rev() {
        let needed = char_len(seg) + 1;
        if kept.is_empty() {
            if needed as isize <= budget {
                kept.push(seg);
                budget -= needed as isize;
            } else {
                break;
            }
        } else if (char_len(seg) + 1) as isize <= budget {
            kept.push(seg);
            budget -= (char_len(seg) + 1) as isize;
        } else {
            break;
        }
    }

    kept.reverse();
    if kept.is_empty() {
        format!("{ELLIPSIS}/{basename}")
    } else {
        format!("{ELLIPSIS}/{}/{}", kept.join("/"), basename)
    }
}

fn abbreviate_url(url: &str, max_len: usize) -> String {
    if char_len(url) <= max_len {
        return url.to_string();
    }

    let parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => return truncate_chars(url, max_len),
    };

    let domain = parsed.host_str().unwrap_or("").to_string();
    let path_part = parsed.path();
    let segments: Vec<&str> = path_part.trim_end_matches('/').split('/').collect();
    let basename = segments.last().copied().unwrap_or("");

    if basename.is_empty() {
        return truncate_chars(url, max_len);
    }

    let mut budget = max_len as isize - char_len(&domain) as isize - char_len(basename) as isize - 4;
    if budget < 0 {
        let trunc = max_len as isize - char_len(&domain) as isize - 5;
        let truncated_base = if trunc > 0 {
            take_chars(basename, trunc as usize)
        } else {
            String::new()
        };
        return format!("{domain}/{ELLIPSIS}/{truncated_base}");
    }

    let mut kept: Vec<&str> = Vec::new();
    for seg in segments[..segments.len().saturating_sub(1)].iter().rev() {
        if (char_len(seg) + 1) as isize <= budget {
            kept.push(seg);
            budget -= (char_len(seg) + 1) as isize;
        } else {
            break;
        }
    }

    kept.reverse();
    if kept.is_empty() {
        format!("{domain}/{ELLIPSIS}/{basename}")
    } else {
        format!("{domain}/{ELLIPSIS}/{}/{}", kept.join("/"), basename)
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn truncate_chars(s: &str, max_len: usize) -> String {
    if char_len(s) <= max_len {
        return s.to_string();
    }
    let take = max_len.saturating_sub(1);
    format!("{}{ELLIPSIS}", take_chars(s, take))
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_normalize_path_separators_mixed() {
        let mixed = r"C:\Users\gilfe\.rust-bot/config.json";
        let result = normalize_path_separators(mixed);
        #[cfg(windows)]
        assert_eq!(result, r"C:\Users\gilfe\.rust-bot\config.json");
        #[cfg(not(windows))]
        assert_eq!(result, mixed);
    }

    #[test]
    fn test_normalize_path_separators_unix_style() {
        let path = "~/.rust-bot/config.json";
        let result = normalize_path_separators(path);
        #[cfg(windows)]
        assert_eq!(result, r"~\.rust-bot\config.json");
        #[cfg(not(windows))]
        assert_eq!(result, path);
    }

    #[test]
    fn test_display_path() {
        let path = PathBuf::from(r"C:\Users\gilfe\.rust-bot").join("config.json");
        let displayed = display_path(&path);
        #[cfg(windows)]
        assert!(!displayed.contains('/'));
        #[cfg(not(windows))]
        assert_eq!(displayed, path.to_string_lossy());
    }

    #[test]
    fn test_abbreviate_path_empty() {
        assert_eq!(abbreviate_path("", 40), "");
    }

    #[test]
    fn test_abbreviate_path_short_unchanged() {
        assert_eq!(abbreviate_path("src/main.rs", 40), "src/main.rs");
    }

    #[test]
    fn test_abbreviate_path_normalizes_backslashes() {
        assert_eq!(abbreviate_path(r"src\lib\mod.rs", 40), "src/lib/mod.rs");
    }

    #[test]
    fn test_abbreviate_path_replaces_home_prefix() {
        let Some(home) = home_dir() else {
            return;
        };
        let home = home.to_string_lossy().replace('\\', "/");
        let path = format!("{home}/projects/rust-bot/src/main.rs");
        let abbreviated = abbreviate_path(&path, 40);
        assert!(abbreviated.starts_with("~/"));
        assert!(abbreviated.ends_with("main.rs"));
    }

    #[test]
    fn test_abbreviate_path_long_relative() {
        let path = "very/long/nested/directory/structure/file.txt";
        let result = abbreviate_path(path, 30);
        assert!(result.starts_with(&format!("{ELLIPSIS}/")));
        assert!(result.ends_with("file.txt"));
        assert!(char_len(&result) <= 30);
    }

    #[test]
    fn test_abbreviate_path_single_segment_truncates() {
        let path = "this-is-a-very-long-single-segment-path-name";
        let result = abbreviate_path(path, 20);
        assert!(result.ends_with(ELLIPSIS));
        assert!(char_len(&result) <= 20);
    }

    #[test]
    fn test_abbreviate_url_short_unchanged() {
        let url = "https://example.com/file.txt";
        assert_eq!(abbreviate_path(url, 40), url);
    }

    #[test]
    fn test_abbreviate_url_long_keeps_domain_and_basename() {
        let url = "https://example.com/api/v2/deep/nested/resource.json";
        let result = abbreviate_path(url, 35);
        assert!(result.starts_with("example.com/"));
        assert!(result.contains(ELLIPSIS));
        assert!(result.ends_with("resource.json"));
        assert!(char_len(&result) <= 35);
    }

    #[test]
    fn test_abbreviate_url_no_basename_truncates() {
        let url = "https://very-long-domain-name-for-testing.example.com/";
        let result = abbreviate_path(url, 25);
        assert!(char_len(&result) <= 25);
        assert!(result.ends_with(ELLIPSIS));
    }

    #[test]
    fn test_abbreviate_url_tight_budget_truncates_basename() {
        let url = "https://example.com/this-is-a-very-long-filename.json";
        let result = abbreviate_path(url, 25);
        assert!(result.starts_with("example.com/"));
        assert!(result.contains(&format!("/{ELLIPSIS}/")));
        assert!(char_len(&result) <= 25);
    }
}
