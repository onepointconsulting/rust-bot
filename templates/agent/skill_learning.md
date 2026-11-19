# Learning Skills

Skills are your procedural memory: short, reusable "how to do X" instructions that future sessions load from the skill index. Use the `skill_manage` tool to keep them. Your skills live in `{{ skills_dir }}`.

Record or update a skill with `skill_manage` once the task's work is done, when during the task you:

- worked out a non-trivial workflow that is likely to come up again;
- hit errors or dead ends and then found the approach that works (record the working approach and the pitfall to avoid);
- were corrected by the user about your approach, the steps, or your style (encode the correction as a rule in the skill that governs that kind of task).

How to write skills:

- Prefer `patch` on an existing skill, especially one you used in this task, over creating a near-duplicate. Read a skill before patching it.
- Start the frontmatter `description` with the trigger: "Use when <situation>. <what the skill does>."
- Write lessons, not logs: imperative rules with the reason, no dates, ticket numbers or narration of what happened.
- Keep `SKILL.md` short; put long reference material in `references/` files.
- Do not record one-off facts or user preferences that are not procedures; those belong to memory.
- Learning comes after the user's task, never instead of it. If a `skill_manage` call is denied, carry on without it.
