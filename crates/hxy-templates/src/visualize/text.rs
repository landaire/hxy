//! `[[hex::visualize("text", encoding?)]]` decode: turn the field's
//! bytes into a display string. Default encoding is UTF-8; common
//! single-byte alternatives (ASCII, Latin-1) and UTF-16 are
//! recognised. Non-decodable byte sequences get the U+FFFD
//! replacement so the surrounding readable text is still legible.

/// Cap how much the frontends decode/render at once.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Decode `view` (already truncated to [`MAX_BYTES`] by the caller)
/// with the given lowercased encoding name. `Err` carries the
/// localized unknown-encoding message.
pub fn decode(view: &[u8], encoding: &str) -> Result<String, String> {
    Ok(match encoding {
        "utf-8" | "utf8" => String::from_utf8_lossy(view).into_owned(),
        "ascii" => view.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect(),
        "latin-1" | "latin1" | "iso-8859-1" => view.iter().map(|&b| b as char).collect(),
        "utf-16-le" | "utf-16le" | "utf16le" => decode_utf16(view, true),
        "utf-16-be" | "utf-16be" | "utf16be" => decode_utf16(view, false),
        other => return Err(hxy_i18n::t_args("visualizer-text-unknown-encoding", &[("name", other)])),
    })
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let v = if little_endian {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        units.push(v);
    }
    String::from_utf16_lossy(&units)
}
