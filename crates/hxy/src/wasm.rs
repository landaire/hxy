//! WASM entry point. Invoked automatically from JS once the wasm module
//! finishes loading (`#[wasm_bindgen(start)]`).

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::HxyApp;
use crate::state::PersistedState;
use crate::state::shared;

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    install_tracing();
    hxy_i18n::init_from_system_locale();

    let canvas = web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window"))?
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?
        .get_element_by_id("hxy_canvas")
        .ok_or_else(|| JsValue::from_str("missing element #hxy_canvas"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| JsValue::from_str("#hxy_canvas is not a canvas"))?;

    let state = shared(PersistedState::default());

    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(move |cc| Ok(Box::new(HxyApp::new(cc, state)))))
            .await
        {
            tracing::error!("eframe WebRunner failed: {e:?}");
        }
    });

    Ok(())
}

fn install_tracing() {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let wasm_layer = tracing_wasm::WASMLayer::new(
        tracing_wasm::WASMLayerConfigBuilder::new().set_max_level(tracing::Level::DEBUG).build(),
    );
    let filter = tracing_subscriber::filter::Targets::new()
        .with_target("hxy_app", tracing::Level::DEBUG)
        .with_target("hxy_view", tracing::Level::DEBUG);
    tracing_subscriber::registry().with(wasm_layer.with_filter(filter)).init();
}
