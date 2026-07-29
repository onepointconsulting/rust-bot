# Skill: Onepoint Client-Facing Newsletter

## When to use this skill

Use this skill whenever the user asks to:
- Create, draft, or write a client-facing newsletter for Onepoint
- Generate a client edition of the Onepoint newsletter
- Produce an external newsletter for Onepoint clients or prospects
- Draft a newsletter for a specific client or client segment

Trigger phrases: "client newsletter", "client-facing newsletter", "external newsletter", "newsletter for clients", "client update newsletter".

---

## Brand Identity

### Logo
- File: `onepoint-mint.png` in the workspace folder
- URL fallback: `https://www.onepointltd.com/wp-content/uploads/2026/05/Vector-6.png`
- Always place the logo at the top of the newsletter (header area), centred or left-aligned.

### Colours
| Role            | Hex       | Usage                                      |
|-----------------|-----------|--------------------------------------------|
| Primary (mint)  | `#00D3BA` | Header background, section dividers, CTAs  |
| Black           | `#000000` | Header logo bar background                 |
| White           | `#FFFFFF` | Header text, body background               |
| Dark text       | `#1A1A1A` | Body copy                                  |
| Light grey      | `#F5F5F5` | Alternate section backgrounds              |
| Mid grey        | `#666666` | Captions, metadata, footer text            |

### Typography
- **Headings**: Arial Bold (fallback: Helvetica, sans-serif)
- **Body**: Arial (fallback: Helvetica, sans-serif)
- **Font sizes**: H1 = 28px, H2 = 20px, body = 15px, footer = 12px

---

## Output Format

Always produce the newsletter as a **self-contained HTML file** with inline CSS.

- Filename pattern: `onepoint_client_newsletter_YYYY-MM.html` (e.g. `onepoint_client_newsletter_2026-07.html`)
- Save to the workspace folder.
- Max width: 680px (email-safe), centred on page
- Mobile-friendly: use `max-width: 100%` on images, fluid layout

---

## Newsletter Structure

Every issue must contain these sections in this order:

### 1. Header
- Onepoint logo on black background (`#000000`)
- Newsletter title: **"Onepoint Perspectives"**
- Issue subtitle: month and year (e.g. "July 2026")
- Tagline: *"Insight & Innovation from Onepoint"*

### 2. Industry Insights
- Section heading: **"Industry Insights"**
- 1–3 short thought-leadership items: trends, research findings, or commentary relevant to clients' industries (AI, data, digital transformation)
- Each item: bold title + 2–4 sentences
- Tone: authoritative, insightful, forward-looking
- If no content is provided, use a placeholder: *"No industry insights this issue."*
- **Do not fabricate statistics or research findings** — only include what is explicitly provided or is well-established fact

### 3. Client Success Stories
- Section heading: **"Client Success Stories"**
- 1–2 brief case study highlights or project outcomes
- Each item: **Client/Project name** (bold, anonymised if required) + outcome-focused 2–4 sentence summary
- Focus on measurable results and business value delivered
- If no content is provided, use a placeholder: *"No client stories this issue."*
- **Never include confidential client details unless explicitly provided and cleared for external use**

### 4. What We've Been Building
- Section heading: **"What We've Been Building"**
- Updates on Onepoint products, tools, or capabilities relevant to clients (e.g. ConvertWise, Agent Harness, Tender Search Tool, Data Lineage Tool)
- Frame updates in terms of client benefit, not internal progress
- If no content is provided, use a placeholder: *"No product updates this issue."*

### 5. Upcoming Events & Opportunities
- Section heading: **"Upcoming Events & Opportunities"**
- Table or list: date, event name, brief description, and a CTA where relevant (e.g. "Get in touch to find out more")
- Include external events (conferences, webinars, roundtables) Onepoint is attending or hosting
- If no content is provided, use a placeholder: *"No upcoming events this issue."*

### 6. From the CEO
- Section heading: **"From the CEO"**
- A short message from Shashin Shah (Founder & CEO)
- Signed off as: *— Shashin Shah, Founder & CEO, Onepoint*
- Tone: professional, visionary, client-centric — focused on partnership, value delivery, and the future of AI & data
- If no content is provided, generate a short placeholder message in keeping with Onepoint's values (purpose beyond profit, AI & data innovation, trusted partnership)

### 7. Footer
- Onepoint Consulting Limited
- Website: www.onepointltd.com
- Contact: info@onepointltd.com
- LinkedIn: https://www.linkedin.com/company/onepointltd
- Legal small print: *"© [Year] Onepoint Consulting Limited. All rights reserved. You are receiving this newsletter because of your relationship with Onepoint. To unsubscribe, reply with 'Unsubscribe' in the subject line."*
- Background: `#1A1A1A`, text: `#AAAAAA`

---

## Tone & Style

- **Formal but approachable** — professional, confident, client-centric
- Avoid internal jargon, team in-jokes, or references to internal processes
- Use "we", "our clients", "our work together" — partnership language
- Every section should communicate value to the reader, not just activity
- Keep sections concise; no section should exceed ~200 words unless the user explicitly provides longer content
- Use British English spelling (e.g. "organisation", "colour", "recognise")
- **Stricter tone/error tolerance than internal newsletter** — proofread carefully before sending
- Never fabricate facts, figures, quotes, client names, or outcomes not present in the source material

---

## Input Handling

