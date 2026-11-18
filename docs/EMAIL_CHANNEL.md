# Email channel

The email channel lets people talk to the bot by mail: it polls a mailbox over IMAP and answers over SMTP.
It is configured under `channels.email` in `config.json`, either by hand, with `rust-bot onboard`, or on the
WebUI's [Settings → Channels](WEBSOCKETS_CHAT.md#settings--channels) page. Channels are built when the gateway
starts, so every change applies after a restart.

The channel only starts when `enabled` and `consentGranted` are both on and the IMAP and SMTP host, username and
password are all set. `check-config`, `load_config` and the Settings dialog refuse a `channels.email` entry that
does not have the shape below (for example a port written as text).

Passwords may be written as `${VAR}` to read them from an environment variable of the process running rust-bot.
The Settings dialog never sends passwords to the browser; a blank password input keeps the stored one.

## `channels.email` reference

| Key | Default | Meaning |
|---|---|---|
| **General** | | |
| `enabled` | `false` | Start the email channel with the gateway. |
| `consentGranted` | `false` | The owner of the mailbox has explicitly agreed that the bot reads and sends mail with it. The channel does not start without it. |
| **Incoming mail (IMAP)** | | |
| `imapHost` | `""` | Host name of the IMAP server that receives the bot's mail. Required. |
| `imapPort` | `993` | Port of the IMAP server (993 for SSL, 143 without). |
| `imapUsername` | `""` | User name for the IMAP login, usually the mailbox address. Required. |
| `imapPassword` | `""` | Password for the IMAP login. Required. |
| `imapMailbox` | `""` | Mailbox (folder) polled for new mail. Blank means `INBOX`. |
| `imapUseSsl` | `true` | Connect to the IMAP server over SSL/TLS. |
| **Outgoing mail (SMTP)** | | |
| `smtpHost` | `""` | Host name of the SMTP server that sends the bot's replies. Required. |
| `smtpPort` | `587` | Port of the SMTP server (587 for STARTTLS, 465 for SSL). |
| `smtpUsername` | `""` | User name for the SMTP login. Required. |
| `smtpPassword` | `""` | Password for the SMTP login. Required. |
| `smtpUseTls` | `true` | Upgrade the SMTP connection with STARTTLS. |
| `smtpUseSsl` | `false` | Connect to the SMTP server over SSL/TLS from the start (port 465). Takes precedence over STARTTLS. |
| `fromAddress` | `""` | Sender address of outgoing mail. Blank uses the SMTP username, then the IMAP username. |
| **Behaviour** | | |
| `autoReplyEnabled` | `false` | Answer incoming mail automatically. When off, the bot only sends mail it is asked to send. |
| `pollIntervalSeconds` | `60` | Seconds between two checks for new mail (at least 5). |
| `markSeen` | `true` | Mark fetched mail as read on the server. |
| `maxBodyChars` | `12000` | Longest mail body (in characters) passed to the agent; longer bodies are cut. |
| `subjectPrefix` | `"Re: "` | Prefix added to the subject of a reply. |
| **Security** | | |
| `allowFrom` | `[]` | Sender addresses the bot accepts mail from; `*` accepts everyone. Mail from anyone else is ignored, and an empty list ignores all mail. |
| `verifyDkim` | `true` | Only accept mail whose `Authentication-Results` header reports `dkim=pass` (anti-spoofing). |
| `verifySpf` | `true` | Only accept mail whose `Authentication-Results` header reports `spf=pass` (anti-spoofing). |
| **Attachments** | | |
| `allowedAttachmentTypes` | `[]` | MIME types of attachments passed to the agent, e.g. `application/pdf` or `image/*`; `*` allows all. Empty ignores every attachment. |
| `maxAttachmentSize` | `2097152` | Largest attachment (in bytes, 2 MB) passed to the agent; larger ones are skipped. |
| `maxAttachmentsPerEmail` | `5` | Most attachments of one mail passed to the agent. |

In `config.json`, `allowFrom` and `allowedAttachmentTypes` are JSON lists; in the Settings form they are typed one
entry per line (no commas).

The help texts come from the doc comments of `EmailConfig` (`src/config/channels.rs`), which also drive the
Settings form; keep the two in step. Other keys in the entry (for example a per-channel `sendProgress`) are kept
as they are when the form saves.

## Example

```json
{
  "channels": {
    "email": {
      "enabled": true,
      "consentGranted": true,
      "imapHost": "imap.example.com",
      "imapUsername": "bot@example.com",
      "imapPassword": "${BOT_IMAP_PASSWORD}",
      "smtpHost": "smtp.example.com",
      "smtpUsername": "bot@example.com",
      "smtpPassword": "${BOT_SMTP_PASSWORD}",
      "allowFrom": ["alice@example.com"]
    }
  }
}
```
