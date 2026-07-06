# Skill: Onepoint Internal Newsletter

## When to use this skill

Use this skill whenever the user asks to:
- Create, draft, or write an internal newsletter for Onepoint
- Generate a new issue of the Onepoint newsletter
- Produce a staff/team newsletter for Onepoint Consulting

Trigger phrases: "newsletter", "internal newsletter", "Onepoint newsletter", "draft a newsletter", "write a newsletter for the team".

---

## Brand Identity

### Logo
- File: `C:/Users/gilfe/.rust-bot/workspace/onepoint-mint.png`
- URL fallback: `https://www.onepointltd.com/wp-content/uploads/2025/09/onepoint-mint.png`
- Always place the logo at the top of the newsletter (header area), centred or left-aligned.

### Colours
| Role            | Hex       | Usage                                      |
|-----------------|-----------|--------------------------------------------|
| Primary (mint)  | `#000000` | Header background, section dividers, CTAs  |
| White           | `#FFFFFF` | Header text on mint, body background       |
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

- Filename pattern: `onepoint_newsletter_YYYY-MM.html` (e.g. `onepoint_newsletter_2026-07.html`)
- Save to: `C:/Users/gilfe/.rust-bot/workspace/`
- Max width: 680px (email-safe), centred on page
- Mobile-friendly: use `max-width: 100%` on images, fluid layout

---

## Newsletter Structure

Every issue must contain these sections in this order:

### 1. Header
- Onepoint logo (`onepoint-mint.png`) on mint green background (`#00D3BA`)
- Newsletter title: **"Onepoint Inside"**
- Issue subtitle: month and year (e.g. "July 2026")
- Tagline: *"Woven by Onepoint"*

### 2. Team News
- Section heading: **"Team News"**
- Short news items about the team: new joiners, departures, anniversaries, awards, certifications
- Bullet list or short paragraphs
- If no content is provided, use a placeholder: *"No team news this issue."*

### 3. Project Updates
- Section heading: **"Project Updates"**
- Updates on client projects or internal initiatives
- Each update: **Project name** (bold) followed by 2–4 sentences of update
- If no content is provided, use a placeholder: *"No project updates this issue."*

### 4. People
- Section heading: **"People"**
- Spotlight on a team member, interview snippet, or personal milestone (birthday, work anniversary, achievement)
- If no content is provided, use a placeholder: *"No people spotlight this issue."*

### 5. Upcoming Events
- Section heading: **"Upcoming Events"**
- Table or list of upcoming events: date, event name, brief description
- Include both internal events (team meetings, training) and relevant external events (conferences, client events)
- If no content is provided, use a placeholder: *"No upcoming events this issue."*
- Do not create events about meditation or related to rust-bot
- Skip this section in case there are no noteworthy events

### 6. From the CEO
- Section heading: **"From the CEO"**
- A short message from Shashin Shah (Founder & CEO)
- Signed off as: *— Shashin Shah, Founder & CEO, Onepoint*
- Tone: warm, forward-looking, motivational but grounded
- If no content is provided, generate a short placeholder message in keeping with Onepoint's values (purpose beyond profit, AI & data innovation, collaboration)

### 7. Footer
- Onepoint Consulting Limited
- Website: www.onepointltd.com
- Contact: info@onepointltd.com
- Small print: *"This is an internal newsletter for Onepoint staff only."*
- Social icons or links (LinkedIn): optional
- Background: `#1A1A1A`, text: `#AAAAAA`

---

## Tone & Style

- **Casual but professional** — friendly, warm, direct
- Avoid jargon and overly corporate language
- Use "we", "our team", "the team" — inclusive language
- Keep sections concise; no section should exceed ~200 words unless the user explicitly provides longer content
- Use British English spelling (e.g. "organisation", "colour", "recognise")

---

## Input Handling

The user may provide content in one of these ways:
1. **Pasting raw content** into the chat — extract and place into the appropriate sections
2. **Providing bullet points or notes** — expand into polished newsletter copy
3. **Providing nothing** — generate a full placeholder issue with all sections present but marked as placeholder
4. **Referencing emails** — if the user says "use the emails from this week", fetch recent Gmail and extract relevant items

Always ask the user to confirm the issue month/year if not specified.

---

## Workflow

1. Confirm the issue date (month/year) with the user if not provided
2. Collect or confirm content for each of the 6 sections. See "Email Handling Step" to know how to deal with emails.
3. Generate the HTML newsletter with inline CSS
4. Save to workspace as `onepoint_newsletter_YYYY-MM.html`
5. Send the file to the user via the `message` tool with `media` parameter
6. Offer to make revisions

## Email Handling Step

1. Make sure that you find first the emails in the requested range without downloading the whole body. Limit the body to 100 characters.
2. Find the emails which are truly relevant for the newsletter.
3. Download the relevant emails locally and extract their content. This can include PDFs and image attachments.
4. Extract the content from attached images and PDFs.

---

## HTML Template Reference

Use this structure as the base template:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Onepoint Inside — [Month Year]</title>
</head>
<body style="margin:0;padding:0;background:#F0F0F0;font-family:Arial,Helvetica,sans-serif;">
  <table width="100%" cellpadding="0" cellspacing="0" style="background:#F0F0F0;">
    <tr><td align="center" style="padding:20px 0;">
      <table width="680" cellpadding="0" cellspacing="0" style="max-width:680px;background:#FFFFFF;">

        <!-- HEADER -->
        <tr><td style="background:#000000;padding:30px 40px;text-align:center;">
          <img src="https://www.onepointltd.com/wp-content/uploads/2025/09/onepoint-mint.png"
               alt="Onepoint" style="max-height:60px;filter:brightness(0) invert(1);">
          <h1 style="color:#FFFFFF;font-size:28px;margin:16px 0 4px;">Onepoint Inside</h1>
          <p style="color:#FFFFFF;font-size:15px;margin:0;">[Month Year] &nbsp;|&nbsp; <em>Woven by Onepoint</em></p>
        </td></tr>

        <!-- SECTION: Team News -->
        <tr><td style="padding:30px 40px;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Team News</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: Project Updates -->
        <tr><td style="padding:30px 40px;background:#F5F5F5;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Project Updates</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: People -->
        <tr><td style="padding:30px 40px;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">People</h2>
          <p style="color:#1A1A1A;font-size:15px;line-height:1.6;">[Content]</p>
        </td></tr>

        <!-- SECTION: Upcoming Events -->
        <tr><td style="padding:30px 40px;background:#F5F5F5;">
          <h2 style="color:#00D3BA;font-size:20px;border-bottom:2px solid #00D3BA;padding-bottom:6px;">Upcoming Events</h2>
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
            &nbsp;|&nbsp; info@onepointltd.com<br><br>
            This is an internal newsletter for Onepoint staff only.
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

| Setting         | Default                          |
|-----------------|----------------------------------|
| Issue date      | Current month and year           |
| Sign-off name   | Shashin Shah, Founder & CEO      |
| Language        | British English                  |
| Output format   | HTML file with inline CSS        |
| Logo source     | URL (no local embed needed)      |
| Max width       | 680px                            |
