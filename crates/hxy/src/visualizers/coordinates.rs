//! `[[hex::visualize("coordinates", lat?, lng?)]]`: render a
//! geographic coordinate pair. Args may be literal numbers (already
//! evaluated by the runtime) or absent, in which case the visualizer
//! reads two consecutive `f64`s out of the field. Renders the text
//! readout plus a normalised world-rect with a dot at the lat/lng;
//! a real basemap is out of scope for this milestone.

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let (lat, lng) = match resolve_coordinates(ctx) {
        Ok(v) => v,
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
    };

    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-coords-info",
            &[("lat", &format!("{lat:.6}")), ("lng", &format!("{lng:.6}"))],
        ))
        .strong(),
    );
    ui.add_space(8.0);

    let (resp, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width().min(640.0), ui.available_width().min(640.0) * 0.5),
        egui::Sense::hover(),
    );
    let rect = resp.rect;
    let bg = ui.visuals().extreme_bg_color;
    let grid = ui.visuals().widgets.noninteractive.bg_stroke.color;
    painter.rect_filled(rect, 0.0, bg);
    // Equirectangular: lng -> x in [-180,180], lat -> y in [-90,90].
    let to_screen = |lat: f64, lng: f64| -> egui::Pos2 {
        let x = rect.left() + ((lng + 180.0) / 360.0) as f32 * rect.width();
        let y = rect.top() + ((90.0 - lat) / 180.0) as f32 * rect.height();
        egui::pos2(x, y)
    };
    // Equator + prime meridian
    let equator = to_screen(0.0, -180.0).y;
    painter.line_segment(
        [egui::pos2(rect.left(), equator), egui::pos2(rect.right(), equator)],
        egui::Stroke::new(1.0, grid),
    );
    let prime = to_screen(0.0, 0.0).x;
    painter.line_segment(
        [egui::pos2(prime, rect.top()), egui::pos2(prime, rect.bottom())],
        egui::Stroke::new(1.0, grid),
    );
    let pos = to_screen(lat, lng);
    let dot_color = ui.visuals().selection.bg_fill;
    painter.circle_filled(pos, 6.0, dot_color);
}

fn resolve_coordinates(ctx: &VisualizerContext) -> Result<(f64, f64), String> {
    if ctx.spec.args.len() >= 2 {
        let lat: f64 = ctx.spec.args[0]
            .parse()
            .map_err(|_| hxy_i18n::t_args("visualizer-coords-bad-arg", &[("which", "lat"), ("got", &ctx.spec.args[0])]))?;
        let lng: f64 = ctx.spec.args[1]
            .parse()
            .map_err(|_| hxy_i18n::t_args("visualizer-coords-bad-arg", &[("which", "lng"), ("got", &ctx.spec.args[1])]))?;
        return Ok((clamp_lat(lat), clamp_lng(lng)));
    }
    if ctx.bytes.len() >= 16 {
        let lat = f64::from_le_bytes(ctx.bytes[0..8].try_into().unwrap());
        let lng = f64::from_le_bytes(ctx.bytes[8..16].try_into().unwrap());
        return Ok((clamp_lat(lat), clamp_lng(lng)));
    }
    Err(hxy_i18n::t("visualizer-coords-need-bytes-or-args"))
}

fn clamp_lat(v: f64) -> f64 {
    v.clamp(-90.0, 90.0)
}
fn clamp_lng(v: f64) -> f64 {
    v.clamp(-180.0, 180.0)
}
