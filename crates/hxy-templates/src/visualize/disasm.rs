//! `[[hex::visualize("disassembler", base_address?, isa?, mode?)]]`
//! decode: disassemble bytes using `iced-x86`. Supported ISAs:
//! `x86`, `x86-32`, `x86-64`, `x64`, `amd64`. Other ISAs (ARM,
//! RISC-V, ...) need a different decoder backend (capstone is C /
//! GPL-LGPL, out of scope for this milestone) -- the frontends
//! surface a clear "not yet supported" message for those.

use iced_x86::Decoder;
use iced_x86::DecoderOptions;
use iced_x86::Formatter;
use iced_x86::Instruction;
use iced_x86::IntelFormatter;

/// Disassembly listing, decoded once and reused across frames (the
/// listing can be tens of kB and parsing every frame is pointless).
#[derive(Default)]
pub struct DisassemblerCache {
    pub fingerprint: Option<[u8; 32]>,
    pub listing: String,
    pub instruction_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Bitness {
    Bits16,
    Bits32,
    Bits64,
}

impl Bitness {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "x86" | "x86-32" | "x86_32" | "i386" | "ia32" | "32" => Self::Bits32,
            "x86-64" | "x86_64" | "x64" | "amd64" | "64" => Self::Bits64,
            "16" | "real" | "x86-16" | "8086" => Self::Bits16,
            _ => return None,
        })
    }

    pub fn bits(&self) -> u32 {
        match self {
            Self::Bits16 => 16,
            Self::Bits32 => 32,
            Self::Bits64 => 64,
        }
    }
}

pub fn disassemble_x86(bytes: &[u8], bitness: Bitness, base_address: u64, cache: &mut DisassemblerCache) {
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
        let hex_bytes: String =
            bytes[start..end.min(bytes.len())].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        listing.push_str(&format!("{ip:016x}  {hex_bytes:<24}  {text}\n"));
        count += 1;
    }
    cache.listing = listing;
    cache.instruction_count = count;
}

/// Parse a base-address arg: `0x`-prefixed hex or plain decimal.
pub fn parse_addr(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

pub fn blake3_with_args(bytes: &[u8], args: &[String]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    for a in args {
        hasher.update(&[0u8]);
        hasher.update(a.as_bytes());
    }
    *hasher.finalize().as_bytes()
}
