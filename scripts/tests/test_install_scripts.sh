#!/bin/sh
# Tests for scripts/install.sh and scripts/uninstall.sh. Run: sh scripts/tests/test_install_scripts.sh
# Every test runs in a throw-away HOME, so nothing on the real machine is touched.
set -u
SCRIPTS_DIR="$(cd "$(dirname "$0")/.." && pwd)"
FAILURES=0
TESTS=0

# Prints a failure line and counts it.
fail_test() {
    echo "  FAIL: $*"
    FAILURES=$((FAILURES + 1))
}

# assert_equals <expected> <actual> <description>
assert_equals() {
    [ "$1" = "$2" ] || fail_test "$3 (expected '$1', got '$2')"
}

# assert_exists <path> <description> / assert_missing <path> <description>
assert_exists() { [ -e "$1" ] || [ -L "$1" ] || fail_test "$2 ($1 should exist)"; }
assert_missing() { { [ ! -e "$1" ] && [ ! -L "$1" ]; } || fail_test "$2 ($1 should not exist)"; }

# Runs the test function $1 in a fresh sandbox with both scripts' environment pointed into it.
run_test() {
    TESTS=$((TESTS + 1))
    echo "$1"
    SANDBOX="$(mktemp -d)"
    export HOME="$SANDBOX/home" RUST_BOT_INSTALL_DIR="$SANDBOX/home/share/rust-bot"
    export RUST_BOT_BIN_DIR="$SANDBOX/home/bin" RUST_BOT_DATA_DIR="$SANDBOX/home/.rust-bot"
    export RUST_BOT_SOURCE_ONLY=1 RUST_BOT_PURGE=no
    mkdir -p "$HOME"
    ( "$1" )
    status=$?
    [ "$status" -eq 0 ] || { echo "  FAIL: exited with status $status"; FAILURES=$((FAILURES + 1)); }
    rm -rf "$SANDBOX"
}

# Creates a fake install directory at $1 whose rust-bot prints "rust-bot <version $2>".
make_fake_install() {
    mkdir -p "$1"
    printf '#!/bin/sh\necho "rust-bot %s"\n' "$2" > "$1/rust-bot"
    chmod +x "$1/rust-bot"
}

# Succeeds when `ln -s` makes real symlinks (Git Bash on Windows copies files instead).
symlinks_supported() {
    ln -s "$0" "$SANDBOX/probe-link" 2>/dev/null && [ -L "$SANDBOX/probe-link" ]
}

load_install() { . "$SCRIPTS_DIR/install.sh"; }
load_uninstall() { . "$SCRIPTS_DIR/uninstall.sh"; }

test_installed_version_empty_without_install() {
    load_install
    assert_equals "" "$(installed_version)" "no install means no version"
}

test_installed_version_reads_version_flag() {
    load_install
    make_fake_install "$INSTALL_DIR" "0.3.7"
    assert_equals "0.3.7" "$(installed_version)" "version parsed from --version"
}

test_swap_install_dir_fresh_install() {
    load_install
    make_fake_install "$SANDBOX/staged" "1.0.0"
    swap_install_dir "$SANDBOX/staged"
    assert_equals "1.0.0" "$(installed_version)" "fresh install"
    assert_missing "$INSTALL_DIR.old" "no backup left"
}

test_swap_install_dir_replaces_previous() {
    load_install
    make_fake_install "$INSTALL_DIR" "1.0.0"
    make_fake_install "$SANDBOX/staged" "2.0.0"
    swap_install_dir "$SANDBOX/staged"
    assert_equals "2.0.0" "$(installed_version)" "updated version"
    assert_missing "$INSTALL_DIR.old" "backup removed after success"
}

test_swap_install_dir_restores_previous_on_failure() {
    load_install
    make_fake_install "$INSTALL_DIR" "1.0.0"
    # fail() exits the subshell, so capture the call in one; the staged dir does not exist.
    ( swap_install_dir "$SANDBOX/missing" ) >/dev/null 2>&1 || true
    assert_equals "1.0.0" "$(installed_version)" "previous install restored"
}

