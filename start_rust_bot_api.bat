cargo build -r
.\target\release\rust-bot api --config ./configs/anthropic/config_gmail_anthropic.json

@REM cargo run --bin rust-bot -- api --host 127.0.0.1 --port 8900 --config ./configs/anthropic/config_gmail_anthropic_api.json