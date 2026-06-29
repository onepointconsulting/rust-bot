#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

cargo build -r
./target/release/rust-bot agent --config ./configs/anthropic/config_gmail_anthropic.json --no-logs
