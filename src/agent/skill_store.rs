//! Filesystem side of self-learnt skills: creating, patching and deleting the
//! skills the agent writes with the `skill_manage` tool.
//!
//! Skills live where [`crate::agent::skills::SkillsLoader`] finds them: the
//! workspace's `skills/<name>/SKILL.md` first, then the bundled skills folder.
//! Only the workspace is ever written. Changing a bundled skill first copies its
//! folder into the workspace, where the copy shadows the bundled one, so the
//! bundled files stay as shipped.
//!
//! Every operation validates before it writes, writes atomically, and returns
//! an error text ready to hand back to the model.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::agent::skill_operation::SkillOperation;
use crate::agent::skills::{BUILTIN_SKILLS_DIR, parse_skill_frontmatter};
use crate::agent::tools::filesystem::{find_and_replace, is_path_under};
use crate::config::schema::SkillsToolConfig;
use crate::utils::fs::write_atomic;

/// File holding a skill's instructions, at the root of its folder.
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// Longest accepted skill name.
pub const MAX_SKILL_NAME_CHARS: usize = 64;

/// Sub-folders of a skill that may hold supporting files.
pub const SUPPORTING_FILE_DIRS: [&str; 4] = ["references", "templates", "scripts", "assets"];

/// Name of the `skill_manage` parameter holding the text a patch looks for.
const OLD_STRING_PARAM: &str = "old_string";

/// Size limits a skill must respect, taken from `tools.skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillLimits {
    /// Limit, in characters, of a `SKILL.md` and of each supporting file.
    pub max_skill_chars: usize,
    /// Limit, in characters, of the frontmatter `description`.
    pub max_description_chars: usize,
}

impl From<&SkillsToolConfig> for SkillLimits {
    fn from(config: &SkillsToolConfig) -> Self {
        Self {
            max_skill_chars: config.max_skill_chars,
            max_description_chars: config.max_description_chars,
        }
    }
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self::from(&SkillsToolConfig::default())
    }
}

/// What a successful operation did, for the tool's answer to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillChange {
    /// One sentence saying what changed.
    pub message: String,
    /// The file (or, for a delete, the folder) that changed.
    pub path: PathBuf,
}

/// Where an existing skill was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillSource {
    /// `<workspace>/skills`: writable.
    Workspace,
    /// The bundled skills folder: read-only, copied before a change.
    Builtin,
}

/// An existing skill: its folder and where that folder lives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocatedSkill {
    directory: PathBuf,
    source: SkillSource,
}

/// A validated change, worked out by [`SkillStore::plan`] but not yet written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedChange {
    /// Write `content` to `relative_target` in the skill's workspace folder.
    /// `located` is `None` for a new skill.
    Write {
        name: String,
        located: Option<LocatedSkill>,
        relative_target: PathBuf,
        content: String,
        message: String,
    },
    /// Remove the supporting file `relative_target` (given as `file_path`).
    RemoveFile {
        name: String,
        located: LocatedSkill,
        relative_target: PathBuf,
        file_path: String,
        message: String,
    },
    /// Remove the whole workspace skill folder.
    Delete {
        name: String,
        located: LocatedSkill,
        message: String,
    },
}

impl PlannedChange {
    /// The sentence describing the change once made.
    fn message(&self) -> &str {
        match self {
            PlannedChange::Write { message, .. }
            | PlannedChange::RemoveFile { message, .. }
            | PlannedChange::Delete { message, .. } => message,
        }
    }
}

/// Creates, changes and deletes skills under the workspace's `skills` folder.
pub struct SkillStore {
    workspace_skills: PathBuf,
    builtin_skills: PathBuf,
    limits: SkillLimits,
    /// Held across each read-modify-write, because gateway sessions share one
    /// store and two patches of the same file must not interleave.
    write_lock: Mutex<()>,
}

impl SkillStore {
    /// A store writing to `<workspace>/skills`, reading bundled skills from
    /// `builtin_skills_dir` (the loader's default when `None`).
    pub fn new(workspace: &Path, builtin_skills_dir: Option<PathBuf>, limits: SkillLimits) -> Self {
        Self {
            workspace_skills: workspace.join("skills"),
            builtin_skills: builtin_skills_dir.unwrap_or_else(|| BUILTIN_SKILLS_DIR.clone()),
            limits,
            write_lock: Mutex::new(()),
        }
    }

    /// The folder new and changed skills are written to.
    pub fn workspace_skills_dir(&self) -> &Path {
        &self.workspace_skills
    }

