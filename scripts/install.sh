#!/bin/sh
# One-line installer for rust-bot (Linux / macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/install.sh | sh
#
# Downloads the latest GitHub release, installs it under ~/.local/share/rust-bot,
# links `rust-bot` into ~/.local/bin and runs `rust-bot onboard` from $HOME so the
# config lands in ~/.rust-bot/. Set RUST_BOT_VERSION (e.g. 0.3.3) to pin a version.
set -eu

REPO="onepointconsulting/rust-bot"
INSTALL_DIR="${RUST_BOT_INSTALL_DIR:-$HOME/.local/share/rust-bot}"
BIN_DIR="${RUST_BOT_BIN_DIR:-$HOME/.local/bin}"

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

# Downloads and unpacks the release for $1 (version) / $2 (platform) into $INSTALL_DIR.
install_release() {
    version="$1"
    platform="$2"
    stage_name="rust-bot-$version-$platform"
    work_dir="$(mktemp -d)"
    trap 'rm -rf "$work_dir"' EXIT
    echo "Downloading rust-bot $version ($platform)..."
    curl -fsSL "https://github.com/$REPO/releases/download/v$version/$stage_name.tar.gz" \
        -o "$work_dir/rust-bot.tar.gz"
    tar xzf "$work_dir/rust-bot.tar.gz" -C "$work_dir"
    rm -rf "$INSTALL_DIR"
    mkdir -p "$(dirname "$INSTALL_DIR")" "$BIN_DIR"
    mv "$work_dir/$stage_name" "$INSTALL_DIR"
    ln -sf "$INSTALL_DIR/rust-bot" "$BIN_DIR/rust-bot"
}

# Warns when $BIN_DIR is not on PATH.
check_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) echo "Add $BIN_DIR to your PATH, e.g.: export PATH=\"$BIN_DIR:\$PATH\"" ;;
    esac
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
version="$(resolve_version)"
[ -n "$version" ] || fail "could not determine the latest version"
install_release "$version" "$(detect_platform)"
check_path

echo "Installed rust-bot $version. Starting onboarding..."
cd "$HOME"
# stdin is the script when piped from curl, so attach the terminal for the prompts.
if [ -r /dev/tty ]; then
    "$BIN_DIR/rust-bot" onboard </dev/tty
else
    echo "No terminal available; run 'rust-bot onboard' to finish setup."
fi