test_uninstall_removes_matching_bin_link_only() {
    symlinks_supported || { echo "  skipped: no symlink support"; return 0; }
    load_uninstall
    make_fake_install "$INSTALL_DIR" "1.0.0"
    mkdir -p "$BIN_DIR"
    ln -s "$INSTALL_DIR/rust-bot" "$BIN_DIR/rust-bot"
    remove_bin_link >/dev/null
    assert_missing "$BIN_DIR/rust-bot" "own link removed"
    ln -s /bin/sh "$BIN_DIR/rust-bot"
    remove_bin_link >/dev/null
    assert_exists "$BIN_DIR/rust-bot" "foreign link kept"
}

test_remove_path_line_only_removes_installer_line() {
    load_uninstall
    printf 'alias a=b\nexport PATH="%s:$PATH"\nexport OTHER=1\n' "$BIN_DIR" > "$HOME/.profile"
    remove_path_line "$HOME/.profile" >/dev/null
    assert_equals "alias a=b
export OTHER=1" "$(cat "$HOME/.profile")" "only the installer line removed"
}

test_remove_path_line_ignores_missing_profile() {
    load_uninstall
    remove_path_line "$HOME/.does-not-exist" >/dev/null
    assert_missing "$HOME/.does-not-exist" "missing profile not created"
}

test_ensure_on_path_then_remove_path_line_round_trip() {
    load_install
    SHELL=/bin/bash PATH="/usr/bin:/bin" ensure_on_path >/dev/null
    grep -qF "$BIN_DIR" "$HOME/.bashrc" || fail_test "installer wrote the PATH line"
    load_uninstall
    remove_path_line "$HOME/.bashrc" >/dev/null
    grep -qF "$BIN_DIR" "$HOME/.bashrc" && fail_test "uninstaller removed the PATH line"
    return 0
}

test_uninstall_keeps_data_by_default() {
    load_uninstall
    mkdir -p "$DATA_DIR" && echo secret > "$DATA_DIR/config.json"
    RUST_BOT_PURGE=no purge_data_if_confirmed >/dev/null
    assert_exists "$DATA_DIR/config.json" "data kept"
}

test_uninstall_purges_data_when_confirmed() {
    load_uninstall
    mkdir -p "$DATA_DIR" && echo secret > "$DATA_DIR/config.json"
    RUST_BOT_PURGE=yes purge_data_if_confirmed >/dev/null
    assert_missing "$DATA_DIR" "data removed"
}

test_uninstall_main_removes_everything_but_data() {
    symlinks_supported || { echo "  skipped: no symlink support"; return 0; }
    load_uninstall
    make_fake_install "$INSTALL_DIR" "1.0.0"
    mkdir -p "$BIN_DIR" "$DATA_DIR"
    ln -s "$INSTALL_DIR/rust-bot" "$BIN_DIR/rust-bot"
    main >/dev/null
    assert_missing "$INSTALL_DIR" "install dir removed"
    assert_missing "$BIN_DIR/rust-bot" "link removed"
    assert_exists "$DATA_DIR" "data kept"
}

test_uninstall_script_can_delete_the_dir_it_runs_from() {
    make_fake_install "$RUST_BOT_INSTALL_DIR" "1.0.0"
    cp "$SCRIPTS_DIR/uninstall.sh" "$RUST_BOT_INSTALL_DIR/uninstall.sh"
    RUST_BOT_SOURCE_ONLY="" sh "$RUST_BOT_INSTALL_DIR/uninstall.sh" >/dev/null || fail_test "script exits cleanly"
    assert_missing "$RUST_BOT_INSTALL_DIR" "install dir removed while the script ran from it"
}

for test_name in $(sed -n 's/^\(test_[a-z_]*\)() {$/\1/p' "$0"); do
    run_test "$test_name"
done

echo "$TESTS tests, $FAILURES failure(s)"
[ "$FAILURES" -eq 0 ]