    /// Create the skill `name` from the full `SKILL.md` text `content`.
    ///
    /// Fails when a workspace or bundled skill of that name exists already.
    pub fn create(&self, name: &str, content: &str) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::Create {
            name: name.to_string(),
            content: content.to_string(),
        })
    }

    /// Replace the whole `SKILL.md` of the existing skill `name` with `content`.
    pub fn rewrite(&self, name: &str, content: &str) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::Rewrite {
            name: name.to_string(),
            content: content.to_string(),
        })
    }

    /// Replace `old_string` with `new_string` in the skill's `SKILL.md`, or in
    /// the supporting file `file_path` when given.
    ///
    /// The text must match once (or `replace_all`). A patched `SKILL.md` must
    /// still be a valid skill document.
    pub fn patch(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        file_path: Option<&str>,
        replace_all: bool,
    ) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::Patch {
            name: name.to_string(),
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
            file_path: file_path.map(str::to_string),
            replace_all,
        })
    }

    /// Add or overwrite the supporting file `file_path` of the skill `name`.
    pub fn write_file(
        &self,
        name: &str,
        file_path: &str,
        file_content: &str,
    ) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::WriteFile {
            name: name.to_string(),
            file_path: file_path.to_string(),
            file_content: file_content.to_string(),
        })
    }

    /// Remove the supporting file `file_path` from the skill `name`. A folder
    /// left empty is removed too.
    pub fn remove_file(&self, name: &str, file_path: &str) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::RemoveFile {
            name: name.to_string(),
            file_path: file_path.to_string(),
        })
    }

    /// Delete the workspace skill `name` with all its files. Bundled skills
    /// cannot be deleted.
    pub fn delete(&self, name: &str) -> Result<SkillChange, String> {
        self.apply(&SkillOperation::Delete {
            name: name.to_string(),
        })
    }

    /// Validate `operation` and carry it out.
    pub fn apply(&self, operation: &SkillOperation) -> Result<SkillChange, String> {
        let _guard = self.lock();
        let planned = self.plan(operation)?;
        self.execute(planned)
    }

    /// Dry run of [`Self::apply`]: validate `operation` against the skills as
    /// they are now (for a patch, compute the patched text) without writing
    /// anything. Returns what applying it would report.
    pub fn check(&self, operation: &SkillOperation) -> Result<SkillChange, String> {
        let _guard = self.lock();
        let planned = self.plan(operation)?;
        Ok(SkillChange {
            message: planned.message().to_string(),
            path: self.planned_path(&planned),
        })
    }

    /// Validate `operation` and work out exactly what it would change.
    /// Reads the skills but never writes. Callers hold the write lock.
    fn plan(&self, operation: &SkillOperation) -> Result<PlannedChange, String> {
        validate_skill_name(operation.skill_name())?;
        match operation {
            SkillOperation::Create { name, content } => self.plan_create(name, content),
            SkillOperation::Rewrite { name, content } => self.plan_rewrite(name, content),
            SkillOperation::Patch {
                name,
                old_string,
                new_string,
                file_path,
                replace_all,
            } => self.plan_patch(
                name,
                old_string,
                new_string,
                file_path.as_deref(),
                *replace_all,
            ),
            SkillOperation::WriteFile {
                name,
                file_path,
                file_content,
            } => self.plan_write_file(name, file_path, file_content),
            SkillOperation::RemoveFile { name, file_path } => {
                self.plan_remove_file(name, file_path)
            }
            SkillOperation::Delete { name } => self.plan_delete(name),
        }
    }

    /// Plan a `create`: a valid document for a name nobody uses yet.
    fn plan_create(&self, name: &str, content: &str) -> Result<PlannedChange, String> {
        let content = strip_byte_order_mark(content);
        validate_skill_document(name, content, &self.limits)?;
        if let Some(existing) = self.locate(name) {
            return Err(format!(
                "Error: a skill named '{name}' already exists at {}. \
                 Use action='patch' to change it.",
                existing.directory.display()
            ));
        }
        let skill_dir = self.workspace_skills.join(name);
        if directory_has_entries(&skill_dir) {
            return Err(format!(
                "Error: cannot create skill '{name}': {} already exists and is not an \
                 empty skill folder. Choose another name.",
                skill_dir.display()
            ));
        }
        Ok(PlannedChange::Write {
            name: name.to_string(),
            located: None,
            relative_target: PathBuf::from(SKILL_FILE_NAME),
            content: content.to_string(),
            message: format!("Skill '{name}' created."),
        })
    }

    /// Plan a full `SKILL.md` rewrite of an existing skill.
    fn plan_rewrite(&self, name: &str, content: &str) -> Result<PlannedChange, String> {
        let content = strip_byte_order_mark(content);
        validate_skill_document(name, content, &self.limits)?;
        let located = self.locate_or_error(name)?;
        Ok(PlannedChange::Write {
            name: name.to_string(),
            located: Some(located),
            relative_target: PathBuf::from(SKILL_FILE_NAME),
            content: content.to_string(),
            message: format!("Skill '{name}' rewritten."),
        })
    }

    /// Plan a find-and-replace, computing the patched text and checking it.
    fn plan_patch(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        file_path: Option<&str>,
        replace_all: bool,
    ) -> Result<PlannedChange, String> {
        if old_string.is_empty() {
            return Err("Error: old_string must not be empty.".to_string());
        }
        let relative_target = match file_path.filter(|path| !is_skill_file_path(path)) {
            Some(path) => validate_supporting_path(path)?,
            None => PathBuf::from(SKILL_FILE_NAME),
        };
        let target_label = relative_target.to_string_lossy().replace('\\', "/");

        let located = self.locate_or_error(name)?;
        let current_path = located.directory.join(&relative_target);
        if !is_path_under(&current_path, &located.directory) {
            return Err(format!(
                "Error: '{target_label}' is outside skill '{name}'."
            ));
        }
        let current = fs::read_to_string(&current_path)
            .map_err(|_| format!("Error: file '{target_label}' not found in skill '{name}'."))?;
        let replacement = find_and_replace(
            &current,
            old_string,
            new_string,
            replace_all,
            &target_label,
            OLD_STRING_PARAM,
        )?;
        validate_size(&replacement.text, &target_label, &self.limits)?;
        if relative_target == Path::new(SKILL_FILE_NAME) {
            validate_skill_document(name, &replacement.text, &self.limits)
                .map_err(|error| format!("Error: the patch would break SKILL.md. {error}"))?;
        }
        let plural = if replacement.count == 1 { "" } else { "s" };
        Ok(PlannedChange::Write {
            name: name.to_string(),
            located: Some(located),
            relative_target,
            content: replacement.text,
            message: format!(
                "Patched {target_label} in skill '{name}' ({} replacement{plural}).",
                replacement.count
            ),
        })
    }

    /// Plan adding or overwriting a supporting file of an existing skill.
    fn plan_write_file(
        &self,
        name: &str,
        file_path: &str,
        file_content: &str,
    ) -> Result<PlannedChange, String> {
        let relative_target = validate_supporting_path(file_path)?;
        validate_size(file_content, file_path, &self.limits)?;
        let located = self.locate_or_error(name)?;
        Ok(PlannedChange::Write {
            name: name.to_string(),
            located: Some(located),
            relative_target,
            content: file_content.to_string(),
            message: format!("File '{file_path}' written to skill '{name}'."),
        })
    }

    /// Plan removing an existing supporting file.
    fn plan_remove_file(&self, name: &str, file_path: &str) -> Result<PlannedChange, String> {
        let relative_target = validate_supporting_path(file_path)?;
        let located = self.locate_or_error(name)?;
        if !located.directory.join(&relative_target).is_file() {
            let available = list_supporting_files(&located.directory);
            let hint = if available.is_empty() {
                "The skill has no supporting files.".to_string()
            } else {
                format!("Available files: {}.", available.join(", "))
            };
            return Err(format!(
                "Error: file '{file_path}' not found in skill '{name}'. {hint}"
            ));
        }
        Ok(PlannedChange::RemoveFile {
            name: name.to_string(),
            located,
            relative_target,
            file_path: file_path.to_string(),
            message: format!("File '{file_path}' removed from skill '{name}'."),
        })
    }

    /// Plan deleting a workspace skill; bundled skills cannot be deleted.
    fn plan_delete(&self, name: &str) -> Result<PlannedChange, String> {
        let located = self.locate_or_error(name)?;
        if located.source == SkillSource::Builtin {
            return Err(format!(
                "Error: '{name}' is a bundled skill and cannot be deleted."
            ));
        }
        Ok(PlannedChange::Delete {
            name: name.to_string(),
            located,
            message: format!("Skill '{name}' deleted."),
        })
    }

    /// Carry out a planned change. Callers hold the write lock.
    fn execute(&self, planned: PlannedChange) -> Result<SkillChange, String> {
        match planned {
            PlannedChange::Write {
                name,
                located,
                relative_target,
                content,
                message,
            } => {
                let skill_dir = match &located {
                    Some(located) => self.make_writable(&name, located)?,
                    None => self.workspace_skills.join(&name),
                };
                let target = skill_dir.join(&relative_target);
                self.write_inside(&skill_dir, &target, &content)?;
                Ok(SkillChange {
                    message,
                    path: target,
                })
            }
            PlannedChange::RemoveFile {
                name,
                located,
                relative_target,
                file_path,
                message,
            } => {
                let skill_dir = self.make_writable(&name, &located)?;
                let target = skill_dir.join(&relative_target);
                if !is_path_under(&target, &skill_dir) {
                    return Err(format!("Error: '{file_path}' is outside skill '{name}'."));
                }
                fs::remove_file(&target)
                    .map_err(|error| format!("Error: could not remove '{file_path}': {error}"))?;
                if let Some(parent) = target.parent() {
                    remove_empty_folders_up_to(parent, &skill_dir);
                }
                Ok(SkillChange {
                    message,
                    path: target,
                })
            }
            PlannedChange::Delete {
                name,
                located,
                message,
            } => {
                fs::remove_dir_all(&located.directory)
                    .map_err(|error| format!("Error: could not delete skill '{name}': {error}"))?;
                Ok(SkillChange {
                    message,
                    path: located.directory,
                })
            }
        }
    }

    /// The path a planned change touches. Writes always land in the
    /// workspace, even for a bundled skill (which is copied there first).
    fn planned_path(&self, planned: &PlannedChange) -> PathBuf {
        match planned {
            PlannedChange::Write {
                name,
                relative_target,
                ..
            }
            | PlannedChange::RemoveFile {
                name,
                relative_target,
                ..
            } => self.workspace_skills.join(name).join(relative_target),
            PlannedChange::Delete { located, .. } => located.directory.clone(),
        }
    }

    /// Take the write lock; a poisoned lock is still usable (it guards no data).
    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The skill `name`, workspace first, as the loader resolves it.
    fn locate(&self, name: &str) -> Option<LocatedSkill> {
        [
            (&self.workspace_skills, SkillSource::Workspace),
            (&self.builtin_skills, SkillSource::Builtin),
        ]
        .into_iter()
        .map(|(root, source)| LocatedSkill {
            directory: root.join(name),
            source,
        })
        .find(|located| located.directory.join(SKILL_FILE_NAME).is_file())
    }

    /// [`Self::locate`], or the error telling the model to create it first.
    fn locate_or_error(&self, name: &str) -> Result<LocatedSkill, String> {
        self.locate(name).ok_or_else(|| {
            format!("Error: skill '{name}' not found. Create it first with action='create'.")
        })
    }

    /// The workspace folder of `located`, copying a bundled skill there first.
    fn make_writable(&self, name: &str, located: &LocatedSkill) -> Result<PathBuf, String> {
        if located.source == SkillSource::Workspace {
            return Ok(located.directory.clone());
        }
        let copy = self.workspace_skills.join(name);
        copy_folder(&located.directory, &copy).map_err(|error| {
            format!("Error: could not copy bundled skill '{name}' into the workspace: {error}")
        })?;
        Ok(copy)
    }

    /// Write `content` to `target` atomically, refusing a target that resolves
    /// outside `skill_dir` (e.g. through a symlink).
    fn write_inside(&self, skill_dir: &Path, target: &Path, content: &str) -> Result<(), String> {
        if !is_path_under(target, skill_dir) {
            return Err(format!(
                "Error: {} is outside the skill folder {}.",
                target.display(),
                skill_dir.display()
            ));
        }
        write_atomic(target, content.as_bytes())
            .map_err(|error| format!("Error: could not write {}: {error}", target.display()))
    }
}

