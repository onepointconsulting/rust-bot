//! Where an instance's files are written on this machine, and how its config refers to them.
//!
//! For an ordinary (`local` / `user`) instance both are the same directory: a file written at
//! `<dir>/credentials/private_key.pem` is recorded under that path. For a Docker instance the
//! files are written into a host folder that the container mounts at a fixed root (`/data`),
//! so the config must record `/data/credentials/private_key.pem` instead.

use std::path::{Component, Path, PathBuf};

use crate::cli::commands::path_for_config;

/// Host directory that receives an instance's files, plus the root its config refers to them by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLayout {
    host_dir: PathBuf,
    /// `None`: the config records host paths. `Some(root)`: the config records POSIX paths under
    /// `root` (a container mount point), whatever the host platform.
    container_root: Option<String>,
}

impl InstanceLayout {
    /// Layout whose config records the host paths themselves.
    pub fn native(host_dir: impl Into<PathBuf>) -> Self {
        Self {
            host_dir: host_dir.into(),
            container_root: None,
        }
    }

    /// Native layout of the instance whose config file is `config_path`: the files live next to it.
    pub fn for_config_file(config_path: &Path) -> Self {
        Self::native(config_path.parent().unwrap_or_else(|| Path::new(".")))
    }

    /// Layout whose files are written to `host_dir`, which a container mounts at `container_root`
    /// (e.g. `/data`). A trailing `/` on `container_root` is ignored.
    pub fn container(host_dir: impl Into<PathBuf>, container_root: &str) -> Self {
        Self {
            host_dir: host_dir.into(),
            container_root: Some(container_root.trim_end_matches('/').to_string()),
        }
    }

    /// Directory on this machine that receives the instance's files.
    pub fn host_dir(&self) -> &Path {
        &self.host_dir
    }

    /// `true` when the config records container paths rather than host paths.
    pub fn is_container(&self) -> bool {
        self.container_root.is_some()
    }

    /// Host location of the instance file at `relative_parts` (e.g. `["credentials", "x.pem"]`).
    pub fn host_path(&self, relative_parts: &[&str]) -> PathBuf {
        relative_parts
            .iter()
            .fold(self.host_dir.clone(), |path, part| path.join(part))
    }

    /// Value the config records for the instance file at `relative_parts`.
    ///
    /// Native: the host path, as [`Path::display`] renders it. Container: `<root>/<parts joined by />`.
    pub fn config_path(&self, relative_parts: &[&str]) -> String {
        match &self.container_root {
            None => self.host_path(relative_parts).display().to_string(),
            Some(root) => join_posix(root, relative_parts.iter().copied()),
        }
    }

    /// Value the config records for a file this machine has just written at `written_host_path`.
    ///
    /// Native: the canonical host path (see [`path_for_config`]), as before this type existed.
    /// Container: the path relative to [`Self::host_dir`], re-rooted under the container root.
    ///
    /// # Errors
    ///
    /// In container mode, when `written_host_path` is not inside [`Self::host_dir`]; the
    /// container could not see such a file.
    pub fn config_path_for_written(&self, written_host_path: PathBuf) -> Result<String, String> {
        let Some(root) = &self.container_root else {
            return Ok(path_for_config(written_host_path).display().to_string());
        };
        let relative = written_host_path
            .strip_prefix(&self.host_dir)
            .map_err(|_| {
                format!(
                    "{} is outside the instance folder {} and would not be visible in the container",
                    written_host_path.display(),
                    self.host_dir.display()
                )
            })?
            .to_path_buf();
        let parts: Vec<String> = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        Ok(join_posix(root, parts.iter().map(String::as_str)))
    }
}

/// `root/part1/part2/...` with `/` separators, independent of the host platform.
fn join_posix<'a>(root: &str, parts: impl Iterator<Item = &'a str>) -> String {
    parts.fold(root.to_string(), |joined, part| format!("{joined}/{part}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_layout_records_the_host_paths() {
        let layout = InstanceLayout::native("/project/.rust-bot");
        let host = layout.host_path(&["credentials", "private_key.pem"]);
        assert_eq!(
            host,
            Path::new("/project/.rust-bot")
                .join("credentials")
                .join("private_key.pem")
        );
        assert_eq!(
            layout.config_path(&["credentials", "private_key.pem"]),
            host.display().to_string()
        );
        assert!(!layout.is_container());
    }

    #[test]
    fn container_layout_writes_on_the_host_and_records_posix_container_paths() {
        let layout = InstanceLayout::container("/srv/bot/data", "/data/");
        assert_eq!(
            layout.host_path(&["users.json"]),
            Path::new("/srv/bot/data").join("users.json")
        );
        // Forward slashes on every platform: the path is read inside a Linux container.
        assert_eq!(
            layout.config_path(&["credentials", "private_key.pem"]),
            "/data/credentials/private_key.pem"
        );
        assert_eq!(layout.config_path(&[]), "/data");
        assert!(layout.is_container());
    }

    #[test]
    fn written_file_inside_the_container_folder_is_re_rooted() {
        let host_dir = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::container(host_dir.path(), "/data");
        let written = layout.host_path(&["credentials", "public_key.pem"]);
        assert_eq!(
            layout.config_path_for_written(written).unwrap(),
            "/data/credentials/public_key.pem"
        );
    }

    #[test]
    fn written_file_outside_the_container_folder_is_an_error() {
        let host_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::container(host_dir.path(), "/data");
        let error = layout
            .config_path_for_written(other_dir.path().join("users.json"))
            .unwrap_err();
        assert!(error.contains("outside the instance folder"), "{error}");
    }

    #[test]
    fn written_file_in_a_native_layout_keeps_the_canonical_host_path() {
        let host_dir = tempfile::tempdir().unwrap();
        let written = host_dir.path().join("users.json");
        std::fs::write(&written, "{}").unwrap();
        let layout = InstanceLayout::native(host_dir.path());
        assert_eq!(
            layout.config_path_for_written(written.clone()).unwrap(),
            path_for_config(written).display().to_string()
        );
    }
}
