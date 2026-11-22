//! Stage static UI assets into `$OUT_DIR` so `rust-embed` can compile them in.
//!
//! `websockets-chat/dist` is gitignored and may be missing during `cargo test`.
//! When `index.html` is absent, a one-file stub is written so the crate still
//! compiles; [`crate::utils::embedded_gateway_ui::is_available`] then reports
//! false because the stub has no `.wasm`.
//!
//! Also stages the optional built-in Gmail OAuth client secret (see
//! `stage_gmail_client_secret`).
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Build-time environment variable naming the Gmail OAuth Desktop client to
/// embed: either a path to `client_secret.json` or the JSON itself.
const GMAIL_CLIENT_SECRET_ENV: &str = "RUST_BOT_GMAIL_CLIENT_SECRET";

/// File in `$OUT_DIR` holding the embedded Gmail client secret (empty when none).
const GMAIL_CLIENT_SECRET_OUT_FILE: &str = "gmail_default_client_secret.json";

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    stage_ui("gateway-ui", Path::new("websockets-chat/dist"), &out_dir);
    // Later: stage_ui("api-ui", Path::new("web-chat/dist"), &out_dir);
    println!("cargo:rerun-if-changed=websockets-chat/dist");
    println!("cargo:rerun-if-changed=websockets-chat/dist/index.html");
    stage_gmail_client_secret(&out_dir);
}

/// Write the built-in Gmail client secret named by [`GMAIL_CLIENT_SECRET_ENV`]
/// to `$OUT_DIR`, or an empty file when the variable is unset or blank, so
/// `include_str!` always finds it. A value starting with `{` is taken as the
/// JSON itself (handy for CI secrets); anything else is a file path.
fn stage_gmail_client_secret(out_dir: &Path) {
    println!("cargo:rerun-if-env-changed={GMAIL_CLIENT_SECRET_ENV}");
    let configured = env::var(GMAIL_CLIENT_SECRET_ENV).unwrap_or_default();
    let configured = configured.trim();
    let client_secret_json = if configured.is_empty() {
        String::new()
    } else if configured.starts_with('{') {
        configured.to_string()
    } else {
        println!("cargo:rerun-if-changed={configured}");
        fs::read_to_string(configured).unwrap_or_else(|err| {
            panic!("{GMAIL_CLIENT_SECRET_ENV}: cannot read {configured}: {err}")
        })
    };
    fs::write(
        out_dir.join(GMAIL_CLIENT_SECRET_OUT_FILE),
        client_secret_json,
    )
    .expect("write embedded Gmail client secret");
}

/// Copy `src` into `$OUT_DIR/{name}`, skipping Trunk's `.stage/` directory.
/// Writes a stub `index.html` when the real bundle is missing.
fn stage_ui(name: &str, src: &Path, out_dir: &Path) {
    let dest = out_dir.join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).unwrap_or_else(|err| {
            panic!("failed to clear staged UI dir {}: {err}", dest.display())
        });
    }
    fs::create_dir_all(&dest)
        .unwrap_or_else(|err| panic!("failed to create staged UI dir {}: {err}", dest.display()));

    if src.join("index.html").is_file() {
        copy_dir_excluding_stage(src, &dest).unwrap_or_else(|err| {
            panic!(
                "failed to copy {} -> {}: {err}",
                src.display(),
                dest.display()
            )
        });
    } else {
        fs::write(
            dest.join("index.html"),
            "<!-- rust-bot: websockets-chat dist not built; run `trunk build --release` in websockets-chat/ -->\n",
        )
        .expect("write stub index.html");
    }
}

fn copy_dir_excluding_stage(src: &Path, dest: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".stage" {
            continue;
        }
        let src_path = entry.path();
        let dest_path = dest.join(&name);
        if src_path.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_dir_excluding_stage(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}
