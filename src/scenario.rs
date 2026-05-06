//! Scenario = fixture data + optional reactive-callback dispatcher.
//!
//! `scenario::load(path)` is the main entry point used by `main.rs`.
//! It dispatches by extension: `.lua` files go through the dellingr
//! loader (which retains the VM as a [`Dispatcher`] for callbacks);
//! everything else parses as TOML and produces a static-only
//! `Scenario`.

use std::path::Path;
use std::sync::Arc;

use crate::fixture::{self, Fixture};
use crate::lua::{self, Dispatcher};

/// Bundles the validated fixture with the dellingr-backed callback
/// dispatcher (if any). All five protocol layers share these via
/// `Arc` so a single `Mutex<State>` services dispatch requests from
/// any worker thread.
#[derive(Clone)]
pub struct Scenario {
    pub fixture: Arc<Fixture>,
    pub dispatcher: Option<Arc<Dispatcher>>,
}

/// Load a scenario from a path. `.lua` extensions go through the
/// dellingr-backed loader; everything else parses as TOML and the
/// dispatcher is `None`.
pub fn load(path: &Path) -> Result<Scenario, String> {
    if path.extension().is_some_and(|e| e == "lua") {
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let chunk_name = format!("@{}", path.display());
        let (fixture, dispatcher) = lua::load_source_with_dispatcher(&source, &chunk_name)?;
        Ok(Scenario {
            fixture: Arc::new(fixture),
            dispatcher: Some(Arc::new(dispatcher)),
        })
    } else {
        let fixture = fixture::load(path)?;
        Ok(Scenario {
            fixture: Arc::new(fixture),
            dispatcher: None,
        })
    }
}
