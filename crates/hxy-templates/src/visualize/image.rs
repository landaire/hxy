//! `[[hex::visualize("image")]]` decode: sniff and decode the
//! field's bytes as an image (PNG, JPEG, GIF, BMP, TIFF, WebP) into
//! a texture-ready RGBA buffer. The frontends upload the buffer and
//! cache the resulting texture keyed by [`fingerprint`], so a re-run
//! that produces the same image keeps the same GPU texture.

/// Cheap blake3 content fingerprint the frontends key their decoded
/// image / texture caches by.
pub fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Decode `bytes` as an image. Returns `(width, height, rgba)` with
/// tightly packed 8-bit RGBA pixels; the error is the decoder's
/// message (the frontends wrap it in a localized label).
pub fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let img = ::image::load_from_memory(bytes).map_err(|e| format!("{e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((w, h, rgba.into_raw()))
}
