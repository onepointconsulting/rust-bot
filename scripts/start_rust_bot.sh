#!/usr/bin/env bash
set -euo pipefail

# Repo root = parent of scripts/ (works from any cwd when invoking this file)
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build -r
./target/release/rust-bot agent --config ./configs/anthropic/config_gmail_anthropic.json --no-logs
