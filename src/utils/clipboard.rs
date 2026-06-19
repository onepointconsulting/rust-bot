use std::path::PathBuf;

use arboard::Clipboard;
use uuid::Uuid;

pub const IMAGE_PASTE_SENTINEL: &str = "\0[PASTED_IMAGE]\0";

pub const TEXT_PASTE_SENTINEL: &str = "\0[PASTED_TEXT]\0";

pub const TEXT_PASTE_COMMAND: &str = "\0[PASTE_TEXT_CMD]\0";

#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub path: PathBuf,
    pub width: usize,
    pub height: usize,
}

fn save_rgba_as_png(
    width: usize,
    height: usize,
    rgba: &[u8],
    path: &std::path::Path,
) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    log::info!("Writing PNG to {}", path.display());
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;
    png_writer.write_image_data(rgba)?;
    Ok(())
}

/// Read an image from clipboard, persist it as a temporary PNG, and return metadata.
///
/// Returns `None` when no image is available or the clipboard API is unavailable.
pub fn try_get_clipboard_image() -> Option<ClipboardImage> {
    let mut clipboard = Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    let path = std::env::temp_dir().join(format!("rust-bot-paste-{}.png", Uuid::new_v4()));
    save_rgba_as_png(image.width, image.height, image.bytes.as_ref(), &path).ok()?;
    Some(ClipboardImage {
        path,
        width: image.width,
        height: image.height,
    })
}

pub fn try_get_clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    let text = clipboard.get_text().ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_rgba_as_png_writes_file() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let out = tmp.path().join("img.png");
        // 1x1 red pixel RGBA
        let px = [255u8, 0, 0, 255];
        save_rgba_as_png(1, 1, &px, &out).expect("png written");
        let bytes = std::fs::read(&out).expect("read png");
        assert!(
            bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "must be a PNG file"
        );
    }
}
