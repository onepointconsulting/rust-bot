# Gmail support

Rust Bot can expose two Gmail agent tools when enabled in config:

- `gmail` — reads messages from the user's inbox (read-only)
- `gmail_email_send` — sends an email to a recipient (plain text or HTML)

Access is granted through Google OAuth; credentials are stored on disk and reused by the agent. Both tools share the same `client_secret.json` and `token_cache.json` paths from config.

## Google Cloud setup

1. Open the [Google Cloud Console](https://console.cloud.google.com/) and create or select a project.
2. Enable the **Gmail API** for that project.
3. Configure the **OAuth consent screen** (External or Internal, depending on your use case). If you plan to use send, ensure the consent screen allows the **Send email on your behalf** scope (`gmail.send`).
4. Create an OAuth **Desktop app** client and download the client secret JSON.
5. Ensure the client allows the redirect URI `http://localhost:8080` (required for the installed-app flow used by `gmail-auth`).

Save the downloaded file as `client_secret.json` before running the OAuth helper (see below). The `gmail-auth` helper looks for `./credentials/client_secret.json` by default; the agent expects credential files under `~/.rust-bot/credentials/` unless you override the paths in config (see [Enabling the Gmail tools](#enabling-the-gmail-tools)).

> Credential files contain secrets and are gitignored. Do not commit `client_secret.json` or `token_cache.json`.



## OAuth helper (`gmail-auth`)

The `gmail-auth` binary is a standalone utility (not part of the main agent loop) that walks through Google login, requests Gmail **read** and **send** access, and writes a `token_cache.json` file containing the refresh and access tokens, in the same directory as `client_secret.json`. Run it once per machine (or again if tokens are revoked or scopes change).

**Prerequisites:** have the `client_secret.json` file downloaded from Google Cloud Console available (see [Google Cloud setup](#google-cloud-setup)).

**Usage:**

```
gmail-auth [CLIENT_SECRET_PATH]
```

The first (optional) argument is the location of the `client_secret.json` file. When omitted, it defaults to `./credentials/client_secret.json`. If the file does not exist, the helper prints an error and exits with a non-zero code.

```bash
# Use the default location (./credentials/client_secret.json)
./target/release/gmail-auth

# Use a client secret stored elsewhere
./target/release/gmail-auth /path/to/client_secret.json
```

When running through Cargo, put the argument after `--`:

```bash
cargo run --bin gmail-auth -- /path/to/client_secret.json
```

```bash
# Run the OAuth flow (opens a browser, listens on localhost:8080)
cargo run --bin gmail-auth

# Or build a release binary
cargo build --release --bin gmail-auth
./target/release/gmail-auth
```

On Windows (PowerShell):

```ps1
cargo run --bin gmail-auth
cargo build --release --bin gmail-auth
.\target\release\gmail-auth.exe
# With an explicit client secret location
.\target\release\gmail-auth.exe C:\path\to\client_secret.json
```

What happens:

1. A local HTTP server on port **8080** receives the OAuth callback (no manual code copy-paste).
2. You sign in with Google and grant Gmail read and send permission.
3. Tokens are persisted to `token_cache.json` in the same directory as the `client_secret.json` file you passed in (for the default location, `./credentials/token_cache.json`). The chosen path is printed at startup.
4. The helper fetches a few inbox subjects to confirm read access works.

If you previously authenticated with read-only scope, delete the old `token_cache.json` and run `gmail-auth` again so the cache includes `gmail.send`.

Because both files end up in the same folder, you can copy that folder's contents to the credential directory the agent uses (defaults shown):

```bash
mkdir -p ~/.rust-bot/credentials
cp ./credentials/client_secret.json ~/.rust-bot/credentials/
cp ./credentials/token_cache.json ~/.rust-bot/credentials/
```

If your config points elsewhere (for example `configs/openai-compat/config_gmail.json` uses `~/.rust-bot/workspace/credentials/`), copy the files to those paths instead.

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

There is no separate flag for send — when `enable` is `true`, the agent registers `gmail` and `gmail_email_send`.

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