/// Check that `name` can be a skill folder: 1–64 characters of lowercase
/// letters, digits, `.`, `_` and `-`, starting with a letter or digit. This also
/// rules out path separators and `..`.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    const RULE: &str = "Use lowercase letters, digits, '.', '_' and '-', \
                        starting with a letter or digit.";
    if name.is_empty() {
        return Err("Error: a skill name is required.".to_string());
    }
    if name.chars().count() > MAX_SKILL_NAME_CHARS {
        return Err(format!(
            "Error: skill name '{name}' is longer than {MAX_SKILL_NAME_CHARS} characters."
        ));
    }
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let only_allowed_characters = name.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    });
    if starts_well && only_allowed_characters {
        Ok(())
    } else {
        Err(format!("Error: invalid skill name '{name}'. {RULE}"))
    }
}

/// Check a full `SKILL.md` text for the skill `name`: within the size limit,
/// a closed frontmatter block whose `name` equals `name`, a `description`
/// within its limit, and a non-empty body.
pub fn validate_skill_document(
    name: &str,
    content: &str,
    limits: &SkillLimits,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("Error: SKILL.md content must not be empty.".to_string());
    }
    validate_size(content, SKILL_FILE_NAME, limits)?;
    let frontmatter = parse_skill_frontmatter(content).ok_or_else(|| {
        "Error: SKILL.md must start with frontmatter: a '---' line, 'name:' and \
         'description:' lines, then a closing '---' line."
            .to_string()
    })?;
    let frontmatter_name = frontmatter
        .fields
        .get("name")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Error: the frontmatter must include 'name'.".to_string())?;
    if frontmatter_name != name {
        return Err(format!(
            "Error: the frontmatter name '{frontmatter_name}' must equal the skill name '{name}'."
        ));
    }
    let description = frontmatter
        .fields
        .get("description")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Error: the frontmatter must include 'description'.".to_string())?;
    let description_chars = description.chars().count();
    if description_chars > limits.max_description_chars {
        return Err(format!(
            "Error: the description is {description_chars} characters; the limit is {}. \
             Keep it to one trigger sentence and move detail into the body.",
            limits.max_description_chars
        ));
    }
    if frontmatter.body.trim().is_empty() {
        return Err("Error: SKILL.md needs instructions after the frontmatter.".to_string());
    }
    Ok(())
}

