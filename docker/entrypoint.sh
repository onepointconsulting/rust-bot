#!/bin/sh
# Container entrypoint: make sure a config exists, validate it, then run rust-bot.
#
# The config lives on the /data volume (RUST_BOT_CONFIG). On the first start it is seeded
# from the template baked into the image. Secrets are never written into it: the template
# refers to them as ${VAR} placeholders, which rust-bot expands from the environment.
set -eu

: "${RUST_BOT_CONFIG:=/data/config.json}"
export RUST_BOT_CONFIG
TEMPLATE=/usr/share/rust-bot/config.template.json

if [ ! -f "$RUST_BOT_CONFIG" ]; then
    mkdir -p "$(dirname "$RUST_BOT_CONFIG")"
    cp "$TEMPLATE" "$RUST_BOT_CONFIG"
    echo "entrypoint: created $RUST_BOT_CONFIG from the image template" >&2
fi

# Fail with a readable message (bad JSON, unset ${VAR}) instead of a panic mid-start.
case "${1:-}" in
    gateway | api)
        rust-bot check-config --config "$RUST_BOT_CONFIG"
        ;;
esac

exec rust-bot "$@"
