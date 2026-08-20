//! `[[hex::visualize("coordinates", lat?, lng?)]]` decode: resolve a
//! geographic coordinate pair. Args may be literal numbers (already
//! evaluated by the runtime) or absent, in which case two
//! consecutive `f64`s are read out of the field.

pub fn resolve_coordinates(bytes: &[u8], args: &[String]) -> Result<(f64, f64), String> {
    if args.len() >= 2 {
        let lat: f64 = args[0]
            .parse()
            .map_err(|_| hxy_i18n::t_args("visualizer-coords-bad-arg", &[("which", "lat"), ("got", &args[0])]))?;
        let lng: f64 = args[1]
            .parse()
            .map_err(|_| hxy_i18n::t_args("visualizer-coords-bad-arg", &[("which", "lng"), ("got", &args[1])]))?;
        return Ok((clamp_lat(lat), clamp_lng(lng)));
    }
    if bytes.len() >= 16 {
        let lat = f64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let lng = f64::from_le_bytes(bytes[8..16].try_into().unwrap());
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
