//! `[[hex::visualize("disassembler", base_address?, isa?, mode?)]]`:
//! disassemble the field's bytes using `iced-x86`. Supported ISAs:
//! `x86`, `x86-32`, `x86-64`, `x64`, `amd64`. Other ISAs (ARM, RISC-V,
//! ...) need a different decoder backend (capstone is C / GPL-LGPL,
//! out of scope for this milestone) -- they fall through to a clear
//! "not yet supported" message rather than blank rendering.

pub use hxy_templates::visualize::disasm::DisassemblerCache;
use hxy_templates::visualize::disasm::Bitness;
use hxy_templates::visualize::disasm::blake3_with_args;
use hxy_templates::visualize::disasm::disassemble_x86;
use hxy_templates::visualize::disasm::parse_addr;

use super::VisualizerCache;
use super::VisualizerContext;

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
                cache.error = Some(hxy_i18n::t_args("visualizer-disasm-unsupported-isa", &[("isa", isa)]));
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
            &[("isa", isa), ("base", &format!("{base_address:#x}")), ("count", &cache.instruction_count.to_string())],
        ))
        .weak(),
    );
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_sized(
            ui.available_size(),
            egui::TextEdit::multiline(&mut cache.listing.as_str()).font(egui::TextStyle::Monospace).code_editor(),
        );
    });
}
