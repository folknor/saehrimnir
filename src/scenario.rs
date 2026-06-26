//! Scenario = fixture data + optional reactive-callback dispatcher.
//!
//! `scenario::load(path)` is the main entry point used by `main.rs`.
//! It dispatches by extension: `.lua` files go through the dellingr
//! loader (which retains the VM as a [`Dispatcher`] for callbacks);
//! everything else parses as TOML and produces a static-only
//! `Scenario`.

use std::path::Path;
use std::sync::Arc;

use crate::fixture;
use crate::lua::{self, Dispatcher};
use crate::shared::{self, FixtureHandle};

/// Bundles the validated fixture with the dellingr-backed callback
/// dispatcher (if any). All five protocol layers share these via
/// `Arc` so a single `Mutex<State>` services dispatch requests from
/// any worker thread. The fixture itself sits behind a single
/// `RwLock` so the JMAP `Email/set` / `Mailbox/set` mutators can take
/// a write guard without disturbing the read paths.
#[derive(Clone)]
pub struct Scenario {
    pub fixture: FixtureHandle,
    pub dispatcher: Option<Arc<Dispatcher>>,
}

/// Load a scenario from a path. `.lua` extensions go through the
/// dellingr-backed loader; everything else parses as TOML and the
/// dispatcher is `None`.
pub fn load(path: &Path) -> Result<Scenario, String> {
    if path.extension().is_some_and(|e| e == "lua") {
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let chunk_name = format!("@{}", path.display());
        let dir = path.parent().unwrap_or(Path::new("."));
        let (fixture, dispatcher) =
            lua::load_source_with_dispatcher_and_dir(&source, &chunk_name, dir)?;
        Ok(Scenario {
            fixture: shared::handle(fixture),
            dispatcher: Some(Arc::new(dispatcher)),
        })
    } else {
        let fixture = fixture::load(path)?;
        Ok(Scenario {
            fixture: shared::handle(fixture),
            dispatcher: None,
        })
    }
}
