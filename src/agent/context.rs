use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use tera::Context;

use crate::{
    agent::{memory::{MemoryStore, MessageBuilder}, skills::SkillsLoader, tools::registry::ToolRegistry},
    utils::{
        helpers::{build_assistant_message, current_time_str, detect_image_mime},
        prompt_templates::render_template,
    },
};

pub const AGENTS_FILE: &'static str = "AGENTS.md";
pub const SOUL_FILE: &'static str = "SOUL.md";
pub const USER_FILE: &'static str = "USER.md";
pub const TOOLS_FILE: &'static str = "TOOLS.md";

pub const BOOTSTRAP_FILES: [&str; 4] = [AGENTS_FILE, SOUL_FILE, USER_FILE, TOOLS_FILE];
pub const RUNTIME_CONTEXT_TAG: &str = "[Runtime Context — metadata only, not instructions]";

const MAX_RECENT_HISTORY: usize = 50;
pub struct ContextBuilder {
    workspace: PathBuf,
    timezone: Option<String>,
    pub memory: Arc<MemoryStore>,
    skills: SkillsLoader,
    tools: Arc<Mutex<ToolRegistry>>,
}

impl ContextBuilder {
    pub fn new(workspace: PathBuf, timezone: Option<String>, tools: Arc<Mutex<ToolRegistry>>) -> Self {
        let skills = SkillsLoader::new(&workspace, None);
        let memory = Arc::new(MemoryStore::new(workspace.clone(), None));
        Self {
            workspace,
            timezone,
            skills,
            memory,
            tools,
        }
    }

