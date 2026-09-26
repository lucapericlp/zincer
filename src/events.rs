//! Machine-readable progress events for embedding callers (e.g. a desktop
//! app driving a sync as a subprocess).
//!
//! When enabled with `--progress-events`, each event is printed to stdout as
//! one line: `ZINCER_EVENT {json}`. Human-readable tracing logs are
//! unaffected, so callers can relay both.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

pub const EVENT_PREFIX: &str = "ZINCER_EVENT ";

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Emit one event; `event` names it and `fields` (a JSON object) carries the
/// payload. No-op unless progress events are enabled.
pub fn emit(event: &str, fields: Value) {
    if !enabled() {
        return;
    }
    let mut payload = match fields {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".into(), other);
            map
        }
    };
    payload.insert("event".into(), Value::String(event.to_string()));
    let line = format!("{EVENT_PREFIX}{}\n", Value::Object(payload));
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.flush();
}
