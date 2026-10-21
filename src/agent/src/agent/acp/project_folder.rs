//! Which project folder a parent may give a child.
//!
//! A child's *home* (memory, sessions, bootstrap files) is never the folder it
//! works on. The folder it works on is chosen per run and becomes the child
//! session's project scope. This module decides whether a requested folder is
//! acceptable, and which rust-bot homes inside it the child's file tools must
//! refuse:
//!
//! * the folder must be absolute and exist;
//! * a restricted parent can only hand out folders inside its own project, so a
//!   child never reaches further than its parent;
//! * the folder may never be, or lie inside, a rust-bot home — that would let the
//!   child read and edit the parent's memory, sessions and its siblings' homes;
//! * a home that merely lies *inside* the folder (the common `./.rust-bot/workspace`
//!   next to the code) is allowed, but becomes a denied subtree.
//!
//! There is no silent fallback to the parent's home: when there is nowhere else
//! to work, the caller is told to name a folder.

use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::context::BOOTSTRAP_FILES;
use crate::agent::tools::filesystem::soft_resolve;

/// Folders a scan for homes never enters: large, generated, and never a home.
const SCAN_SKIPPED_FOLDERS: [&str; 5] = [".git", "node_modules", "__pycache__", ".venv", "target"];
/// How deep below the project folder a rust-bot home is looked for.
const SCAN_MAX_DEPTH: usize = 6;
/// How many folders a scan visits before it gives up (a huge tree stays cheap).
const SCAN_MAX_FOLDERS: usize = 50_000;

/// What the parent knows when it picks a folder.
#[derive(Debug, Clone)]
pub struct ProjectRequest<'a> {
    /// The `cwd` the model passed to the tool, if any.
    pub requested: Option<&'a str>,
    /// The agent's stored default (`agent.json`).
    pub agent_default: Option<&'a str>,
    /// The parent session's current project folder.
    pub parent_project: &'a Path,
    /// Whether the parent session is confined to its project folder.
    pub parent_restricted: bool,
    /// The parent's own home.
    pub parent_workspace: &'a Path,
    /// Whether the child will have the shell tool (it is not path-confined).
    pub child_shell_enabled: bool,
}

/// The folder the child works on and what it must not touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChoice {
    pub cwd: PathBuf,
    /// Subtrees the child's file tools refuse: the parent's home plus every
    /// rust-bot home found inside `cwd`.
    pub denied_roots: Vec<PathBuf>,
    /// Things the operator should hear about.
    pub warnings: Vec<String>,
}

/// Why no folder could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectFolderError {
    /// Nothing was passed, there is no stored default, and the parent has no
    /// separate project folder to fall back to.
    NoProjectFolder,
    NotAbsolute(PathBuf),
    NotAFolder(PathBuf),
    /// A restricted parent cannot hand out a folder outside its own project.
    OutsideParentProject {
        cwd: PathBuf,
        project: PathBuf,
    },
    /// The folder is, or lies inside, a rust-bot home.
    InsideHome {
        cwd: PathBuf,
        home: PathBuf,
    },
}

impl std::fmt::Display for ProjectFolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectFolderError::NoProjectFolder => write!(
                f,
                "no project folder: pass `cwd` (the folder the agent should work on, e.g. the \
                 repository to review), or give the agent a default with acp_update_agent. The \
                 agent never works inside rust-bot's own workspace."
            ),
            ProjectFolderError::NotAbsolute(path) => {
                write!(f, "cwd must be an absolute path, got '{}'", path.display())
            }
            ProjectFolderError::NotAFolder(path) => write!(
                f,
                "cwd '{}' does not exist or is not a folder",
                path.display()
            ),
            ProjectFolderError::OutsideParentProject { cwd, project } => write!(
                f,
                "cwd '{}' is outside this session's project folder '{}'; an agent can never \
                 reach further than the agent that starts it",
                cwd.display(),
                project.display()
            ),
            ProjectFolderError::InsideHome { cwd, home } => write!(
                f,
                "cwd '{}' is inside the rust-bot home '{}', which holds memory and conversations \
                 of other agents; pick the project folder instead",
                cwd.display(),
                home.display()
            ),
        }
    }
}

impl std::error::Error for ProjectFolderError {}

/// Whether `dir` is a rust-bot home: a workspace with its own memory and sessions
/// and bootstrap files, or one that holds child agents.
pub fn is_rust_bot_home(dir: &Path) -> bool {
    let has_children = dir.join("acp").join("agents").is_dir();
    let has_workspace_layout = dir.join("memory").is_dir()
        && dir.join("sessions").is_dir()
        && BOOTSTRAP_FILES.iter().any(|file| dir.join(file).is_file());
    has_children || has_workspace_layout
}