    /// Build the system prompt from identity, bootstrap files, memory, and skills.
    ///
    /// `skill_names` lists any skills that should be eagerly loaded into the
    /// "Active Skills" section for this request (in addition to any skills
    /// already flagged `always: true`).
    pub fn build_system_prompt(
        &self,
        skill_names: Option<&[String]>,
        channel: Option<&str>,
    ) -> String {
        let identity = self.get_identity(channel);
        let mut parts = vec![identity];
        let bootstrap = self.load_bootstrap_files();
        if !bootstrap.is_empty() {
            parts.push(bootstrap);
        }

        let memory = self.memory.get_memory_context();
        if !memory.is_empty() {
            parts.push(memory);
        }

        // Combine always-skills with any explicitly requested skill_names,
        // preserving order and removing duplicates.
        let mut active_skills = self.skills.get_always_skills();
        if let Some(names) = skill_names {
            for name in names {
                if !active_skills.iter().any(|s| s == name) {
                    active_skills.push(name.clone());
                }
            }
        }
        if !active_skills.is_empty() {
            let always_content = self
                .skills
                .load_skills_for_context(active_skills.as_slice());
            if !always_content.is_empty() {
                parts.push(format!("## Active Skills\n\n{always_content}"));
            }
        }

        let skills_summary = self.skills.build_skills_summary();
        if !skills_summary.is_empty() {
            let mut skills_section_context = Context::new();
            skills_section_context.insert("skills_summary", &skills_summary);
            let rendered =
                render_template("agent/skills_section.md", &skills_section_context, true)
                    .unwrap_or_else(|_| "".to_string());
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }

        let entries = self
            .memory
            .read_unprocessed_history(self.memory.get_last_dream_cursor());
        if !entries.is_empty() {
            let capped = entries[entries.len().saturating_sub(MAX_RECENT_HISTORY)..].to_vec();
            let history_joined = capped
                .iter()
                .map(|e| {
                    format!(
                        "[{}] {}",
                        e["timestamp"].as_str().unwrap_or(""),
                        e["content"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<String>>()
                .join("\n");
            parts.push(format!("## Recent History\n\n{history_joined}"));
        }

        return parts.join("\n\n---\n\n");
    }

    /// Map `std::env::consts::OS` (lowercase) to a human-readable OS name.
    fn os_display_name(os: &str) -> &str {
        match os {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        }
    }

    /// Get the core identity section.
    fn get_identity(&self, channel: Option<&str>) -> String {
        let workspace_path = {
            let raw = self
                .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone())
            .to_string_lossy()
            .into_owned();
            // Strip Windows extended-length prefix ("\\?\") and normalise
            // backslashes to forward slashes so paths read naturally.
            let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
            stripped.replace('\\', "/")
        };
        let system = Self::os_display_name(std::env::consts::OS);
        // Use std::env or std::process::Command to detect architecture.
        // The Rust equivalent for platform.machine() is to use std::env::consts::ARCH.
        let machine = std::env::consts::ARCH;
        let runtime = format!("{system} {machine}");
        let mut platform_policy_context = Context::new();
        platform_policy_context.insert("system", &system);
        let platform_policy =
            render_template("agent/platform_policy.md", &platform_policy_context, true)
                .unwrap_or_else(|_| "".to_string());
        let mut id_context = Context::new();
        id_context.insert("workspace_path", &workspace_path);
        id_context.insert("runtime", &runtime);
        id_context.insert("platform_policy", platform_policy.as_str());
        id_context.insert("channel", &channel.unwrap_or("cli"));
        return render_template("agent/identity.md", &id_context, true)
            .unwrap_or_else(|_| "".to_string());
    }

    /// Build untrusted runtime metadata block for injection before the user message.
    ///
    /// Call this per-request and prepend the result as a system message before the
    /// user turn so the model always has current time and routing context.
    pub(crate) fn build_runtime_context(
        channel_option: Option<&str>,
        chat_id_option: Option<&str>,
        timezone: Option<&str>,
    ) -> String {
        let mut lines = vec![format!(
            "Current Time: {}",
            current_time_str(timezone)
        )];
        if let Some(channel) = channel_option
            && let Some(chat_id) = chat_id_option
        {
            if !channel.is_empty() && !chat_id.is_empty() {
                lines.push(format!("Channel: {}", channel));
                lines.push(format!("Chat ID: {}", chat_id));
            }
        }
        return format!("{RUNTIME_CONTEXT_TAG}\n{}", lines.join("\n"));
    }

    /// Guess an image MIME type from a file's extension. Used as a fallback when
    /// magic-byte detection (`detect_image_mime`) returns `None`.
    fn guess_image_mime_from_extension(path: &std::path::Path) -> Option<&'static str> {
        match path
            .extension()?
            .to_str()?
            .to_lowercase()
            .as_str()
        {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        }
    }

    /// Build user message content with optional base64-encoded images.
    ///
    /// When `media` is empty or `None`, returns a plain string `Value`.
    /// When images are present, returns a JSON array of image blocks followed
    /// by a `{"type": "text", "text": …}` block — matching the OpenAI
    /// multi-modal message format.
    fn build_user_content(
        &self,
        text: &str,
        media: Option<&[String]>,
    ) -> serde_json::Value {
        let paths = match media {
            Some(m) if !m.is_empty() => m,
            _ => return serde_json::Value::String(text.to_owned()),
        };

        let mut images: Vec<serde_json::Value> = Vec::new();
        for path_str in paths {
            let p = std::path::Path::new(path_str);
            if !p.is_file() {
                continue;
            }
            let raw = match std::fs::read(p) {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("build_user_content: could not read {:?}: {}", p, e);
                    continue;
                }
            };
            let mime = detect_image_mime(&raw)
                .or_else(|| Self::guess_image_mime_from_extension(p));
            let mime = match mime {
                Some(m) if m.starts_with("image/") => m,
                _ => continue,
            };
            let b64 = BASE64.encode(&raw);
            images.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{b64}") },
                "_meta": { "path": path_str },
            }));
        }

        if images.is_empty() {
            return serde_json::Value::String(text.to_owned());
        }
        images.push(serde_json::json!({"type": "text", "text": text}));
        serde_json::Value::Array(images)
    }

    fn merge_message_content(
        left: serde_json::Value,
        right: serde_json::Value,
    ) -> serde_json::Value {
        if left.is_string() && right.is_string() {
            if let Some(left_str) = left.as_str()
                && !left_str.is_empty()
            {
                return format!("{left_str}\n\n{}", right.as_str().unwrap_or("")).into();
            }
            return right.into();
        }
        fn to_blocks(value: serde_json::Value) -> Vec<serde_json::Value> {
            if value.is_array() {
                return value
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|item| {
                        if item.is_object() {
                            item.clone()
                        } else {
                            let text = item
                                .as_str()
                                .map(|s| s.to_owned())
                                .unwrap_or_else(|| item.to_string());
                            serde_json::json!({"type": "text", "text": text})
                        }
                    })
                    .collect();
            }
            if value.is_null() {
                return vec![];
            }
            let text = value
                .as_str()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| value.to_string());
            return vec![serde_json::json!({"type": "text", "text": text})];
        }
        let mut merged = to_blocks(left);
        merged.extend(to_blocks(right));
        return merged.into();
    }

    /// Load all bootstrap files from workspace.    
    fn load_bootstrap_files(&self) -> String {
        let mut parts: Vec<String> = vec![];
        for filename in BOOTSTRAP_FILES {
            let path = self.workspace.join(filename);
            if path.exists() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => parts.push(format!("## {filename}\n\n{}", content.trim())),
                    Err(e) => log::warn!("Failed to read bootstrap file {:?}: {}", path, e),
                }
            }
        }
        return parts.join("\n\n");
    }

    /// Add a tool result to the message list.
    fn add_tool_result(mut messages: Vec<serde_json::Value>, tool_call_id: &str, tool_name: &str, result: serde_json::Value) -> Vec<serde_json::Value> {
        messages.push(serde_json::json!({ "role": "tool", "tool_call_id": tool_call_id, "name": tool_name, "content": result }));
        return messages;
    }

    /// Append an assistant turn to `messages` and return the updated list.
    pub fn add_assistant_message(
        &self,
        mut messages: Vec<serde_json::Value>,
        content: Option<&str>,
        tool_calls: Option<Vec<serde_json::Value>>,
        reasoning_content: Option<&str>,
        thinking_blocks: Option<Vec<serde_json::Value>>,
    ) -> Vec<serde_json::Value> {
        messages.push(build_assistant_message(
            content,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        ));
        messages
    }
    
}

