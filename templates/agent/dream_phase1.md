Compare conversation history against current memory files. Also scan memory files for stale content — even if not mentioned in history.

Output one line per finding:
[FILE] atomic fact (not already in memory)
[FILE-REMOVE] reason for removal{% if learn_skills %}
[SKILL] <existing-skill-name | NEW:proposed-name> reusable procedure: the steps and the pitfall avoided{% endif %}

Files: USER (identity, preferences), SOUL (bot behavior, tone), MEMORY (knowledge, project context)

Rules:
- Atomic facts: "has a cat named Luna" not "discussed pet care"
- Corrections: [USER] location is Tokyo, not Osaka
- Capture confirmed approaches the user validated

Staleness — flag for [FILE-REMOVE]:
- Time-sensitive data older than 14 days: weather, daily status, one-time meetings, passed events
- Completed one-time tasks: triage, one-time reviews, finished research, resolved incidents
- Resolved tracking: merged/closed PRs, fixed issues, completed migrations
- Detailed incident info after 14 days — reduce to one-line summary
- Superseded: approaches replaced by newer solutions, deprecated dependencies

Do not add: current weather, transient status, temporary errors, conversational filler.
{% if learn_skills %}
Skills — procedural memory, kept apart from the memory files:
- Emit [SKILL] only for a reusable procedure: a non-trivial workflow likely to come up again, the working approach found after errors or dead ends (name the pitfall), or a user correction about how to do a kind of task. History lines starting "PROCEDURE:" are candidates.
- Never for facts, preferences or one-off events: those stay [USER], [SOUL] or [MEMORY].
- Prefer an existing skill from the list below when the lesson fits it; use NEW:<name> (lowercase letters, digits, '.', '_', '-') only when none does.
- Lessons, not logs: imperative steps and the reason, no dates, ticket numbers or narration.
- One [SKILL] line per lesson; merge a lesson repeated across entries into one line.

Existing skills:
{{ skill_index }}
{% endif %}
[SKIP] if nothing needs updating.
