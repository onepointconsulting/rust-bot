#!/usr/bin/env bash
set -euo pipefail

# Repo root = parent of scripts/ (works from any cwd when invoking this file)
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Trunk first so `cargo build` of rust-bot can embed websockets-chat/dist.
cd web-chat
trunk build --release  || exit 1

cd ..

cd websockets-chat
trunk build --release || exit 1

cd ..
cargo build -r