impl MessageBuilder for ContextBuilder {

    /// Build the complete message list for an LLM call.
    ///
    /// The runtime context (current time, channel routing) is merged into the
    /// current user turn rather than added as a separate message, avoiding
    /// consecutive same-role messages that some providers reject.
    fn build_messages(
        &self,
        history: &[serde_json::Value],
        current_message: &str,
        skill_names: Option<&[String]>,
        media: Option<&[String]>,
        channel: Option<&str>,
        chat_id: Option<&str>,
        current_role: &str,
    ) -> Vec<serde_json::Value> {
        let runtime_ctx =
            ContextBuilder::build_runtime_context(channel, chat_id, self.timezone.as_deref());
        let user_content = self.build_user_content(current_message, media);

        // Merge runtime context block and user content into a single value so
        // we never produce two consecutive messages with the same role.
        let merged: serde_json::Value = if let Some(text) = user_content.as_str() {
            serde_json::Value::String(format!("{runtime_ctx}\n\n{text}"))
        } else {
            // user_content is already an array of blocks; prepend the runtime tag block
            let mut blocks = vec![serde_json::json!({"type": "text", "text": runtime_ctx})];
            if let Some(arr) = user_content.as_array() {
                blocks.extend(arr.iter().cloned());
            }
            serde_json::Value::Array(blocks)
        };

        let system_content = self.build_system_prompt(skill_names, channel);
        let mut messages: Vec<serde_json::Value> = std::iter::once(
            serde_json::json!({"role": "system", "content": system_content}),
        )
        .chain(history.iter().cloned())
        .collect();

        // If the last message already has the same role, merge rather than append.
        if messages
            .last()
            .and_then(|m| m["role"].as_str())
            == Some(current_role)
        {
            if let Some(last) = messages.last_mut() {
                let existing_content = last["content"].take();
                last["content"] = Self::merge_message_content(existing_content, merged);
            }
        } else {
            messages.push(serde_json::json!({"role": current_role, "content": merged}));
        }

        messages
    }

