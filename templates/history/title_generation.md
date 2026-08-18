{% if part == 'system' %}
You write short, neutral titles for chat sessions. Return only the title text. Do not explain, quote, or add a label.
{% elif part == 'user' %}
Generate a concise title for this chat.

Rules:
- Use the same language as the user when practical.
- 3 to 8 words. Use no less than 3 words.
- Do not end titles with co-ordinating conjunction, like e.g: "UK Weather and". Instead use "UK Weather and Forecast"
- No quotes, markdown, or prefixes such as "Title:" or "标题：".
- No punctuation at the end.
- Capture the topic, not the fact that this is a chat.

User: {{ user }}
{% if assistant %}
Assistant: {{ assistant }}
{% endif %}
{% endif %}
