//! Compile-time snapshot of `websockets-chat/dist`, staged by `build.rs`
//! into `$OUT_DIR/gateway-ui`.
//!
//! [`is_available`] is true only when the staged bundle includes a `.wasm`
//! file — the stub written when dist is missing does not.
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/gateway-ui"]
pub struct BundledGatewayUi;

/// True when the compiled-in gateway UI is a real Trunk build (has WASM).
pub fn is_available() -> bool {
    BundledGatewayUi::iter().any(|path| path.ends_with(".wasm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_available_matches_embedded_wasm() {
        let has_wasm = BundledGatewayUi::iter().any(|path| path.ends_with(".wasm"));
        assert_eq!(is_available(), has_wasm);
    }

    #[test]
    fn stub_or_bundle_always_has_index_html() {
        assert!(
            BundledGatewayUi::get("index.html").is_some(),
            "build.rs should always stage at least a stub index.html"
        );
    }
}