    fn get_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_definitions()
    }
}

pub const DEFAULT_CURRENT_ROLE: &'static str = "user";

impl MessageBuilder for Arc<ContextBuilder> {
    fn build_messages(
        &self,
        history: &[serde_json::Value],
        current_message: &str,
        skill_names: Option<&[String]>,
        media: Option<&[String]>,
        channel: Option<&str>,
        chat_id: Option<&str>,
        current_role: &str,
    ) -> Vec<serde_json::Value> {
        self.as_ref().build_messages(
            history,
            current_message,
            skill_names,
            media,
            channel,
            chat_id,
            current_role,
        )
    }

    fn get_definitions(&self) -> Vec<serde_json::Value> {
        self.as_ref().get_definitions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_builder(tmp: &TempDir) -> ContextBuilder {
        ContextBuilder::new(tmp.path().to_path_buf(), None, Arc::new(Mutex::new(ToolRegistry::new())))
    }

    // ── os_display_name ───────────────────────────────────────────────────────

    #[test]
    fn os_display_name_macos() {
        assert_eq!(ContextBuilder::os_display_name("macos"), "macOS");
    }

    #[test]
    fn os_display_name_windows() {
        assert_eq!(ContextBuilder::os_display_name("windows"), "Windows");
    }

    #[test]
    fn os_display_name_linux() {
        assert_eq!(ContextBuilder::os_display_name("linux"), "Linux");
    }

    #[test]
    fn os_display_name_unknown_passthrough() {
        assert_eq!(ContextBuilder::os_display_name("freebsd"), "freebsd");
    }

    // ── get_identity ──────────────────────────────────────────────────────────

    #[test]
    fn get_identity_returns_non_empty_string() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(None);
        assert!(!result.is_empty(), "identity should not be empty");
    }

