//! Feature-flag entry points for the channel-protocol refactor (T00 decision 10).
//!
//! Four independent switches gate the new routing/codec paths.  They default to
//! OFF so the legacy flat Dispatcher path remains the active behavior; the
//! leader does not flip them on.  T06 is responsible for wiring the real
//! executors behind these flags.
//!
//! | Flag                    | Gates                                                     |
//! |-------------------------|-----------------------------------------------------------|
//! | `features.new_routeplan`    | The model-first RoutePlan path in the HTTP handlers. T06 wires it for EVERY routed endpoint (Chat / Responses / Messages / CountTokens / Embeddings) on BOTH stream and non-stream paths via `handlers::maybe_route_plan` → `executor::driver::{route_plan_response, route_stream_plan}`. OFF → legacy flat Dispatcher path unchanged. |
//! | `features.cross_protocol_codec` | G2 conversion groups (Chat→Anthropic, Messages→Chat, Responses→Chat). |
//! | `features.native_responses` | Responses G1 native `/responses` group.                  |
//! | `features.ollama_native`    | Native Ollama `/api/chat` group (added by T06 to the Chat matrix; OFF until the executor + downstream Chat chain pass their tests). |

use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

/// Snapshot of the four routing feature flags.  Defaults are all-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeatureFlags {
    pub new_routeplan: bool,
    pub cross_protocol_codec: bool,
    pub native_responses: bool,
    pub ollama_native: bool,
}

impl FeatureFlags {
    /// Everything off: legacy flat routing (production default until rollout).
    pub fn all_off() -> Self {
        Self::default()
    }

    /// Everything on — used only by tests / manual staging.
    pub fn all_on() -> Self {
        Self {
            new_routeplan: true,
            cross_protocol_codec: true,
            native_responses: true,
            ollama_native: true,
        }
    }

    /// Whether any conversion group may be built.
    pub fn conversions_enabled(&self) -> bool {
        self.cross_protocol_codec
    }
}

/// Read the four flags from the Tauri settings store (`settings.json`).
///
/// Keys are namespaced under `features.*`.  Missing values are treated as
/// OFF.  The planner never reads the store itself — handlers read it once and
/// pass a snapshot so the planner stays pure and deterministic in tests.
pub fn read_feature_flags(app: &AppHandle) -> FeatureFlags {
    let Ok(store) = app.store("settings.json") else {
        return FeatureFlags::default();
    };
    FeatureFlags {
        new_routeplan: store
            .get("features.new_routeplan")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        cross_protocol_codec: store
            .get("features.cross_protocol_codec")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        native_responses: store
            .get("features.native_responses")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ollama_native: store
            .get("features.ollama_native")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_all_off() {
        let f = FeatureFlags::default();
        assert!(!f.new_routeplan);
        assert!(!f.cross_protocol_codec);
        assert!(!f.native_responses);
        assert!(!f.ollama_native);
        assert!(!f.conversions_enabled());
    }

    #[test]
    fn all_on_enables_conversions() {
        let f = FeatureFlags::all_on();
        assert!(f.new_routeplan);
        assert!(f.conversions_enabled());
    }
}
