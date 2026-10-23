#!/bin/sh
# One-line installer for rust-bot (Linux / macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/install.sh | sh
#
# Downloads the latest GitHub release, installs it under ~/.local/share/rust-bot,
# links `rust-bot` into ~/.local/bin (/usr/local/bin for root) and runs `rust-bot onboard` from $HOME so the
# config lands in ~/.rust-bot/. Set RUST_BOT_VERSION (e.g. 0.3.3) to pin a version.
#
# Re-running the script on an existing install updates it in place: the config in
# ~/.rust-bot/ is untouched, onboarding is skipped, and nothing happens when the
# requested version is already installed (set RUST_BOT_FORCE=1 to reinstall anyway).
# Set RUST_BOT_SOURCE_ONLY=1 to load the functions without installing (used by the tests).
set -eu

REPO="onepointconsulting/rust-bot"
INSTALL_DIR="${RUST_BOT_INSTALL_DIR:-$HOME/.local/share/rust-bot}"
# root installs into /usr/local/bin, which is already on every shell's PATH.
if [ "$(id -u)" -eq 0 ]; then
    DEFAULT_BIN_DIR="/usr/local/bin"
else
    DEFAULT_BIN_DIR="$HOME/.local/bin"
fi
BIN_DIR="${RUST_BOT_BIN_DIR:-$DEFAULT_BIN_DIR}"

# Prints an error message and aborts.
fail() {
    echo "error: $*" >&2
    exit 1
}

# Prints the release platform suffix (e.g. linux-x86_64) for this machine.
detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Linux/x86_64) echo "linux-x86_64" ;;
        Linux/aarch64 | Linux/arm64) echo "linux-aarch64" ;;
        Darwin/arm64) echo "macos-aarch64" ;;
        *) fail "no pre-built package for $os/$arch" ;;
    esac
}

# Prints the version to install: $RUST_BOT_VERSION or the latest release tag without the "v".
resolve_version() {
    if [ -n "${RUST_BOT_VERSION:-}" ]; then
        echo "${RUST_BOT_VERSION#v}"
        return
    fi
    curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1
}

# Prints the version of the rust-bot installed in $INSTALL_DIR, or nothing when there is none.
installed_version() {
    [ -x "$INSTALL_DIR/rust-bot" ] || return 0
    "$INSTALL_DIR/rust-bot" --version 2>/dev/null | awk '{ print $NF }' | head -n 1
}

# Replaces $INSTALL_DIR with the directory $1. The previous install is kept aside until
# the new one is in place and restored if the move fails, so a failed update never
# leaves the machine without a working rust-bot.
swap_install_dir() {
    staged_dir="$1"
    backup_dir="$INSTALL_DIR.old"
    rm -rf "$backup_dir"
    mkdir -p "$(dirname "$INSTALL_DIR")"
    [ ! -e "$INSTALL_DIR" ] || mv "$INSTALL_DIR" "$backup_dir"
    if ! mv "$staged_dir" "$INSTALL_DIR"; then
        [ ! -e "$backup_dir" ] || mv "$backup_dir" "$INSTALL_DIR"
        fail "could not install into $INSTALL_DIR; the previous install was restored"
    fi
    rm -rf "$backup_dir"
}

# Downloads and unpacks the release for $1 (version) / $2 (platform) into $INSTALL_DIR.
install_release() {
    version="$1"
    platform="$2"
    stage_name="rust-bot-$version-$platform"
    work_dir="$(mktemp -d)"
    trap 'rm -rf "$work_dir"' EXIT
    echo "Downloading rust-bot $version ($platform)..."
    curl -fsSL "https://github.com/$REPO/releases/download/v$version/$stage_name.tar.gz"         -o "$work_dir/rust-bot.tar.gz"
    tar xzf "$work_dir/rust-bot.tar.gz" -C "$work_dir"
    swap_install_dir "$work_dir/$stage_name"
    mkdir -p "$BIN_DIR"
    ln -sf "$INSTALL_DIR/rust-bot" "$BIN_DIR/rust-bot"
}

# Ensures $BIN_DIR is on PATH: makes it available to this script and, if it is missing,
# persists it in the user's shell profile (a piped script cannot change the parent shell).
ensure_on_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) return ;;
    esac
    export PATH="$BIN_DIR:$PATH"
    case "${SHELL:-}" in
        */zsh) profile="$HOME/.zshrc" ;;
        */bash) profile="$HOME/.bashrc" ;;
        *) profile="$HOME/.profile" ;;
    esac
    path_line="export PATH=\"$BIN_DIR:\$PATH\""
    grep -qsF "$path_line" "$profile" || echo "$path_line" >> "$profile"
    echo "Added $BIN_DIR to PATH in $profile. Open a new terminal or run: $path_line"
}

# Installs or updates rust-bot to the requested version; onboarding only runs on a fresh install.
main() {
    command -v curl >/dev/null 2>&1 || fail "curl is required"
    version="$(resolve_version)"
    [ -n "$version" ] || fail "could not determine the latest version"
    previous_version="$(installed_version)"
    if [ "$previous_version" = "$version" ] && [ -z "${RUST_BOT_FORCE:-}" ]; then
        echo "rust-bot $version is already installed (set RUST_BOT_FORCE=1 to reinstall)."
        return
    fi
    install_release "$version" "$(detect_platform)"
    ensure_on_path

    if [ -n "$previous_version" ]; then
        echo "Updated rust-bot $previous_version -> $version. Your config in ~/.rust-bot/ was left untouched."
        echo "Restart any running rust-bot gateway to pick up the new version."
        return
    fi
    echo "Installed rust-bot $version. To remove it later, run: sh $INSTALL_DIR/uninstall.sh"
    echo "Starting onboarding..."
    cd "$HOME"
    # stdin is the script when piped from curl, so attach the terminal for the prompts.
    if [ -r /dev/tty ]; then
        "$BIN_DIR/rust-bot" onboard </dev/tty
    else
        echo "No terminal available; run 'rust-bot onboard' to finish setup."
    fi
}

[ -n "${RUST_BOT_SOURCE_ONLY:-}" ] || main
