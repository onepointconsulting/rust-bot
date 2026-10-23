#!/bin/sh
# Uninstaller for rust-bot (Linux / macOS), the counterpart of install.sh.
#
#   curl -fsSL https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/uninstall.sh | sh
#
# Removes the install directory, the `rust-bot` link in the bin directory and the PATH line
# install.sh added to the shell profile. The config and workspace in ~/.rust-bot/ are kept
# unless you confirm their deletion when asked (RUST_BOT_PURGE=yes|no answers for you).
# Set RUST_BOT_SOURCE_ONLY=1 to load the functions without uninstalling (used by the tests).
set -eu

# These defaults must stay in sync with install.sh.
INSTALL_DIR="${RUST_BOT_INSTALL_DIR:-$HOME/.local/share/rust-bot}"
if [ "$(id -u)" -eq 0 ]; then
    DEFAULT_BIN_DIR="/usr/local/bin"
else
    DEFAULT_BIN_DIR="$HOME/.local/bin"
fi
BIN_DIR="${RUST_BOT_BIN_DIR:-$DEFAULT_BIN_DIR}"
DATA_DIR="${RUST_BOT_DATA_DIR:-$HOME/.rust-bot}"

# Removes the $BIN_DIR/rust-bot link, but only when it points into $INSTALL_DIR.
remove_bin_link() {
    link="$BIN_DIR/rust-bot"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$INSTALL_DIR/rust-bot" ]; then
        rm -f "$link"
        echo "Removed $link"
    fi
}

# Removes the install directory.
remove_install_dir() {
    if [ -d "$INSTALL_DIR" ]; then
        rm -rf "$INSTALL_DIR"
        echo "Removed $INSTALL_DIR"
    fi
}

# Removes the exact PATH line install.sh appended to the shell profile $1, if present.
remove_path_line() {
    profile="$1"
    path_line="export PATH=\"$BIN_DIR:\$PATH\""
    [ -f "$profile" ] || return 0
    grep -qxF "$path_line" "$profile" || return 0
    remaining="$(mktemp)"
    grep -vxF "$path_line" "$profile" > "$remaining" || true
    cat "$remaining" > "$profile"
    rm -f "$remaining"
    echo "Removed the PATH line from $profile"
}

# Exits successfully (0) when the config/workspace in $DATA_DIR should be deleted.
# RUST_BOT_PURGE=yes|no decides without asking; otherwise the user is asked on the
# terminal, and with no terminal the data is kept.
should_purge_data() {
    case "${RUST_BOT_PURGE:-}" in
        yes | y | 1 | true) return 0 ;;
        no | n | 0 | false) return 1 ;;
    esac
    [ -r /dev/tty ] || return 1
    printf 'Also delete your config, API keys and workspace in %s? [y/N] ' "$DATA_DIR" > /dev/tty
    read -r answer < /dev/tty || return 1
    case "$answer" in
        y | Y | yes | YES) return 0 ;;
        *) return 1 ;;
    esac
}

# Deletes $DATA_DIR when the user agrees, otherwise says it was kept.
purge_data_if_confirmed() {
    [ -d "$DATA_DIR" ] || return 0
    if should_purge_data; then
        rm -rf "$DATA_DIR"
        echo "Removed $DATA_DIR"
    else
        echo "Kept $DATA_DIR (delete it manually, or set RUST_BOT_PURGE=yes, to remove your data)."
    fi
}

main() {
    remove_bin_link
    remove_install_dir
    remove_path_line "$HOME/.zshrc"
    remove_path_line "$HOME/.bashrc"
    remove_path_line "$HOME/.profile"
    purge_data_if_confirmed
    echo "rust-bot was uninstalled. Restart running rust-bot processes and open a new terminal."
}

[ -n "${RUST_BOT_SOURCE_ONLY:-}" ] || main
