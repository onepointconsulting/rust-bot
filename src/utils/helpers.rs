

use std::path::Path;
use std::path::PathBuf;
use std::fs;

/// Ensure directory exists, return it.
pub fn ensure_dir<P: AsRef<Path>>(path: P) -> PathBuf {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        let _ = fs::create_dir_all(path_ref);
    }
    path_ref.to_path_buf()
}

pub fn touch_file<P: AsRef<Path>>(path: P) {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        let _ = fs::File::create(path_ref);
    }
}