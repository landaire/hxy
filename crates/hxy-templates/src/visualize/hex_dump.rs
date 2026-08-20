//! `[[hex::visualize("hex_viewer")]]` formatting: render bytes as a
//! fixed-pitch hex dump string. Inline equivalent of opening the
//! parent file at the field's offset, but focused on a small slice.

use std::fmt::Write;

/// Bytes per row in the dump. Matches the hex view's default width
/// so the user can mentally line up offsets without doing math.
pub const COLS: usize = 16;
/// Cap how much the frontends render so a 100MB field doesn't blow
/// up the UI thread. The user opened a *visualizer*, not the main
/// editor.
pub const MAX_BYTES: usize = 64 * 1024;

pub fn format_dump(base_offset: u64, bytes: &[u8]) -> String {
    let rows = bytes.len().div_ceil(COLS);
    let mut out = String::with_capacity(rows * (10 + COLS * 3 + 2 + COLS + 1));
    for (row_idx, chunk) in bytes.chunks(COLS).enumerate() {
        let off = base_offset + (row_idx * COLS) as u64;
        let _ = write!(out, "{off:08X}  ");
        for col in 0..COLS {
            if col < chunk.len() {
                let _ = write!(out, "{:02X} ", chunk[col]);
            } else {
                out.push_str("   ");
            }
            if col == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for &b in chunk {
            out.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
        }
        out.push('\n');
    }
    out
}
