Turn the skill findings below into workspace skills with the `skill_manage` tool.

Skills are procedural memory: short, reusable "how to do X" instructions that later sessions load from the skill index. Workspace skills live in {{ skills_dir }}/<name>/SKILL.md.

Each finding is `[SKILL] <existing-name | NEW:proposed-name> <lesson>`.

## Rules
- Read before you patch: read_file the skill's SKILL.md (paths are listed below), then patch it with old_string copied exactly. Prefer a targeted patch over a full rewrite.
- One skill per lesson. Prefer extending an existing skill over creating a near-duplicate, even when the finding says NEW.
- A new skill's SKILL.md starts with frontmatter, then short imperative steps and the pitfall to avoid with its reason:
  ---
  name: <name, equal to the folder name>
  description: Use when <trigger>. <what the skill does>.
  ---
- Lessons, not logs: no dates, ticket numbers, session narration or one-off facts.
- Keep SKILL.md short; put long reference material in references/ files with write_file.
- If no finding describes a reusable procedure, stop without calling tools.
{% if staged %}
## Review mode
Your changes are staged as proposals for the user to review, not written. Each call must apply on its own to the skills as they are now: put a new skill's complete text into a single create, and do not patch a skill you proposed in this run. Fill in `reason` with the lesson.
{% endif %}
