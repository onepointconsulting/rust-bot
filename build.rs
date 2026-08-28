//! Stage static UI assets into `$OUT_DIR` so `rust-embed` can compile them in.
//!
//! `websockets-chat/dist` is gitignored and may be missing during `cargo test`.
//! When `index.html` is absent, a one-file stub is written so the crate still
//! compiles; [`crate::utils::embedded_gateway_ui::is_available`] then reports
//! false because the stub has no `.wasm`.
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    stage_ui("gateway-ui", Path::new("websockets-chat/dist"), &out_dir);
    // Later: stage_ui("api-ui", Path::new("web-chat/dist"), &out_dir);
    println!("cargo:rerun-if-changed=websockets-chat/dist");
    println!("cargo:rerun-if-changed=websockets-chat/dist/index.html");
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
