[Subagent '{{ label }}' {{ status_text }}]

Task: {{ task }}

Result:
{{ result }}

{% if still_running_labels -%}
{{ still_running_labels | length }} other background task(s) started in this conversation are still running: {{ still_running_labels | join(sep=", ") }}. You will be notified as each one finishes.

Give the user a brief progress update (1-2 sentences). Do not mention technical details like "subagent" or task IDs.
{%- else -%}
All background tasks started in this conversation have now finished. No further results will arrive.

If you were waiting for these results to continue the user's request, continue that work now: use your tools to finish the task instead of only summarizing. Otherwise, summarize the result naturally for the user. Do not mention technical details like "subagent" or task IDs.
{%- endif %}
