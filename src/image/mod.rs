//! Icon / thumbnail processing shared across domain crates that upload media
//! to Roblox.
//!
//! Pipeline: load a PNG/JPEG from disk or bytes, optionally apply alpha bleed
//! (fixes ringing artifacts when Roblox resizes images with transparency),
//! re-encode as PNG. Plus a [`hash_bytes`] helper so callers can compute a
//! stable hash that matches what the upload code will see.
//!
//! The alpha-bleed implementation is adapted from
//! [Asphalt](https://github.com/jackTabsCode/asphalt) (MIT), which itself
//! derives from [Tarmac](https://github.com/Roblox/tarmac) (MIT). Both
//! notices are in `THIRD-PARTY-NOTICES.md`.

mod alpha_bleed;

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use image::ImageFormat;

/// Load an image from disk, optionally apply alpha bleed, and return the
/// processed PNG bytes ready for upload.
pub fn process_image(path: &Path, bleed: bool) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read image: {}", path.display()))?;
    process_bytes(&bytes, bleed)
        .with_context(|| format!("Failed to process image: {}", path.display()))
}

/// Apply alpha bleed (if enabled) to in-memory image bytes and re-encode as
/// PNG. Use this when you already have the bytes (e.g. just downloaded an
/// asset and want the same pipeline applied for hash comparison).
pub fn process_bytes(bytes: &[u8], bleed: bool) -> Result<Vec<u8>> {
    let mut img = image::load_from_memory(bytes).context("Failed to decode image bytes")?;

    if bleed {
        alpha_bleed::alpha_bleed(&mut img);
    }

    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .context("Failed to encode image as PNG")?;

    Ok(buf)
}

/// Compute the blake3 hash of processed PNG bytes. Used by domain crates to
/// decide whether an icon changed without re-uploading.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
