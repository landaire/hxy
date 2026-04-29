//! `[[hex::visualize("disassembler", base_address?, isa?, mode?)]]`:
//! disassemble the field's bytes using `iced-x86`. Supported ISAs:
//! `x86`, `x86-32`, `x86-64`, `x64`, `amd64`. Other ISAs (ARM, RISC-V,
//! ...) need a different decoder backend (capstone is C / GPL-LGPL,
//! out of scope for this milestone) -- they fall through to a clear
//! "not yet supported" message rather than blank rendering.

use iced_x86::Decoder;
use iced_x86::DecoderOptions;
use iced_x86::Formatter;
use iced_x86::Instruction;
use iced_x86::IntelFormatter;

use super::VisualizerCache;
use super::VisualizerContext;

#[derive(Default)]
pub struct DisassemblerCache {
    pub fingerprint: Option<[u8; 32]>,
    pub listing: String,
    pub instruction_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
enum Bitness {
    Bits16,
    Bits32,
    Bits64,
}

impl Bitness {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "x86" | "x86-32" | "x86_32" | "i386" | "ia32" | "32" => Self::Bits32,
            "x86-64" | "x86_64" | "x64" | "amd64" | "64" => Self::Bits64,
            "16" | "real" | "x86-16" | "8086" => Self::Bits16,
            _ => return None,
        })
    }

    fn bits(&self) -> u32 {
        match self {
            Self::Bits16 => 16,
            Self::Bits32 => 32,
            Self::Bits64 => 64,
        }
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.disassembler.get_or_insert_with(DisassemblerCache::default);
    let fingerprint = blake3_with_args(ctx.bytes, &ctx.spec.args);
    let stale = cache.fingerprint != Some(fingerprint);

    let base_address: u64 = ctx.spec.args.first().and_then(|a| parse_addr(a)).unwrap_or(ctx.node.span.offset);
    let isa = ctx.spec.args.get(1).map(|s| s.as_str()).unwrap_or("x86-64");

    if stale {
        cache.fingerprint = Some(fingerprint);
        cache.listing.clear();
        cache.instruction_count = 0;
        cache.error = None;
        match Bitness::parse(isa) {
            Some(b) => disassemble_x86(ctx.bytes, b, base_address, cache),
            None => {
                cache.error = Some(hxy_i18n::t_args(
                    "visualizer-disasm-unsupported-isa",
                    &[("isa", isa)],
                ));
            }
        }
    }

    if let Some(err) = &cache.error {
        ui.colored_label(ui.visuals().error_fg_color, err);
        return;
    }
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-disasm-info",
            &[
                ("isa", isa),
                ("base", &format!("{base_address:#x}")),
                ("count", &cache.instruction_count.to_string()),
            ],
        ))
        .weak(),
    );
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_sized(
            ui.available_size(),
            egui::TextEdit::multiline(&mut cache.listing.as_str())
                .font(egui::TextStyle::Monospace)
                .code_editor(),
        );
    });
}

fn disassemble_x86(bytes: &[u8], bitness: Bitness, base_address: u64, cache: &mut DisassemblerCache) {
    let mut decoder = Decoder::with_ip(bitness.bits(), bytes, base_address, DecoderOptions::NONE);
    let mut formatter = IntelFormatter::new();
    formatter.options_mut().set_first_operand_char_index(8);
    let mut instr = Instruction::default();
    let mut listing = String::new();
    let mut count = 0usize;
    while decoder.can_decode() {
        decoder.decode_out(&mut instr);
        let ip = instr.ip();
        let mut text = String::new();
        formatter.format(&instr, &mut text);
        let start = (instr.ip() - base_address) as usize;
        let end = start + instr.len();
        let hex_bytes: String = bytes[start..end.min(bytes.len())]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        listing.push_str(&format!("{ip:016x}  {hex_bytes:<24}  {text}\n"));
        count += 1;
    }
    cache.listing = listing;
    cache.instruction_count = count;
}

fn parse_addr(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

fn blake3_with_args(bytes: &[u8], args: &[String]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    for a in args {
        hasher.update(&[0u8]);
        hasher.update(a.as_bytes());
    }
    *hasher.finalize().as_bytes()
}