/// Check that `content` of the file `label` is within the size limit.
fn validate_size(content: &str, label: &str, limits: &SkillLimits) -> Result<(), String> {
    let chars = content.chars().count();
    if chars > limits.max_skill_chars {
        return Err(format!(
            "Error: {label} would be {chars} characters; the limit is {}. Split it into \
             a shorter SKILL.md and supporting files under references/ or templates/.",
            limits.max_skill_chars
        ));
    }
    Ok(())
}

/// Check a supporting-file path and return it as a relative path.
///
/// It must be relative, name a file below one of [`SUPPORTING_FILE_DIRS`],
/// and contain no `.`, `..` or empty segments. `\` counts as a separator.
pub fn validate_supporting_path(file_path: &str) -> Result<PathBuf, String> {
    let allowed = SUPPORTING_FILE_DIRS.join("/, ");
    let normalized = file_path.trim().replace('\\', "/");
    if normalized.is_empty() {
        return Err("Error: file_path is required.".to_string());
    }
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(format!(
            "Error: file_path '{file_path}' must be relative to the skill folder, \
             e.g. 'references/notes.md'."
        ));
    }
    let segments: Vec<&str> = normalized.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(format!(
            "Error: file_path '{file_path}' must not contain '.', '..' or empty segments."
        ));
    }
    if !SUPPORTING_FILE_DIRS.contains(&segments[0]) {
        return Err(format!(
            "Error: file_path '{file_path}' must start with one of: {allowed}/."
        ));
    }
    if segments.len() < 2 {
        return Err(format!(
            "Error: file_path '{file_path}' names a folder; give a file, e.g. '{}/notes.md'.",
            segments[0]
        ));
    }
    let relative: PathBuf = segments.iter().collect();
    // Belt and braces: the segment checks above leave only plain names.
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "Error: file_path '{file_path}' is not a plain relative path."
        ));
    }
    Ok(relative)
}