    #[test]
    fn get_identity_contains_workspace_path() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(None);
        // Normalise the expected path the same way get_identity does.
        let raw = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf())
            .to_string_lossy()
            .into_owned();
        println!("raw: {}", raw);
        let expected = raw.strip_prefix(r"\\?\").unwrap_or(&raw).replace('\\', "/");
        assert!(
            result.contains(&expected),
            "workspace path '{}' should appear in identity; got:\n{result}",
            expected
        );
    }

    #[test]
    fn get_identity_workspace_path_has_no_extended_prefix_or_backslashes() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(None);
        println!("result: {}", result);
        assert!(
            !result.contains(r"\\?\"),
            "path should not contain '\\\\?\\' extended prefix; got:\n{result}"
        );
        assert!(
            !result.contains('\\'),
            "path should use forward slashes only; got:\n{result}"
        );
    }

    #[test]
    fn get_identity_contains_runtime_line() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(None);
        let arch = std::env::consts::ARCH;
        assert!(
            result.contains(arch),
            "ARCH '{}' should appear in runtime; got:\n{result}",
            arch
        );
    }

    #[test]
    fn get_identity_channel_none_defaults_to_cli_format_hint() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(None);
        assert!(
            result.contains("terminal"),
            "cli channel should produce terminal format hint; got:\n{result}"
        );
    }

    #[test]
    fn get_identity_channel_telegram_produces_messaging_hint() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(Some("telegram"));
        assert!(
            result.contains("messaging app"),
            "telegram channel should produce messaging app hint; got:\n{result}"
        );
    }

    #[test]
    fn get_identity_channel_email_produces_email_hint() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.get_identity(Some("email"));
        assert!(
            result.contains("email"),
            "email channel should produce email hint; got:\n{result}"
        );
    }

    // ── build_runtime_context ────────────────────────────────────────────────

    #[test]
    fn build_runtime_context_contains_current_time_and_tag() {
        let result = ContextBuilder::build_runtime_context(None, None, None);
        assert!(
            result.contains("Current Time:"),
            "should contain Current Time"
        );
        assert!(
            result.contains(RUNTIME_CONTEXT_TAG),
            "should contain runtime context tag"
        );
    }

    #[test]
    fn build_runtime_context_channel_only_omits_channel_info() {
        // Requires both channel AND chat_id to emit either; channel-only is silently omitted.
        let result = ContextBuilder::build_runtime_context(Some("telegram"), None, None);
        assert!(
            !result.contains("Channel:"),
            "channel should be absent when chat_id is missing"
        );
    }

    #[test]
    fn build_runtime_context_both_channel_and_chat_id_appear() {
        let result =
            ContextBuilder::build_runtime_context(Some("telegram"), Some("12345"), None);
        assert!(
            result.contains("Channel: telegram"),
            "channel should appear"
        );
        assert!(result.contains("Chat ID: 12345"), "chat_id should appear");
    }

    #[test]
    fn build_runtime_context_empty_strings_suppressed() {
        let result = ContextBuilder::build_runtime_context(Some(""), Some("12345"), None);
        assert!(
            !result.contains("Channel:"),
            "empty channel should be suppressed"
        );
        assert!(
            !result.contains("Chat ID:"),
            "chat_id should be suppressed when channel is empty"
        );
    }

    #[test]
    fn build_runtime_context_uses_timezone_parameter() {
        let result =
            ContextBuilder::build_runtime_context(None, None, Some("Europe/London"));
        assert!(
            result.contains("Europe/London"),
            "timezone parameter should appear in time string"
        );
    }

    // ── build_user_content ───────────────────────────────────────────────────

    #[test]
    fn build_user_content_no_media_returns_plain_string() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.build_user_content("hello", None);
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn build_user_content_empty_media_returns_plain_string() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result = b.build_user_content("hello", Some(&[]));
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn build_user_content_nonexistent_file_falls_back_to_text() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let result =
            b.build_user_content("msg", Some(&["/no/such/file.png".to_string()]));
        assert_eq!(result, serde_json::json!("msg"));
    }

    #[test]
    fn build_user_content_non_image_file_skipped() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        // Write a text file — no magic bytes and non-image extension
        let txt = tmp.path().join("doc.txt");
        fs::write(&txt, b"just text").unwrap();
        let b = make_builder(&tmp);
        let result =
            b.build_user_content("msg", Some(&[txt.to_string_lossy().into_owned()]));
        // No valid image → falls back to plain string
        assert_eq!(result, serde_json::json!("msg"));
    }

    #[test]
    fn build_user_content_png_produces_image_url_block() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        // Minimal valid PNG magic bytes
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"rest of png data");
        let img_path = tmp.path().join("pic.png");
        fs::write(&img_path, &png).unwrap();
        let b = make_builder(&tmp);
        let result = b.build_user_content(
            "describe this",
            Some(&[img_path.to_string_lossy().into_owned()]),
        );
        let arr = result.as_array().expect("should be an array");
        assert_eq!(arr.len(), 2, "one image block + one text block");
        // Image block
        assert_eq!(arr[0]["type"], "image_url");
        let url = arr[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "URL prefix: {url}");
        // Text block is last
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr[1]["text"], "describe this");
    }

    #[test]
    fn build_user_content_meta_path_recorded() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"data");
        let img_path = tmp.path().join("img.png");
        fs::write(&img_path, &png).unwrap();
        let path_str = img_path.to_string_lossy().into_owned();
        let b = make_builder(&tmp);
        let result = b.build_user_content("text", Some(&[path_str.clone()]));
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["_meta"]["path"], path_str);
    }

    #[test]
    fn build_user_content_extension_fallback_jpeg() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        // Unknown magic bytes but .jpg extension → guessed as image/jpeg
        let img_path = tmp.path().join("photo.jpg");
        fs::write(&img_path, b"\x00\x01\x02\x03fake jpeg body").unwrap();
        let b = make_builder(&tmp);
        let result = b.build_user_content(
            "text",
            Some(&[img_path.to_string_lossy().into_owned()]),
        );
        let arr = result.as_array().expect("should produce image block via extension");
        let url = arr[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "URL: {url}");
    }

    #[test]
    fn build_user_content_multiple_images_all_included() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        let png_magic = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let p1 = tmp.path().join("a.png");
        let p2 = tmp.path().join("b.png");
        fs::write(&p1, &png_magic).unwrap();
        fs::write(&p2, &png_magic).unwrap();
        let b = make_builder(&tmp);
        let result = b.build_user_content(
            "two images",
            Some(&[
                p1.to_string_lossy().into_owned(),
                p2.to_string_lossy().into_owned(),
            ]),
        );
        let arr = result.as_array().unwrap();
        // 2 image blocks + 1 text block
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2]["text"], "two images");
    }

    // ── build_system_prompt helpers ───────────────────────────────────────────

    fn write_history_entries(tmp: &TempDir, entries: &[(&str, &str)]) {
        use std::fs;
        let memory_dir = tmp.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let mut lines = String::new();
        for (i, (ts, content)) in entries.iter().enumerate() {
            lines.push_str(&format!(
                "{{\"cursor\":{},\"timestamp\":\"{}\",\"content\":\"{}\"}}\n",
                i + 1,
                ts,
                content
            ));
        }
        fs::write(memory_dir.join("history.jsonl"), lines).unwrap();
    }

    fn write_memory_md(tmp: &TempDir, content: &str) {
        use std::fs;
        let memory_dir = tmp.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("MEMORY.md"), content).unwrap();
    }

    fn write_skill_md(tmp: &TempDir, name: &str, frontmatter: &str, body: &str) {
        use std::fs;
        let skill_dir = tmp.path().join("skills").join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .unwrap();
    }

    // ── build_system_prompt ───────────────────────────────────────────────────

    #[test]
    fn build_system_prompt_has_no_empty_parts() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        // Write a skill with a description so build_skills_summary returns non-empty
        // and the skills_section template render path is exercised.
        let skills_dir = tmp.path().join("skills").join("test-skill");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("SKILL.md"),
            "---\ndescription: A test skill\n---\n# Test skill body",
        )
        .unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        // No section should be empty — check no part between separators is blank
        for part in prompt.split("\n\n---\n\n") {
            assert!(
                !part.trim().is_empty(),
                "prompt contains an empty section; full prompt:\n{prompt}"
            );
        }
    }

    #[test]
    fn build_system_prompt_sections_joined_by_rule_separator() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SOUL.md"), "# Soul").unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            prompt.contains("\n\n---\n\n"),
            "sections should be separated by '\\n\\n---\\n\\n'"
        );
    }

    #[test]
    fn build_system_prompt_bootstrap_single_file_appears() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SOUL.md"), "# Soul\nSome soul content.").unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            prompt.contains("Some soul content."),
            "bootstrap content should appear"
        );
    }

    #[test]
    fn build_system_prompt_bootstrap_multiple_files_all_appear() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SOUL.md"), "soul-marker").unwrap();
        fs::write(tmp.path().join("USER.md"), "user-marker").unwrap();
        fs::write(tmp.path().join("TOOLS.md"), "tools-marker").unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(prompt.contains("soul-marker"), "SOUL.md content missing");
        assert!(prompt.contains("user-marker"), "USER.md content missing");
        assert!(prompt.contains("tools-marker"), "TOOLS.md content missing");
    }

    #[test]
    fn build_system_prompt_bootstrap_content_is_trimmed() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        // File has trailing newlines that should be stripped.
        fs::write(tmp.path().join("SOUL.md"), "soul-marker\n\n\n").unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        let soul_section = prompt
            .split("\n\n---\n\n")
            .find(|p| p.contains("SOUL.md"))
            .expect("SOUL.md section missing");
        assert!(
            !soul_section.ends_with('\n'),
            "bootstrap content should be trimmed; section:\n{soul_section}"
        );
    }

    #[test]
    fn build_system_prompt_missing_bootstrap_files_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        for filename in BOOTSTRAP_FILES {
            assert!(
                !prompt.contains(filename),
                "missing file '{filename}' should not appear in prompt"
            );
        }
    }

    #[test]
    fn build_system_prompt_memory_section_appears_when_nonempty() {
        let tmp = TempDir::new().unwrap();
        write_memory_md(&tmp, "# Memory\nRemember this important fact.");
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            prompt.contains("Remember this important fact."),
            "memory content should appear in prompt"
        );
    }

    #[test]
    fn build_system_prompt_no_memory_section_when_absent() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        // get_memory_context emits "## Long-term memory:\n..." only when content exists.
        assert!(
            !prompt.contains("## Long-term memory:"),
            "memory section header should be absent when MEMORY.md is missing"
        );
    }

    #[test]
    fn build_system_prompt_always_skill_in_active_skills_section() {
        let tmp = TempDir::new().unwrap();
        write_skill_md(
            &tmp,
            "eager",
            "description: Does things\nalways: true\n",
            "# Eager body",
        );
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            prompt.contains("Active Skills"),
            "Active Skills section should be present"
        );
        assert!(
            prompt.contains("Eager body"),
            "always-skill content should appear"
        );
    }

    #[test]
    fn build_system_prompt_non_always_skill_in_skills_summary_only() {
        let tmp = TempDir::new().unwrap();
        write_skill_md(
            &tmp,
            "lazy-skill",
            "description: A lazy skill\n",
            "# Lazy body",
        );
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            !prompt.contains("Active Skills"),
            "non-always skill should not create Active Skills section"
        );
        assert!(
            prompt.contains("lazy-skill"),
            "skill name should appear in summary"
        );
    }

    #[test]
    fn build_system_prompt_history_entries_formatted_correctly() {
        let tmp = TempDir::new().unwrap();
        write_history_entries(
            &tmp,
            &[
                ("2026-01-01 10:00", "first entry"),
                ("2026-01-01 10:01", "second entry"),
            ],
        );
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            prompt.contains("[2026-01-01 10:00] first entry"),
            "first entry should be formatted as [ts] content"
        );
        assert!(
            prompt.contains("[2026-01-01 10:01] second entry"),
            "second entry should appear"
        );
    }

    #[test]
    fn build_system_prompt_history_capped_at_max_recent() {
        let tmp = TempDir::new().unwrap();
        // Write 55 entries; only the last 50 should appear.
        let entries: Vec<(String, String)> = (1u32..=55)
            .map(|i| (format!("2026-01-01 {:05}", i), format!("item-{i:05}")))
            .collect();
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(ts, c)| (ts.as_str(), c.as_str()))
            .collect();
        write_history_entries(&tmp, &refs);
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        // Entries 1-5 should be dropped
        for i in 1u32..=5 {
            assert!(
                !prompt.contains(&format!("item-{i:05}")),
                "item-{i:05} should be excluded (beyond MAX_RECENT_HISTORY={MAX_RECENT_HISTORY})"
            );
        }
        assert!(
            prompt.contains("item-00055"),
            "newest entry should be present"
        );
    }

    #[test]
    fn build_system_prompt_history_section_absent_when_no_entries() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, None);
        assert!(
            !prompt.contains("Recent History"),
            "history section should be absent when there are no history entries"
        );
    }

    #[test]
    fn build_system_prompt_channel_telegram_propagates_to_identity() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let prompt = b.build_system_prompt(None, Some("telegram"));
        assert!(
            prompt.contains("messaging app"),
            "telegram channel hint should appear in prompt"
        );
    }

    // ── build_messages ───────────────────────────────────────────────────────

    fn bm(b: &ContextBuilder, text: &str) -> Vec<serde_json::Value> {
        b.build_messages(&[], text, None, None, None, None, "user")
    }

    #[test]
    fn build_messages_has_system_then_user() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = bm(&b, "hello");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn build_messages_user_content_contains_runtime_tag_and_text() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = bm(&b, "say hi");
        let content = msgs[1]["content"].as_str().expect("user content should be string");
        assert!(content.contains(RUNTIME_CONTEXT_TAG), "runtime tag missing");
        assert!(content.contains("say hi"), "user text missing");
        assert!(content.contains("Current Time:"), "time missing");
    }

    #[test]
    fn build_messages_history_is_inserted_between_system_and_user() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let history = vec![
            serde_json::json!({"role": "user", "content": "prev question"}),
            serde_json::json!({"role": "assistant", "content": "prev answer"}),
        ];
        let msgs = b.build_messages(&history, "new question", None, None, None, None, "user");
        assert_eq!(msgs.len(), 4, "system + 2 history + 1 new user");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "prev question");
        assert_eq!(msgs[2]["content"], "prev answer");
        assert_eq!(msgs[3]["role"], "user");
    }

    #[test]
    fn build_messages_merges_when_last_history_has_same_role() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        // History ends with a user message — current user turn should be merged.
        let history = vec![
            serde_json::json!({"role": "user", "content": "earlier user msg"}),
        ];
        let msgs = b.build_messages(&history, "continuation", None, None, None, None, "user");
        // system + merged-user (no extra message appended)
        assert_eq!(msgs.len(), 2, "should merge, not append");
        assert_eq!(msgs[1]["role"], "user");
        // The merged content should contain both the original and the new text.
        let content = msgs[1]["content"].as_str().expect("merged content is a string");
        assert!(content.contains("earlier user msg"), "original content lost");
        assert!(content.contains("continuation"), "new content missing");
    }

    #[test]
    fn build_messages_no_merge_when_roles_differ() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let history = vec![
            serde_json::json!({"role": "assistant", "content": "assistant turn"}),
        ];
        let msgs = b.build_messages(&history, "next user", None, None, None, None, "user");
        // system + assistant history + new user
        assert_eq!(msgs.len(), 3, "should not merge across different roles");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn build_messages_skill_names_appear_in_system_prompt() {
        let tmp = TempDir::new().unwrap();
        write_skill_md(&tmp, "my-skill", "description: My skill\n", "# Skill content here");
        let b = make_builder(&tmp);
        let skill_names = vec!["my-skill".to_string()];
        let msgs = b.build_messages(&[], "hi", Some(&skill_names), None, None, None, "user");
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("Skill content here"), "requested skill should be in system prompt");
    }

    #[test]
    fn build_messages_channel_appears_in_user_content_and_system() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = b.build_messages(&[], "msg", None, None, Some("telegram"), Some("99"), "user");
        let user_content = msgs[1]["content"].as_str().unwrap();
        assert!(user_content.contains("Channel: telegram"), "channel missing from runtime ctx");
        assert!(user_content.contains("Chat ID: 99"), "chat_id missing from runtime ctx");
        // System prompt identity section should have the telegram format hint
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("messaging app"), "channel not forwarded to system prompt");
    }

    #[test]
    fn build_messages_custom_role() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = b.build_messages(&[], "tool output", None, None, None, None, "tool");
        assert_eq!(msgs[1]["role"], "tool");
    }

    // ── add_assistant_message ────────────────────────────────────────────────

    #[test]
    fn add_assistant_message_appends_to_empty_list() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = b.add_assistant_message(vec![], Some("hello"), None, None, None);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn add_assistant_message_appends_after_existing() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let existing = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let msgs = b.add_assistant_message(existing, Some("reply"), None, None, None);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "reply");
    }

    #[test]
    fn add_assistant_message_with_tool_calls() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let tc = vec![serde_json::json!({"id": "call_1", "type": "function"})];
        let msgs = b.add_assistant_message(vec![], None, Some(tc), None, None);
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn add_assistant_message_with_reasoning() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let msgs = b.add_assistant_message(vec![], Some("answer"), None, Some("thought"), None);
        assert_eq!(msgs[0]["reasoning_content"], "thought");
    }

    #[test]
    fn add_assistant_message_with_thinking_blocks() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        let tb = vec![serde_json::json!({"type": "thinking", "thinking": "deep thought"})];
        let msgs = b.add_assistant_message(vec![], Some("answer"), None, None, Some(tb));
        assert_eq!(msgs[0]["thinking_blocks"][0]["thinking"], "deep thought");
    }

    #[test]
    fn add_assistant_message_returns_ownership() {
        let tmp = TempDir::new().unwrap();
        let b = make_builder(&tmp);
        // Ensure the return value is the updated vec (ownership transferred)
        let result = b.add_assistant_message(vec![], Some("x"), None, None, None);
        assert_eq!(result.len(), 1);
    }
}
