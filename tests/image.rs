#![allow(clippy::unwrap_used)]

use std::io::Cursor;

use image::{ImageBuffer, ImageFormat, Rgba};
use rbx_core::image::{hash_bytes, process_bytes, process_image};
use tempfile::tempdir;

/// Build a 2x2 PNG with one fully-transparent pixel, returned as bytes.
fn build_test_png(transparent_corner_color: Rgba<u8>) -> Vec<u8> {
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(2, 2);
    img.put_pixel(0, 0, Rgba([255, 0, 0, 255])); // opaque red
    img.put_pixel(1, 0, Rgba([0, 255, 0, 255])); // opaque green
    img.put_pixel(0, 1, Rgba([0, 0, 255, 255])); // opaque blue
    img.put_pixel(1, 1, transparent_corner_color); // transparent (alpha=0)

    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .unwrap();
    buf
}

#[test]
fn process_bytes_without_bleed_preserves_transparent_color_channels() {
    // Transparent pixel with garbage RGB. Without bleed, those RGB values
    // are preserved.
    let bytes = build_test_png(Rgba([200, 100, 50, 0]));
    let out = process_bytes(&bytes, false).unwrap();
    let img = image::load_from_memory(&out).unwrap().into_rgba8();
    let pixel = img.get_pixel(1, 1);
    // Alpha stays 0.
    assert_eq!(pixel[3], 0);
    // RGB is what we put in (no bleed).
    assert_eq!(pixel[0], 200);
    assert_eq!(pixel[1], 100);
    assert_eq!(pixel[2], 50);
}

#[test]
fn process_bytes_with_bleed_overwrites_transparent_pixel_rgb() {
    // Bleed averages the RGB of equidistant opaque neighbors when no single
    // closest neighbor wins (here the transparent corner has 3 opaque
    // neighbors all 1 step away: red/green/blue). We don't pin which color
    // wins; we only assert that:
    //   - alpha stays 0 (bleed only touches RGB)
    //   - RGB is NOT the original transparent placeholder (something happened)
    let bytes = build_test_png(Rgba([0, 0, 0, 0]));
    let out = process_bytes(&bytes, true).unwrap();
    let img = image::load_from_memory(&out).unwrap().into_rgba8();
    let pixel = img.get_pixel(1, 1);
    assert_eq!(pixel[3], 0, "bleed must not change alpha");
    // The placeholder we wrote was (0, 0, 0). After bleed, at least one
    // channel must be non-zero (because at least one neighbor is non-zero
    // in that channel).
    assert!(
        pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0,
        "bleed should pull non-zero RGB from opaque neighbors, got ({}, {}, {})",
        pixel[0],
        pixel[1],
        pixel[2]
    );
}

#[test]
fn process_image_round_trips_through_disk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.png");
    let bytes = build_test_png(Rgba([0, 0, 0, 0]));
    std::fs::write(&path, &bytes).unwrap();

    let out = process_image(&path, false).unwrap();
    // Output is valid PNG (decodes without error).
    let _ = image::load_from_memory(&out).unwrap();
    assert!(!out.is_empty());
}

#[test]
fn process_image_missing_file_errors_with_path() {
    let err = process_image(std::path::Path::new("does-not-exist.png"), false)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("does-not-exist.png"),
        "error should mention the missing path: {err}"
    );
}

#[test]
fn process_bytes_garbage_input_errors() {
    let err = process_bytes(b"not an image", false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("decode"));
}

#[test]
fn hash_bytes_is_deterministic() {
    let h1 = hash_bytes(b"hello world");
    let h2 = hash_bytes(b"hello world");
    assert_eq!(h1, h2);
}

#[test]
fn hash_bytes_differs_for_different_input() {
    let h1 = hash_bytes(b"hello world");
    let h2 = hash_bytes(b"hello world!");
    assert_ne!(h1, h2);
}

#[test]
fn hash_bytes_is_blake3_hex() {
    // BLAKE3 hex digest is 64 chars (32 bytes).
    let h = hash_bytes(b"x");
    assert_eq!(h.len(), 64);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn bleed_changes_hash_for_image_with_transparency() {
    // The whole point of bleed: same pixels visually, but the RGB of
    // transparent pixels differs, so the hash differs.
    let bytes = build_test_png(Rgba([99, 99, 99, 0]));
    let no_bleed = process_bytes(&bytes, false).unwrap();
    let with_bleed = process_bytes(&bytes, true).unwrap();
    assert_ne!(hash_bytes(&no_bleed), hash_bytes(&with_bleed));
}

#[test]
fn bleed_is_a_noop_on_image_without_transparency() {
    // No transparent pixels -> bleed has nothing to fill, hashes match.
    let bytes = build_test_png(Rgba([10, 20, 30, 255])); // opaque corner
    let no_bleed = process_bytes(&bytes, false).unwrap();
    let with_bleed = process_bytes(&bytes, true).unwrap();
    assert_eq!(hash_bytes(&no_bleed), hash_bytes(&with_bleed));
}