/// Whether `file_path` names the skill's own `SKILL.md` (bare or `./SKILL.md`).
fn is_skill_file_path(file_path: &str) -> bool {
    matches!(
        file_path.trim().trim_start_matches("./"),
        SKILL_FILE_NAME | ""
    )
}

/// `content` without a leading UTF-8 byte-order mark.
fn strip_byte_order_mark(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Whether `folder` exists and holds anything.
fn directory_has_entries(folder: &Path) -> bool {
    if folder.is_file() {
        return true;
    }
    fs::read_dir(folder).is_ok_and(|mut entries| entries.next().is_some())
}

/// The supporting files of the skill in `skill_dir`, as `/`-separated relative
/// paths, sorted.
fn list_supporting_files(skill_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for sub_folder in SUPPORTING_FILE_DIRS {
        collect_files(&skill_dir.join(sub_folder), skill_dir, &mut files);
    }
    files.sort();
    files
}

/// Append every file below `folder` to `files`, relative to `root`.
fn collect_files(folder: &Path, root: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, root, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Copy `source` with everything below it to `destination`. Symlinks are
/// skipped so a copy can never pull in files from outside the skill.
fn copy_folder(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_folder(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Remove `folder` and its parents while they are empty, stopping at `stop`
/// (which is never removed).
fn remove_empty_folders_up_to(folder: &Path, stop: &Path) {
    let mut current = folder.to_path_buf();
    while current != stop && current.starts_with(stop) {
        if fs::remove_dir(&current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skills::SkillsLoader;
    use tempfile::TempDir;

    /// A workspace and a bundled-skills folder, both empty.
    struct Fixture {
        workspace: TempDir,
        builtin: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                workspace: tempfile::tempdir().unwrap(),
                builtin: tempfile::tempdir().unwrap(),
            }
        }

        fn store(&self) -> SkillStore {
            self.store_with(SkillLimits::default())
        }

        fn store_with(&self, limits: SkillLimits) -> SkillStore {
            SkillStore::new(
                self.workspace.path(),
                Some(self.builtin.path().to_path_buf()),
                limits,
            )
        }

        fn loader(&self) -> SkillsLoader {
            SkillsLoader::new(
                self.workspace.path(),
                Some(self.builtin.path().to_path_buf()),
            )
        }

        fn workspace_skill(&self, name: &str) -> PathBuf {
            self.workspace.path().join("skills").join(name)
        }

        fn add_builtin_skill(&self, name: &str, body: &str) -> PathBuf {
            let folder = self.builtin.path().join(name);
            fs::create_dir_all(&folder).unwrap();
            fs::write(
                folder.join(SKILL_FILE_NAME),
                skill_document(name, "Bundled.", body),
            )
            .unwrap();
            folder
        }
    }

    fn skill_document(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
    }

    fn deploy_skill() -> String {
        skill_document(
            "deploy-app",
            "Use when deploying the app. Runs the release steps.",
            "## Steps\n1. Run cargo build --release\n2. Copy the binary",
        )
    }

    // ── validate_skill_name ───────────────────────────────────────────────

    #[test]
    fn skill_names_of_lowercase_digits_and_separators_are_valid() {
        for name in ["deploy", "deploy-app", "v2.release_notes", "9lives"] {
            assert!(validate_skill_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn skill_names_that_could_escape_or_break_the_folder_are_invalid() {
        for name in [
            "",
            "..",
            "../evil",
            "a/b",
            r"a\b",
            "Deploy",
            "-dash",
            ".hidden",
            "with space",
        ] {
            assert!(validate_skill_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn skill_names_longer_than_the_limit_are_invalid() {
        assert!(validate_skill_name(&"a".repeat(MAX_SKILL_NAME_CHARS)).is_ok());
        let error = validate_skill_name(&"a".repeat(MAX_SKILL_NAME_CHARS + 1)).unwrap_err();
        assert!(error.contains("longer than 64"), "{error}");
    }

    // ── validate_skill_document ───────────────────────────────────────────

    #[test]
    fn a_complete_skill_document_is_valid() {
        assert!(
            validate_skill_document("deploy-app", &deploy_skill(), &SkillLimits::default()).is_ok()
        );
    }

    #[test]
    fn a_document_without_frontmatter_is_invalid() {
        let error =
            validate_skill_document("x", "# Just a body\n", &SkillLimits::default()).unwrap_err();
        assert!(error.contains("must start with frontmatter"), "{error}");
    }

    #[test]
    fn a_document_with_an_unclosed_frontmatter_is_invalid() {
        let error = validate_skill_document(
            "x",
            "---\nname: x\ndescription: d\nbody",
            &SkillLimits::default(),
        )
        .unwrap_err();
        assert!(error.contains("must start with frontmatter"), "{error}");
    }

    #[test]
    fn a_document_missing_name_or_description_is_invalid() {
        let limits = SkillLimits::default();
        let no_name = "---\ndescription: d\n---\nbody\n";
        assert!(
            validate_skill_document("x", no_name, &limits)
                .unwrap_err()
                .contains("'name'")
        );
        let no_description = "---\nname: x\n---\nbody\n";
        assert!(
            validate_skill_document("x", no_description, &limits)
                .unwrap_err()
                .contains("'description'")
        );
    }

    #[test]
    fn a_frontmatter_name_different_from_the_skill_name_is_invalid() {
        let error =
            validate_skill_document("other", &deploy_skill(), &SkillLimits::default()).unwrap_err();
        assert!(
            error.contains("must equal the skill name 'other'"),
            "{error}"
        );
    }

    #[test]
    fn a_document_without_a_body_is_invalid() {
        let error = validate_skill_document(
            "x",
            "---\nname: x\ndescription: d\n---\n   \n",
            &SkillLimits::default(),
        )
        .unwrap_err();
        assert!(error.contains("needs instructions"), "{error}");
    }

    #[test]
    fn documents_and_descriptions_over_their_limits_are_invalid() {
        let limits = SkillLimits {
            max_skill_chars: 1_000,
            max_description_chars: 20,
        };
        let long_body = skill_document("x", "short", &"word ".repeat(300));
        assert!(
            validate_skill_document("x", &long_body, &limits)
                .unwrap_err()
                .contains("the limit is 1000")
        );
        let long_description = skill_document("x", &"d".repeat(21), "body");
        assert!(
            validate_skill_document("x", &long_description, &limits)
                .unwrap_err()
                .contains("the limit is 20")
        );
    }

    // ── validate_supporting_path ──────────────────────────────────────────

    #[test]
    fn supporting_paths_below_an_allowed_folder_are_valid() {
        assert_eq!(
            validate_supporting_path("references/api.md").unwrap(),
            PathBuf::from("references").join("api.md")
        );
        assert_eq!(
            validate_supporting_path(r"scripts\nested\run.sh").unwrap(),
            PathBuf::from("scripts").join("nested").join("run.sh")
        );
    }

    #[test]
    fn supporting_paths_outside_the_allowed_folders_are_invalid() {
        for path in [
            "",
            "notes.md",
            "references",
            "other/notes.md",
            "/etc/passwd",
            r"C:\Windows\x.md",
            "references/../SKILL.md",
            "references/./x.md",
            "references//x.md",
            "../references/x.md",
        ] {
            assert!(validate_supporting_path(path).is_err(), "{path:?}");
        }
    }

    // ── create ────────────────────────────────────────────────────────────

    #[test]
    fn create_writes_the_skill_and_the_loader_lists_it() {
        let fixture = Fixture::new();
        let change = fixture
            .store()
            .create("deploy-app", &deploy_skill())
            .unwrap();

        assert_eq!(
            change.path,
            fixture.workspace_skill("deploy-app").join(SKILL_FILE_NAME)
        );
        assert_eq!(fs::read_to_string(&change.path).unwrap(), deploy_skill());
        let summaries = fixture.loader().list_skill_summaries();
        assert!(summaries.iter().any(|summary| summary.name == "deploy-app"
            && summary.description == "Use when deploying the app. Runs the release steps."));
    }

    #[test]
    fn create_refuses_a_name_already_taken_by_a_workspace_or_bundled_skill() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let error = store.create("deploy-app", &deploy_skill()).unwrap_err();
        assert!(error.contains("already exists"), "{error}");

        fixture.add_builtin_skill("weather", "Look up the weather.");
        let error = store
            .create("weather", &skill_document("weather", "d", "body"))
            .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn create_refuses_an_invalid_document_and_writes_nothing() {
        let fixture = Fixture::new();
        let error = fixture.store().create("x", "no frontmatter").unwrap_err();
        assert!(error.starts_with("Error"), "{error}");
        assert!(!fixture.workspace_skill("x").exists());
    }

    #[test]
    fn create_strips_a_byte_order_mark() {
        let fixture = Fixture::new();
        let change = fixture
            .store()
            .create("deploy-app", &format!("\u{feff}{}", deploy_skill()))
            .unwrap();
        assert_eq!(fs::read_to_string(change.path).unwrap(), deploy_skill());
    }

    // ── rewrite ───────────────────────────────────────────────────────────

    #[test]
    fn rewrite_replaces_the_whole_document() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let new_document = skill_document("deploy-app", "New.", "New body.");
        let change = store.rewrite("deploy-app", &new_document).unwrap();
        assert_eq!(fs::read_to_string(change.path).unwrap(), new_document);
    }

    #[test]
    fn rewrite_of_a_missing_skill_says_to_create_it() {
        let fixture = Fixture::new();
        let error = fixture
            .store()
            .rewrite("ghost", &skill_document("ghost", "d", "b"))
            .unwrap_err();
        assert!(error.contains("action='create'"), "{error}");
    }

    // ── patch ─────────────────────────────────────────────────────────────

    #[test]
    fn patch_replaces_a_unique_match_in_skill_md() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let change = store
            .patch(
                "deploy-app",
                "Copy the binary",
                "Copy the binary to /opt/app",
                None,
                false,
            )
            .unwrap();
        assert!(
            change.message.contains("1 replacement)"),
            "{}",
            change.message
        );
        let text = fs::read_to_string(change.path).unwrap();
        assert!(text.contains("Copy the binary to /opt/app"));
    }

    #[test]
    fn patch_refuses_an_ambiguous_match_unless_replace_all() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let document = skill_document("dup", "d", "step\nstep");
        store.create("dup", &document).unwrap();
        let error = store.patch("dup", "step", "STEP", None, false).unwrap_err();
        assert!(error.contains("appears 2 times"), "{error}");
        let change = store.patch("dup", "step", "STEP", None, true).unwrap();
        assert!(
            change.message.contains("2 replacements"),
            "{}",
            change.message
        );
    }

    #[test]
    fn patch_that_misses_returns_the_closest_text() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        // The hint compares whole lines: two of the three lines match.
        let error = store
            .patch(
                "deploy-app",
                "## Steps\n1. Run cargo build --release\n2. Copy the binaries",
                "x",
                None,
                false,
            )
            .unwrap_err();
        assert!(
            error.contains("old_string not found in SKILL.md"),
            "{error}"
        );
        assert!(error.contains("Best match"), "{error}");
    }

    #[test]
    fn patch_that_would_break_the_frontmatter_is_refused_and_writes_nothing() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let error = store
            .patch(
                "deploy-app",
                "name: deploy-app",
                "name: renamed",
                None,
                false,
            )
            .unwrap_err();
        assert!(error.contains("would break SKILL.md"), "{error}");
        let skill_file = fixture.workspace_skill("deploy-app").join(SKILL_FILE_NAME);
        assert_eq!(fs::read_to_string(skill_file).unwrap(), deploy_skill());
    }

    #[test]
    fn patch_with_an_empty_old_string_is_refused() {
        let fixture = Fixture::new();
        let error = fixture
            .store()
            .patch("x", "", "y", None, false)
            .unwrap_err();
        assert!(error.contains("old_string must not be empty"), "{error}");
    }

    #[test]
    fn patch_of_a_supporting_file_edits_that_file() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        store
            .write_file("deploy-app", "references/hosts.md", "host: alpha\n")
            .unwrap();
        let change = store
            .patch(
                "deploy-app",
                "alpha",
                "beta",
                Some("references/hosts.md"),
                false,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(change.path).unwrap(), "host: beta\n");
    }

    #[test]
    fn patch_treats_a_skill_md_file_path_as_the_skill_itself() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let change = store
            .patch(
                "deploy-app",
                "Copy the binary",
                "Copy it",
                Some("SKILL.md"),
                false,
            )
            .unwrap();
        assert!(change.path.ends_with(SKILL_FILE_NAME));
    }

    // ── copy-on-write of bundled skills ───────────────────────────────────

    #[test]
    fn patching_a_bundled_skill_copies_it_into_the_workspace() {
        let fixture = Fixture::new();
        let bundled = fixture.add_builtin_skill("weather", "Use wttr.in.");
        fs::create_dir_all(bundled.join("references")).unwrap();
        fs::write(bundled.join("references").join("cities.md"), "Paris").unwrap();
        let original = fs::read_to_string(bundled.join(SKILL_FILE_NAME)).unwrap();

        let change = fixture
            .store()
            .patch(
                "weather",
                "Use wttr.in.",
                "Use wttr.in?format=3.",
                None,
                false,
            )
            .unwrap();

        let copy = fixture.workspace_skill("weather");
        assert_eq!(change.path, copy.join(SKILL_FILE_NAME));
        assert!(
            fs::read_to_string(&change.path)
                .unwrap()
                .contains("format=3")
        );
        assert_eq!(
            fs::read_to_string(copy.join("references").join("cities.md")).unwrap(),
            "Paris",
            "supporting files come along"
        );
        assert_eq!(
            fs::read_to_string(bundled.join(SKILL_FILE_NAME)).unwrap(),
            original,
            "the bundled skill is untouched"
        );
    }

    #[test]
    fn a_failed_patch_of_a_bundled_skill_leaves_no_copy() {
        let fixture = Fixture::new();
        fixture.add_builtin_skill("weather", "Use wttr.in.");
        assert!(
            fixture
                .store()
                .patch("weather", "not there", "x", None, false)
                .is_err()
        );
        assert!(!fixture.workspace_skill("weather").exists());
    }

    // ── write_file / remove_file ──────────────────────────────────────────

    #[test]
    fn write_file_adds_a_supporting_file() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let change = store
            .write_file(
                "deploy-app",
                "scripts/release.sh",
                "cargo build --release\n",
            )
            .unwrap();
        assert_eq!(
            change.path,
            fixture
                .workspace_skill("deploy-app")
                .join("scripts")
                .join("release.sh")
        );
        assert_eq!(
            fs::read_to_string(change.path).unwrap(),
            "cargo build --release\n"
        );
    }

    #[test]
    fn write_file_refuses_paths_outside_the_allowed_folders() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        for path in [
            "../escape.md",
            "SKILL.md",
            "notes.md",
            "references/../../x.md",
        ] {
            assert!(store.write_file("deploy-app", path, "x").is_err(), "{path}");
        }
    }

    #[test]
    fn write_file_refuses_content_over_the_limit() {
        let fixture = Fixture::new();
        let store = fixture.store_with(SkillLimits {
            max_skill_chars: 1_000,
            max_description_chars: 1_024,
        });
        store.create("deploy-app", &deploy_skill()).unwrap();
        let error = store
            .write_file("deploy-app", "references/big.md", &"x".repeat(1_001))
            .unwrap_err();
        assert!(error.contains("the limit is 1000"), "{error}");
    }

    #[test]
    fn write_file_to_a_missing_skill_says_to_create_it() {
        let fixture = Fixture::new();
        let error = fixture
            .store()
            .write_file("ghost", "references/a.md", "x")
            .unwrap_err();
        assert!(error.contains("action='create'"), "{error}");
    }

    #[test]
    fn remove_file_deletes_the_file_and_its_empty_folder() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        store
            .write_file("deploy-app", "references/hosts.md", "alpha")
            .unwrap();
        store
            .remove_file("deploy-app", "references/hosts.md")
            .unwrap();
        let skill_dir = fixture.workspace_skill("deploy-app");
        assert!(!skill_dir.join("references").exists());
        assert!(skill_dir.join(SKILL_FILE_NAME).exists());
    }

    #[test]
    fn remove_file_of_a_missing_file_lists_the_available_ones() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        store
            .write_file("deploy-app", "references/hosts.md", "alpha")
            .unwrap();
        let error = store
            .remove_file("deploy-app", "references/other.md")
            .unwrap_err();
        assert!(error.contains("references/hosts.md"), "{error}");
    }

    // ── delete ────────────────────────────────────────────────────────────

    #[test]
    fn delete_removes_a_workspace_skill() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        store.delete("deploy-app").unwrap();
        assert!(!fixture.workspace_skill("deploy-app").exists());
        assert!(fixture.loader().list_skill_summaries().is_empty());
    }

    #[test]
    fn delete_refuses_a_bundled_skill() {
        let fixture = Fixture::new();
        let bundled = fixture.add_builtin_skill("weather", "Use wttr.in.");
        let error = fixture.store().delete("weather").unwrap_err();
        assert!(error.contains("bundled skill"), "{error}");
        assert!(bundled.join(SKILL_FILE_NAME).exists());
    }

    #[test]
    fn delete_of_a_workspace_copy_brings_back_the_bundled_skill() {
        let fixture = Fixture::new();
        fixture.add_builtin_skill("weather", "Use wttr.in.");
        let store = fixture.store();
        store
            .patch("weather", "Use wttr.in.", "Changed.", None, false)
            .unwrap();
        store.delete("weather").unwrap();
        let loader = fixture.loader();
        let summaries = loader.list_skill_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].description, "Bundled.");
    }

    #[test]
    fn delete_of_a_missing_skill_is_an_error() {
        let fixture = Fixture::new();
        assert!(fixture.store().delete("ghost").is_err());
    }

    // ── check / apply ─────────────────────────────────────────────────────

    /// Every file below `folder`, relative and sorted, with its content.
    fn snapshot(folder: &Path) -> Vec<(String, String)> {
        let mut files = Vec::new();
        collect_files(folder, folder, &mut files);
        files.sort();
        files
            .into_iter()
            .map(|relative| {
                let content = fs::read_to_string(folder.join(&relative)).unwrap_or_default();
                (relative, content)
            })
            .collect()
    }

    #[test]
    fn check_validates_every_operation_without_touching_the_disk() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        store
            .write_file("deploy-app", "references/notes.md", "old notes")
            .unwrap();
        fixture.add_builtin_skill("weather", "Use wttr.in.");
        let skills_root = fixture.workspace.path().join("skills");
        let before = snapshot(&skills_root);

        let operations = [
            SkillOperation::Create {
                name: "fresh".into(),
                content: skill_document("fresh", "Use when testing.", "Step."),
            },
            SkillOperation::Rewrite {
                name: "deploy-app".into(),
                content: deploy_skill().replace("Copy the binary", "Upload"),
            },
            SkillOperation::Patch {
                name: "weather".into(),
                old_string: "wttr.in".into(),
                new_string: "open-meteo".into(),
                file_path: None,
                replace_all: false,
            },
            SkillOperation::WriteFile {
                name: "deploy-app".into(),
                file_path: "references/more.md".into(),
                file_content: "more".into(),
            },
            SkillOperation::RemoveFile {
                name: "deploy-app".into(),
                file_path: "references/notes.md".into(),
            },
            SkillOperation::Delete {
                name: "deploy-app".into(),
            },
        ];
        for operation in &operations {
            let change = store
                .check(operation)
                .unwrap_or_else(|e| panic!("{operation:?}: {e}"));
            assert!(!change.message.is_empty());
        }
        assert_eq!(snapshot(&skills_root), before);
        // The bundled skill was not copied into the workspace by the dry run.
        assert!(!fixture.workspace_skill("weather").exists());
    }

    #[test]
    fn check_reports_the_same_errors_as_apply() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.create("deploy-app", &deploy_skill()).unwrap();
        let missing_text = SkillOperation::Patch {
            name: "deploy-app".into(),
            old_string: "not in the skill".into(),
            new_string: "x".into(),
            file_path: None,
            replace_all: false,
        };
        let duplicate = SkillOperation::Create {
            name: "deploy-app".into(),
            content: deploy_skill(),
        };
        for operation in [missing_text, duplicate] {
            assert_eq!(
                store.check(&operation).unwrap_err(),
                store.apply(&operation).unwrap_err()
            );
        }
    }

    #[test]
    fn check_names_the_path_apply_writes() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let operation = SkillOperation::Create {
            name: "deploy-app".into(),
            content: deploy_skill(),
        };
        let checked = store.check(&operation).unwrap();
        let applied = store.apply(&operation).unwrap();
        assert_eq!(checked, applied);
        assert!(applied.path.is_file());
    }
}