The user may provide content in one of these ways:
1. **Pasting raw content** into the chat — extract and place into the appropriate sections
2. **Providing bullet points or notes** — expand into polished newsletter copy
3. **Providing nothing** — generate a full placeholder issue with all sections present but marked as placeholder
4. **Referencing emails** — if the user says "use the emails from this week", fetch recent Gmail and extract relevant items
5. **Providing documents or attachments** — parse DOCX, PDF, or images via OCR as needed

Always ask the user to confirm:
- The issue month/year if not specified
- Whether any client names or project details need to be anonymised before including

---

## Workflow

1. Confirm the issue date (month/year) with the user if not provided
2. Confirm whether any client/project details need anonymising
3. Collect or confirm content for each of the 6 sections. See "Email Handling Step" for email-sourced content.
4. Generate the HTML newsletter with inline CSS
5. Save to workspace as `onepoint_client_newsletter_YYYY-MM.html`
6. Send the file to the user via the `message` tool with `media` parameter
7. Offer to make revisions before the user sends to clients

## Email Handling Step

1. First find emails in the requested range without downloading the whole body — limit body to 100 characters.
2. Identify emails truly relevant to client-facing content (project outcomes, product updates, events, CEO messages).
3. Exclude internal-only content (HR matters, internal ops, staff personal news) — this is an external newsletter.
4. Download the relevant emails locally and extract their content, including PDF and image attachments.
5. Extract content from attached images and PDFs via OCR where needed.

---

## Content Guardrails

These rules must always be followed for client-facing output:

- **No fabrication**: Never invent facts, figures, quotes, client names, or outcomes not present in the source material.
- **No internal leakage**: Do not include internal team news, staff personal details, financial figures, or anything marked confidential.
- **Anonymise by default**: If a client name is mentioned in source material and it's unclear whether it's cleared for external use, anonymise it (e.g. "a leading financial services client") and flag it for the reviewer.
- **Flag for review**: If any content seems incomplete, ambiguous, or potentially sensitive, add a reviewer note in HTML comments (`<!-- REVIEWER NOTE: ... -->`) rather than guessing.

---

## HTML Template Reference

Use this structure as the base template:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Onepoint Perspectives — [Month Year]</title>
</head>
<body style="margin:0;padding:0;background:#F0F0F0;font-family:Arial,Helvetica,sans-serif;">
  <table width="100%" cellpadding="0" cellspacing="0" style="background:#F0F0F0;">
    <tr><td align="center" style="padding:20px 0;">
      <table width="680" cellpadding="0" cellspacing="0" style="max-width:680px;background:#FFFFFF;">

        <!-- HEADER -->
        <tr><td style="background:#000000;padding:30px 40px;text-align:center;">
          <img src="https://www.onepointltd.com/wp-content/uploads/2026/05/Vector-6.png"
               alt="Onepoint" style="max-height:60px;filter:brightness(0) invert(1);">
          <h1 style="color:#FFFFFF;font-size:28px;margin:16px 0 4px;">Onepoint Perspectives</h1>
          <p style="color:#FFFFFF;font-size:15px;margin:0;">[Month Year] &nbsp;|&nbsp; <em>Insight &amp; Innovation from Onepoint</em></p>
        </td></tr>

        <!-- SECTION: Industry Insights -->
        <tr><td style="padding:30px 40px;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Industry Insights</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: Client Success Stories -->
        <tr><td style="padding:30px 40px;background:#F5F5F5;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Client Success Stories</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: What We've Been Building -->
        <tr><td style="padding:30px 40px;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">What We've Been Building</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: Upcoming Events & Opportunities -->
        <tr><td style="padding:30px 40px;background:#F5F5F5;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Upcoming Events &amp; Opportunities</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: From the CEO -->
        <tr><td style="padding:30px 40px;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">From the CEO</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
          <p style="color:#1A1A1A;font-size:15px;"><em>— Shashin Shah, Founder &amp; CEO, Onepoint</em></p>
        </td></tr>

        <!-- FOOTER -->
        <tr><td style="background:#1A1A1A;padding:24px 40px;text-align:center;">
          <p style="color:#AAAAAA;font-size:12px;margin:0;">
            <strong style="color:#FFFFFF;">Onepoint Consulting Limited</strong><br>
            <a href="https://www.onepointltd.com" style="color:#00D3BA;">www.onepointltd.com</a>
            &nbsp;|&nbsp; info@onepointltd.com
            &nbsp;|&nbsp; <a href="https://www.linkedin.com/company/onepointconsulting/" style="color:#00D3BA;">LinkedIn</a><br><br>
            &copy; [Year] Onepoint Consulting Limited. All rights reserved.<br>
            You are receiving this newsletter because of your relationship with Onepoint.
            To unsubscribe, reply with &lsquo;Unsubscribe&rsquo; in the subject line.
          </p>
        </td></tr>

      </table>
    </td></tr>
  </table>
</body>
</html>
```

---

## Defaults (when user doesn't specify)

| Setting              | Default                                      |
|----------------------|----------------------------------------------|
| Issue date           | Current month and year                       |
| Newsletter title     | Onepoint Perspectives                        |
| Sign-off name        | Shashin Shah, Founder & CEO                  |
| Language             | British English                              |
| Output format        | HTML file with inline CSS                    |
| Logo source          | URL (no local embed needed)                  |
| Max width            | 680px                                        |
| Client anonymisation | Anonymise by default unless cleared          |
| Tone                 | Formal, professional, client-centric         |
