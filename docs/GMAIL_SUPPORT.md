# Gmail support

Rust Bot can expose three Gmail agent tools when enabled in config:

- `gmail` — reads messages from the user's inbox (read-only)
- `gmail_email_send` — sends an email to a recipient (plain text or HTML)
- `gmail_email_download` — downloads an email and its attachments into the workspace

Access is granted through Google OAuth; credentials are stored on disk and reused by the agent. All three tools share the same two files from config:

- `client_secret.json` (`tools.gmail.clientSecretPath`): the OAuth Desktop-app client. Release binaries built with a [built-in client](#built-in-google-client) write it for you; otherwise you download it from Google Cloud Console.
- `token_cache.json` (`tools.gmail.tokenCachePath`): the refresh and access tokens, written when you sign in.

## Built-in Google client

Binaries built with the `RUST_BOT_GMAIL_CLIENT_SECRET` environment variable embed a shared OAuth Desktop client. With such a binary, users skip the Google Cloud setup below: `rust-bot gmail-auth` (and the config wizard) write the built-in client to `tools.gmail.clientSecretPath` when no file is there yet, and the user only signs in. A client secret the user installed is never replaced, and `--client-secret` still overrides it.

`RUST_BOT_GMAIL_CLIENT_SECRET` is read by `build.rs` and holds either the path of a `client_secret.json` or the JSON itself:

```powershell
# Local build (PowerShell)
$env:RUST_BOT_GMAIL_CLIENT_SECRET = "$env:USERPROFILE\.rust-bot\workspace\credentials\client_secret.json"
cargo build --release
```

The release workflows pass the GitHub Actions secret `RUST_BOT_GMAIL_CLIENT_SECRET` (the JSON content); when that secret is not set, nothing is embedded and users bring their own client as before.

Who can sign in with the built-in client depends on its Google Cloud project:

- **Internal** (project in a Google Workspace organisation): every account in that organisation, with no Google review and no token expiry.
- **External, Testing**: only accounts listed under *Audience → Test users*, and their sign-in expires after 7 days (`rust-bot gmail-auth --force` to renew).
- **External, In production, unverified**: any account, behind a "Google hasn't verified this app" warning, capped at 100 users. Removing the cap needs Google verification, which for `gmail.readonly` (a restricted scope) includes a yearly security assessment.

A Desktop client's secret is not confidential (Google treats installed apps as public clients), but anyone with the binary can extract it and use the client in the same project, so keep its quota and consent screen in mind.

## Google Cloud setup

Only needed when your binary has no built-in client, or you want your own.


1. Open the [Google Cloud Console](https://console.cloud.google.com/) and create or select a project.
2. Enable the **Gmail API** for that project.
3. Configure the **OAuth consent screen** (External or Internal, depending on your use case). If you plan to use send, ensure the consent screen allows the **Send email on your behalf** scope (`gmail.send`).
4. Create an OAuth **Desktop app** client and download the client secret JSON.
5. No redirect URI needs registering: Desktop-app clients accept the `http://localhost:<port>` redirect the sign-in uses (a free local port is picked each time).

> Credential files contain secrets and are gitignored. Do not commit `client_secret.json` or `token_cache.json`.

## Signing in

There are two main routes. Both read the paths from your config, so no files need copying by hand.

### `rust-bot gmail-auth`

```bash
# Install the downloaded client secret at tools.gmail.clientSecretPath and sign in
rust-bot gmail-auth --client-secret ~/Downloads/client_secret_1234.json

# Client secret already in place: just sign in
rust-bot gmail-auth

# A different config file
rust-bot gmail-auth -c ./configs/openai-compat/config_gmail.json

# Replace the client secret and/or sign in again from scratch
rust-bot gmail-auth --client-secret new_client.json --force
```

| Option                   | Meaning                                                                                                   |
| ------------------------ | --------------------------------------------------------------------------------------------------------- |
| `-c, --config <PATH>`    | Config file whose `tools.gmail` paths are used (same default as the other subcommands)                    |
| `--client-secret <PATH>` | Downloaded OAuth client JSON. It is checked and copied to `tools.gmail.clientSecretPath`                 |
| `--force`                | Replace a different existing client secret, and delete the token cache to sign in again                   |

What happens:

1. The client secret is checked (it must be a Google OAuth client JSON) and installed, if you passed `--client-secret`. A different existing file is only replaced with `--force`.
   Without `--client-secret` and with no file at `tools.gmail.clientSecretPath`, the built-in client is written there (if the binary has one).
2. If there is no token cache yet (or `--force` is set), a sign-in link is printed; open it in your browser. A local server on a free port receives the OAuth callback, so there is no code to copy and paste. Sign in with Google and grant Gmail read and send permission.
3. The tokens are written to `tools.gmail.tokenCachePath` (the directory is created if needed), and one inbox message is listed to check that access works.
4. If you are already signed in, nothing changes; use `--force` to sign in again.
5. If `tools.gmail.enable` is `false`, you are reminded to switch it on.

Restart the agent or gateway afterwards to load the Gmail tools.

If you previously signed in with the read-only scope, run `rust-bot gmail-auth --force` so the token cache includes `gmail.send`.

### Config wizard

`rust-bot onboard --wizard` → **Tools** → **gmail** asks for the paths and then:

- with no client secret, uses the built-in client if the binary has one; otherwise asks for the path of the downloaded JSON (leave it empty to skip) and installs it;
- offers to **Sign in to Google now?**;
- when you are already signed in, offers to sign in again (default no).

Skipping or a failed sign-in never cancels the wizard: it prints a note that Gmail stays disabled until you run `rust-bot gmail-auth -c <config>`.

### Standalone `gmail-auth` binary (legacy)

The release packages still ship a separate `gmail-auth` binary. It uses the same sign-in code, but it does not read the config: it writes `token_cache.json` next to the client secret you pass (default `./credentials/client_secret.json`), so you then have to copy both files to the paths in your config.

```bash
cargo run --bin gmail-auth -- /path/to/client_secret.json
```

## Missing credentials

The agent never exits because of missing Gmail credentials. When `tools.gmail.enable` is `true` but a file is missing, the Gmail tools are left out and everything else starts normally:

- **CLI (`agent`)**: a yellow `Warning:` line on stderr before the welcome text or the one-shot reply (stdout stays clean when piped).
- **`gateway` / `api`**: a `Warning:` line in the start-up status lines.
- **`acp`**: a log warning only (stdout carries the protocol).
- **websockets-chat**: administrators see a dismissible amber strip in the chat listing the warning. A dismissed warning stays hidden until the page is reloaded.
- **Settings → Tools**: the Gmail row shows **Needs credentials** with the reason underneath. It is worked out each time the page loads, so it clears as soon as `gmail-auth` has run (the tools themselves still need a restart).

The message says what is missing, for example:

```
Gmail tools disabled: not signed in (no token cache at ~/.rust-bot/credentials/token_cache.json). Run `rust-bot gmail-auth` and restart.
```

If a credential file is deleted while the agent is running, the Gmail tools return an error naming the file and `rust-bot gmail-auth` instead of starting a sign-in.

## Enabling the Gmail tools

Enable both tools in your agent config under `tools.gmail`:

```json
"gmail": {
  "enable": true,
  "client_secret_path": "~/.rust-bot/credentials/client_secret.json",
  "token_cache_path": "~/.rust-bot/credentials/token_cache.json",
  "max_results": 20
}
```

There is no separate flag for send — when `enable` is `true` and both credential files exist, the agent registers `gmail`, `gmail_email_send` and `gmail_email_download`.

A sample config with Gmail enabled is in `configs/openai-compat/config_gmail.json`. Run the agent with that config once credentials are in place:

```bash
# Read inbox
cargo run -- agent -m "Summarize my latest inbox emails" \
  --config ./configs/openai-compat/config_gmail.json

# Send plain-text email (the model chooses the gmail_email_send tool)
cargo run -- agent -m "Send an email to alice@example.com with subject Hello and body Hi Alice" \
  --config ./configs/openai-compat/config_gmail.json

# Send HTML email (ask the model to use format html and HTML in the body)
cargo run -- agent -m "Send an HTML email to alice@example.com with subject Report and body containing a bold greeting" \
  --config ./configs/openai-compat/config_gmail.json
```

The agent uses the cached tokens from `token_cache.json` and refreshes them automatically via `yup-oauth2` when they expire.

## Gmail agent tools


| Tool name          | Purpose                      | Key parameters                                           |
| ------------------ | ---------------------------- | -------------------------------------------------------- |
| `gmail`            | List and read inbox messages | `limit`, `after`, `before`, `only_subject`, `body_limit` |
| `gmail_email_send` | Send an email                | `to`, `subject`, `body` (required); `format` (optional)  |




### `gmail_email_send` parameters


| Parameter | Required | Default | Description                                               |
| --------- | -------- | ------- | --------------------------------------------------------- |
| `to`      | yes      | —       | Recipient email address                                   |
| `subject` | yes      | —       | Email subject (non-ASCII characters are RFC 2047–encoded) |
| `body`    | yes      | —       | Message body: plain text or HTML, depending on `format`   |
| `format`  | no       | `plain` | `plain` for `text/plain`, or `html` for `text/html`       |


When `format` is `html`, pass HTML markup in `body` (for example `<p>Hello</p>`). Gmail renders it as HTML in the recipient's client. When omitted or set to `plain`, the body is sent as plain text.

Send uses the Gmail API `users.messages.send` endpoint with an RFC 2822 MIME message encoded as base64url. Messages are sent from the authenticated Google account.