/// Pick and check the project folder for a run. See the module documentation.
pub fn choose_project_folder(
    request: &ProjectRequest<'_>,
) -> Result<ProjectChoice, ProjectFolderError> {
    let parent_workspace = soft_resolve(request.parent_workspace);
    let parent_project = soft_resolve(request.parent_project);

    let chosen = non_empty(request.requested)
        .or_else(|| non_empty(request.agent_default))
        .map(PathBuf::from)
        .or_else(|| (parent_project != parent_workspace).then(|| parent_project.clone()))
        .ok_or(ProjectFolderError::NoProjectFolder)?;

    if !chosen.is_absolute() {
        return Err(ProjectFolderError::NotAbsolute(chosen));
    }
    if !chosen.is_dir() {
        return Err(ProjectFolderError::NotAFolder(chosen));
    }
    let cwd = soft_resolve(&chosen);

    if request.parent_restricted && !cwd.starts_with(&parent_project) {
        return Err(ProjectFolderError::OutsideParentProject {
            cwd,
            project: parent_project,
        });
    }
    if let Some(home) = home_containing(&cwd, &parent_workspace) {
        return Err(ProjectFolderError::InsideHome { cwd, home });
    }

    let scan = homes_inside(&cwd);
    let mut denied_roots = vec![parent_workspace.clone()];
    for home in &scan.homes {
        if !denied_roots.contains(home) {
            denied_roots.push(home.clone());
        }
    }

    let mut warnings = Vec::new();
    if !scan.homes.is_empty() && request.child_shell_enabled {
        warnings.push(format!(
            "the project folder contains rust-bot home(s) ({}). The agent's file tools cannot \
             read them, but its shell tool is not confined and could; disable shell for this \
             agent (tools.exec.enable false in its overlay) if that matters",
            scan.homes
                .iter()
                .map(|home| home.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if scan.truncated {
        warnings.push(format!(
            "the project folder is very large; rust-bot homes deeper than {SCAN_MAX_DEPTH} \
             levels, or past the first {SCAN_MAX_FOLDERS} folders, were not looked for"
        ));
    }

    Ok(ProjectChoice {
        cwd,
        denied_roots,
        warnings,
    })
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// The home that `cwd` is, or lies inside: the parent's own, or the nearest
/// ancestor (itself included) that looks like any rust-bot home.
fn home_containing(cwd: &Path, parent_workspace: &Path) -> Option<PathBuf> {
    if cwd.starts_with(parent_workspace) {
        return Some(parent_workspace.to_path_buf());
    }
    cwd.ancestors()
        .find(|ancestor| is_rust_bot_home(ancestor))
        .map(Path::to_path_buf)
}

/// The homes found below a folder.
#[derive(Debug, Default)]
struct HomeScan {
    homes: Vec<PathBuf>,
    /// The scan stopped early (depth or size limit).
    truncated: bool,
}

/// Look for rust-bot homes below `root`, without following symlinks and without
/// entering a home that was found (everything below it is denied as a whole).
fn homes_inside(root: &Path) -> HomeScan {
    let mut scan = HomeScan::default();
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((folder, depth)) = stack.pop() {
        visited += 1;
        if visited > SCAN_MAX_FOLDERS {
            scan.truncated = true;
            break;
        }
        if depth > 0 && is_rust_bot_home(&folder) {
            scan.homes.push(folder);
            continue;
        }
        if depth >= SCAN_MAX_DEPTH {
            scan.truncated = true;
            continue;
        }
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if SCAN_SKIPPED_FOLDERS.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    scan.homes.sort();
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make `dir` look like a rust-bot home.
    fn make_home(dir: &Path) {
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::create_dir_all(dir.join("sessions")).unwrap();
        fs::write(dir.join("SOUL.md"), "soul").unwrap();
    }

    /// A parent home at `<root>/home` and an unrelated project at `<root>/project`.
    struct Layout {
        _root: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
    }

    fn layout() -> Layout {
        let root = tempfile::tempdir().unwrap();
        let base = soft_resolve(root.path());
        let home = base.join("home");
        let project = base.join("project");
        make_home(&home);
        fs::create_dir_all(&project).unwrap();
        Layout {
            _root: root,
            home,
            project,
        }
    }

    fn request<'a>(layout: &'a Layout, requested: Option<&'a str>) -> ProjectRequest<'a> {
        ProjectRequest {
            requested,
            agent_default: None,
            // By default the parent has no separate project folder: it works in its home.
            parent_project: &layout.home,
            parent_restricted: false,
            parent_workspace: &layout.home,
            child_shell_enabled: false,
        }
    }

    fn requested_path(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    // ── accepted ────────────────────────────────────────────────────────────

    #[test]
    fn a_full_access_parent_may_hand_out_any_existing_folder() {
        let layout = layout();
        let path = requested_path(&layout.project);
        let choice = choose_project_folder(&request(&layout, Some(&path))).unwrap();
        assert_eq!(choice.cwd, layout.project);
        assert_eq!(choice.denied_roots, vec![layout.home.clone()]);
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn a_restricted_parent_may_hand_out_a_folder_inside_its_project() {
        let layout = layout();
        let inner = layout.project.join("src");
        fs::create_dir_all(&inner).unwrap();
        let path = requested_path(&inner);
        let mut asked = request(&layout, Some(&path));
        asked.parent_project = &layout.project;
        asked.parent_restricted = true;

        let choice = choose_project_folder(&asked).unwrap();

        assert_eq!(choice.cwd, inner);
    }

    #[test]
    fn the_agents_default_is_used_when_nothing_is_passed() {
        let layout = layout();
        let default = requested_path(&layout.project);
        let mut asked = request(&layout, None);
        asked.agent_default = Some(&default);
        assert_eq!(choose_project_folder(&asked).unwrap().cwd, layout.project);
    }

    #[test]
    fn a_passed_folder_wins_over_the_default() {
        let layout = layout();
        let other = layout.project.join("other");
        fs::create_dir_all(&other).unwrap();
        let default = requested_path(&layout.project);
        let passed = requested_path(&other);
        let mut asked = request(&layout, Some(&passed));
        asked.agent_default = Some(&default);
        assert_eq!(choose_project_folder(&asked).unwrap().cwd, other);
    }

    #[test]
    fn a_parent_with_a_separate_project_hands_it_on_when_nothing_is_passed() {
        let layout = layout();
        let mut asked = request(&layout, None);
        asked.parent_project = &layout.project;
        assert_eq!(choose_project_folder(&asked).unwrap().cwd, layout.project);
    }

    // ── rejected ────────────────────────────────────────────────────────────

    #[test]
    fn no_folder_at_all_asks_for_one_instead_of_using_the_parents_home() {
        let layout = layout();
        let error = choose_project_folder(&request(&layout, None)).unwrap_err();
        assert_eq!(error, ProjectFolderError::NoProjectFolder);
        assert!(error.to_string().contains("pass `cwd`"));
        // An empty string counts as nothing.
        let error = choose_project_folder(&request(&layout, Some("  "))).unwrap_err();
        assert_eq!(error, ProjectFolderError::NoProjectFolder);
    }

    #[test]
    fn a_relative_or_missing_folder_is_rejected() {
        let layout = layout();
        assert!(matches!(
            choose_project_folder(&request(&layout, Some("some/where"))),
            Err(ProjectFolderError::NotAbsolute(_))
        ));
        let missing = requested_path(&layout.project.join("nope"));
        assert!(matches!(
            choose_project_folder(&request(&layout, Some(&missing))),
            Err(ProjectFolderError::NotAFolder(_))
        ));
        // A file is not a folder.
        let file = layout.project.join("file.txt");
        fs::write(&file, "x").unwrap();
        assert!(matches!(
            choose_project_folder(&request(&layout, Some(&requested_path(&file)))),
            Err(ProjectFolderError::NotAFolder(_))
        ));
    }

    #[test]
    fn a_restricted_parent_cannot_hand_out_a_folder_outside_its_project() {
        let layout = layout();
        let sibling = layout.project.parent().unwrap().join("sibling");
        fs::create_dir_all(&sibling).unwrap();
        let path = requested_path(&sibling);
        let mut asked = request(&layout, Some(&path));
        asked.parent_project = &layout.project;
        asked.parent_restricted = true;

        assert!(matches!(
            choose_project_folder(&asked),
            Err(ProjectFolderError::OutsideParentProject { .. })
        ));
        // A `..` detour out of the project is outside too.
        let detour = requested_path(&layout.project.join("..").join("sibling"));
        let mut asked = request(&layout, Some(&detour));
        asked.parent_project = &layout.project;
        asked.parent_restricted = true;
        assert!(matches!(
            choose_project_folder(&asked),
            Err(ProjectFolderError::OutsideParentProject { .. })
        ));
    }

    #[test]
    fn the_parents_home_and_folders_inside_it_are_rejected() {
        let layout = layout();
        let inner = layout.home.join("memory");
        for target in [layout.home.clone(), inner] {
            let path = requested_path(&target);
            let error = choose_project_folder(&request(&layout, Some(&path))).unwrap_err();
            assert!(
                matches!(error, ProjectFolderError::InsideHome { .. }),
                "{error}"
            );
        }
    }

    #[test]
    fn a_sibling_childs_home_is_rejected() {
        let layout = layout();
        let child = layout.home.join("acp").join("agents").join("code-review");
        make_home(&child);
        let path = requested_path(&child);
        let error = choose_project_folder(&request(&layout, Some(&path))).unwrap_err();
        assert!(
            matches!(error, ProjectFolderError::InsideHome { .. }),
            "{error}"
        );
    }

    #[test]
    fn any_other_rust_bot_home_is_rejected_even_when_it_is_not_the_parents() {
        let layout = layout();
        let stranger = layout.project.join("somebody-elses-workspace");
        make_home(&stranger);
        let inside = stranger.join("memory");
        for target in [stranger.clone(), inside] {
            let path = requested_path(&target);
            let error = choose_project_folder(&request(&layout, Some(&path))).unwrap_err();
            assert!(
                matches!(error, ProjectFolderError::InsideHome { .. }),
                "{error}"
            );
        }
    }

    // ── homes inside the folder ─────────────────────────────────────────────

    #[test]
    fn a_folder_that_contains_a_home_is_allowed_and_the_home_is_denied() {
        let layout = layout();
        let nested = layout.project.join(".rust-bot").join("workspace");
        make_home(&nested);
        let path = requested_path(&layout.project);

        let choice = choose_project_folder(&request(&layout, Some(&path))).unwrap();

        assert_eq!(choice.cwd, layout.project);
        assert!(choice.denied_roots.contains(&layout.home));
        assert!(choice.denied_roots.contains(&nested));
        assert_eq!(choice.denied_roots.len(), 2);
    }

    #[test]
    fn the_parents_own_home_inside_the_project_is_denied_once() {
        let layout = layout();
        // The parent's home lives inside the project, as in `--workspace ./.rust-bot/workspace`.
        let home_in_project = layout.project.join(".rust-bot").join("workspace");
        make_home(&home_in_project);
        let path = requested_path(&layout.project);
        let asked = ProjectRequest {
            requested: Some(&path),
            agent_default: None,
            parent_project: &layout.project,
            parent_restricted: true,
            parent_workspace: &home_in_project,
            child_shell_enabled: false,
        };

        let choice = choose_project_folder(&asked).unwrap();

        assert_eq!(choice.denied_roots, vec![home_in_project]);
    }

    #[test]
    fn a_contained_home_warns_only_when_the_child_has_a_shell() {
        let layout = layout();
        make_home(&layout.project.join(".rust-bot").join("workspace"));
        let path = requested_path(&layout.project);

        let quiet = choose_project_folder(&request(&layout, Some(&path))).unwrap();
        assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);

        let mut with_shell = request(&layout, Some(&path));
        with_shell.child_shell_enabled = true;
        let warned = choose_project_folder(&with_shell).unwrap();
        assert_eq!(warned.warnings.len(), 1);
        assert!(
            warned.warnings[0].contains("shell"),
            "{:?}",
            warned.warnings
        );
    }

    #[test]
    fn a_home_inside_a_skipped_folder_is_not_scanned_for() {
        let layout = layout();
        make_home(&layout.project.join("node_modules").join("pkg").join("ws"));
        let path = requested_path(&layout.project);
        let choice = choose_project_folder(&request(&layout, Some(&path))).unwrap();
        assert_eq!(choice.denied_roots, vec![layout.home.clone()]);
    }

    #[test]
    fn the_scan_does_not_descend_into_a_home_it_found() {
        let layout = layout();
        let outer = layout.project.join("ws");
        make_home(&outer);
        make_home(&outer.join("acp").join("agents").join("inner"));
        let path = requested_path(&layout.project);
        let choice = choose_project_folder(&request(&layout, Some(&path))).unwrap();
        // The outer home is denied as a whole; the inner one is not listed again.
        assert_eq!(choice.denied_roots.len(), 2);
        assert!(choice.denied_roots.contains(&outer));
    }

    #[test]
    fn a_very_deep_home_is_not_found_and_the_operator_is_told() {
        let layout = layout();
        let mut deep = layout.project.clone();
        for level in 0..=SCAN_MAX_DEPTH {
            deep = deep.join(format!("d{level}"));
        }
        make_home(&deep);
        let path = requested_path(&layout.project);
        let choice = choose_project_folder(&request(&layout, Some(&path))).unwrap();
        assert!(!choice.denied_roots.contains(&deep));
        assert!(choice.warnings.iter().any(|w| w.contains("very large")));
    }

    // ── what counts as a home ───────────────────────────────────────────────

    #[test]
    fn ordinary_project_folders_are_not_homes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_rust_bot_home(dir.path()));
        // A web app with its own `memory/` and `sessions/` folders is not a home.
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::create_dir_all(dir.path().join("sessions")).unwrap();
        assert!(!is_rust_bot_home(dir.path()));
        fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
        assert!(is_rust_bot_home(dir.path()));
    }

    #[test]
    fn a_folder_with_child_agents_is_a_home() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("acp").join("agents")).unwrap();
        assert!(is_rust_bot_home(dir.path()));
    }
}
