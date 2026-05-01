//! Checksum tool.
//!
//! Compute one or more checksums over a byte range in a single I/O
//! pass: read the source in fixed-size chunks, fan each chunk out
//! to every enabled hasher, finalize at end. Six algorithms ship by
//! default (CRC32, MD5, SHA-1, SHA-256, SHA-512, BLAKE3); selection
//! is per-tab and persists across the panel's lifetime.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;

use crate::files::FileId;

/// Read window. Same value as the strings tool -- enough to amortize
/// per-call overhead, small enough to bound memory while we hold
/// chunks live for the parallel hasher feed.
const CHUNK_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Algorithm {
    Crc32,
    Adler32,
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

impl Algorithm {
    pub const ALL: [Algorithm; 7] =
        [Self::Crc32, Self::Adler32, Self::Md5, Self::Sha1, Self::Sha256, Self::Sha512, Self::Blake3];

    pub fn label(self) -> &'static str {
        match self {
            Self::Crc32 => "CRC32",
            Self::Adler32 => "Adler-32",
            Self::Md5 => "MD5",
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
            Self::Sha512 => "SHA-512",
            Self::Blake3 => "BLAKE3",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChecksumConfig {
    /// Algorithms enabled for the next compute. `BTreeSet` so the
    /// stored order is stable across saves and the result rows render
    /// in a predictable sequence.
    pub algorithms: BTreeSet<Algorithm>,
    pub range: ByteRange,
}

impl Default for ChecksumConfig {
    fn default() -> Self {
        // Default ticks the algorithms most users actually want to
        // verify with. CRC32 / MD5 stay off by default -- they're
        // historical, frequently used for downloaded files, and
        // available with one click. SHA-256 / BLAKE3 are the modern
        // strong hashes that should be on by default.
        let mut algorithms = BTreeSet::new();
        algorithms.insert(Algorithm::Sha256);
        algorithms.insert(Algorithm::Blake3);
        Self { algorithms, range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(0)).expect("empty range valid") }
    }
}

#[derive(Clone, Debug)]
pub struct ChecksumResult {
    /// Map keyed by algorithm so the UI can render a stable order.
    /// Values are lowercase hex with no separators.
    pub values: BTreeMap<Algorithm, String>,
    pub computed_at: jiff::Timestamp,
    pub source_len: u64,
    pub config: ChecksumConfig,
}

#[derive(Clone, Debug)]
pub enum ChecksumOutcome {
    Ok(ChecksumResult),
    Err(String),
}

pub struct ChecksumComputation {
    pub inbox: egui_inbox::UiInbox<ChecksumOutcome>,
    pub file_id: FileId,
    pub started: web_time::Instant,
}

#[derive(Default)]
pub struct ChecksumsPanel {
    pub config: ChecksumConfig,
    pub last_result: Option<ChecksumResult>,
    pub running: Option<ChecksumComputation>,
}

/// Streaming hasher dispatcher. Each variant wraps the algorithm's
/// own incremental hasher so the streaming loop can dispatch on the
/// enum once per chunk, not once per algorithm per chunk.
enum Hasher {
    Crc32(crc32fast::Hasher),
    Adler32(adler2::Adler32),
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Blake3(Box<blake3::Hasher>),
}

impl Hasher {
    fn for_algorithm(alg: Algorithm) -> Self {
        match alg {
            Algorithm::Crc32 => Self::Crc32(crc32fast::Hasher::new()),
            Algorithm::Adler32 => Self::Adler32(adler2::Adler32::new()),
            Algorithm::Md5 => Self::Md5(<md5::Md5 as md5::Digest>::new()),
            Algorithm::Sha1 => Self::Sha1(<sha1::Sha1 as sha1::Digest>::new()),
            Algorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            Algorithm::Sha512 => Self::Sha512(sha2::Sha512::new()),
            Algorithm::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Crc32(h) => h.update(bytes),
            Self::Adler32(h) => h.write_slice(bytes),
            Self::Md5(h) => md5::Digest::update(h, bytes),
            Self::Sha1(h) => sha1::Digest::update(h, bytes),
            Self::Sha256(h) => h.update(bytes),
            Self::Sha512(h) => h.update(bytes),
            Self::Blake3(h) => {
                h.update(bytes);
            }
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Crc32(h) => format!("{:08x}", h.finalize()),
            Self::Adler32(h) => format!("{:08x}", h.checksum()),
            Self::Md5(h) => hex_encode(&md5::Digest::finalize(h)),
            Self::Sha1(h) => hex_encode(&sha1::Digest::finalize(h)),
            Self::Sha256(h) => hex_encode(&h.finalize()),
            Self::Sha512(h) => hex_encode(&h.finalize()),
            Self::Blake3(h) => h.finalize().to_hex().to_string(),
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Synchronous checksum compute. Reads the configured range from
/// `source` once, feeding each chunk to every enabled algorithm
/// before advancing. Returns one hex value per enabled algorithm.
pub fn compute(source: &dyn HexSource, config: &ChecksumConfig) -> Result<ChecksumResult, String> {
    let source_len = source.len().get();
    let range_start = config.range.start().get();
    let range_end = config.range.end().get();
    if range_end > source_len {
        return Err(format!("range {range_start}..{range_end} exceeds source length {source_len}"));
    }
    if config.algorithms.is_empty() {
        return Err("no algorithms selected".to_owned());
    }
    let mut hashers: Vec<(Algorithm, Hasher)> =
        config.algorithms.iter().map(|&alg| (alg, Hasher::for_algorithm(alg))).collect();
    let mut offset = range_start;
    while offset < range_end {
        let stop = (offset + CHUNK_BYTES).min(range_end);
        let chunk_range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(stop))
            .map_err(|e| format!("range {offset}..{stop}: {e}"))?;
        let bytes = source.read(chunk_range).map_err(|e| format!("read {offset}..{stop}: {e}"))?;
        for (_alg, hasher) in hashers.iter_mut() {
            hasher.update(&bytes);
        }
        offset = stop;
    }
    let values: BTreeMap<Algorithm, String> =
        hashers.into_iter().map(|(alg, hasher)| (alg, hasher.finalize_hex())).collect();
    Ok(ChecksumResult { values, computed_at: jiff::Timestamp::now(), source_len, config: config.clone() })
}

/// Spin up a checksum worker on the shared background pool.
pub fn spawn_compute(
    ctx: &egui::Context,
    id: FileId,
    source: Arc<dyn HexSource>,
    config: ChecksumConfig,
) -> ChecksumComputation {
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    let started = web_time::Instant::now();
    crate::background::submit(move || {
        let outcome = match compute(&*source, &config) {
            Ok(result) => ChecksumOutcome::Ok(result),
            Err(e) => ChecksumOutcome::Err(e),
        };
        let _ = sender.send(outcome);
    });
    ChecksumComputation { inbox, file_id: id, started }
}

#[derive(Clone, Debug)]
pub enum ChecksumsEvent {
    /// User pressed the "Run" button. Caller re-runs against the
    /// current panel config.
    Run,
    /// Copy `text` to the clipboard. The host owns clipboard
    /// access; the panel just emits the request.
    Copy(String),
}

/// Render the per-file Checksums panel without virtual addressing
/// (range labels show raw file offsets).
pub fn show(ui: &mut egui::Ui, file_label: Option<&str>, panel: &mut ChecksumsPanel) -> Vec<ChecksumsEvent> {
    show_inner(ui, file_label, panel, None)
}

/// Render the per-file Checksums panel with virtual addressing
/// applied to the range label.
pub fn show_with_vaddr(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut ChecksumsPanel,
    virtual_base: u64,
) -> Vec<ChecksumsEvent> {
    show_inner(ui, file_label, panel, Some(virtual_base))
}

fn show_inner(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut ChecksumsPanel,
    virtual_base: Option<u64>,
) -> Vec<ChecksumsEvent> {
    let mut events: Vec<ChecksumsEvent> = Vec::new();
    ui.horizontal(|ui| {
        ui.heading(hxy_i18n::t("checksums-heading"));
        ui.add_space(8.0);
        let label = file_label.unwrap_or("");
        ui.label(egui::RichText::new(label).weak());
    });
    ui.separator();

    if file_label.is_none() {
        ui.label(hxy_i18n::t("checksums-no-active-file"));
        return events;
    }

    let running = panel.running.is_some();

    ui.horizontal_wrapped(|ui| {
        for alg in Algorithm::ALL {
            let mut enabled = panel.config.algorithms.contains(&alg);
            if ui.checkbox(&mut enabled, alg.label()).changed() {
                if enabled {
                    panel.config.algorithms.insert(alg);
                } else {
                    panel.config.algorithms.remove(&alg);
                }
            }
        }
    });

    ui.horizontal(|ui| {
        let run_label = if running { hxy_i18n::t("checksums-running") } else { hxy_i18n::t("checksums-run") };
        let run_button = egui::Button::new(run_label);
        let can_run = !running && !panel.config.algorithms.is_empty();
        if ui.add_enabled(can_run, run_button).clicked() {
            events.push(ChecksumsEvent::Run);
        }
        if !panel.config.algorithms.is_empty()
            && let Some(result) = panel.last_result.as_ref()
        {
            let copy_button = egui::Button::new(hxy_i18n::t("checksums-copy-all"));
            if ui.add(copy_button).clicked() {
                events.push(ChecksumsEvent::Copy(format_all_for_copy(result)));
            }
        }
    });

    let range = panel.config.range;
    if !range.is_empty() {
        let base = virtual_base.unwrap_or(0);
        ui.label(hxy_i18n::t_args(
            "checksums-range",
            &[
                ("start", &format!("0x{:X}", range.start().get().saturating_add(base))),
                ("end", &format!("0x{:X}", range.end().get().saturating_add(base))),
                ("length", &format_bytes(range.len().get())),
            ],
        ));
    }

    ui.separator();

    let Some(result) = panel.last_result.as_ref() else {
        if running {
            ui.label(hxy_i18n::t("checksums-running"));
        } else {
            ui.label(hxy_i18n::t("checksums-no-results-yet"));
        }
        return events;
    };

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("checksums-results-grid").num_columns(3).striped(true).show(ui, |ui| {
            ui.label(egui::RichText::new(hxy_i18n::t("checksums-col-algorithm")).strong());
            ui.label(egui::RichText::new(hxy_i18n::t("checksums-col-value")).strong());
            ui.label("");
            ui.end_row();
            for (alg, value) in &result.values {
                ui.label(alg.label());
                // Extend wrap mode keeps the hex digest on one
                // line; the outer ScrollArea handles overflow
                // when the panel is narrow. `selectable(true)`
                // lets the user double-click to grab the value
                // without taking the dedicated Copy button path.
                ui.add(
                    egui::Label::new(egui::RichText::new(value).monospace())
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                );
                if ui.small_button(hxy_i18n::t("checksums-copy")).clicked() {
                    events.push(ChecksumsEvent::Copy(value.clone()));
                }
                ui.end_row();
            }
        });
    });

    events
}

fn format_all_for_copy(result: &ChecksumResult) -> String {
    let mut out = String::new();
    for (alg, value) in &result.values {
        use std::fmt::Write;
        let _ = writeln!(&mut out, "{}: {}", alg.label(), value);
    }
    out
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(algs: &[Algorithm], len: u64) -> ChecksumConfig {
        ChecksumConfig {
            algorithms: algs.iter().copied().collect(),
            range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap(),
        }
    }

    #[test]
    fn known_vectors_for_empty_input() {
        let source = hxy_core::MemorySource::new(Vec::new());
        let config = cfg(&Algorithm::ALL, 0);
        let result = compute(&source, &config).unwrap();
        // All hashes for empty input -- well-known values.
        assert_eq!(result.values[&Algorithm::Crc32], "00000000");
        // Adler-32 of an empty buffer is 1 by definition (initial s2=0,
        // s1=1; concat'ing the high half of s2 with s1 yields 0x00000001).
        assert_eq!(result.values[&Algorithm::Adler32], "00000001");
        assert_eq!(result.values[&Algorithm::Md5], "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(result.values[&Algorithm::Sha1], "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            result.values[&Algorithm::Sha256],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            result.values[&Algorithm::Sha512],
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            result.values[&Algorithm::Blake3],
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn known_vectors_for_abc() {
        let source = hxy_core::MemorySource::new(b"abc".to_vec());
        let config = cfg(&Algorithm::ALL, 3);
        let result = compute(&source, &config).unwrap();
        // Adler-32 of "abc" = 0x024d0127 (RFC 1950 worked example).
        assert_eq!(result.values[&Algorithm::Adler32], "024d0127");
        assert_eq!(result.values[&Algorithm::Md5], "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(result.values[&Algorithm::Sha1], "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            result.values[&Algorithm::Sha256],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        // Feed 5 MiB of bytes so the streaming loop iterates more
        // than once per CHUNK_BYTES window.
        let bytes: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i & 0xff) as u8).collect();
        let source = hxy_core::MemorySource::new(bytes.clone());
        let result = compute(&source, &cfg(&[Algorithm::Sha256], bytes.len() as u64)).unwrap();
        let expected = {
            let mut h = sha2::Sha256::new();
            h.update(&bytes);
            hex_encode(&h.finalize())
        };
        assert_eq!(result.values[&Algorithm::Sha256], expected);
    }

    #[test]
    fn empty_algorithm_set_is_an_error() {
        let source = hxy_core::MemorySource::new(b"abc".to_vec());
        let cfg = ChecksumConfig {
            algorithms: BTreeSet::new(),
            range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(3)).unwrap(),
        };
        let err = compute(&source, &cfg).unwrap_err();
        assert!(err.contains("no algorithms"));
    }
}
