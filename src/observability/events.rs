use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

#[derive(Debug, Serialize)]
pub struct ObservabilityEvent {
    pub ts: DateTime<Utc>,
    pub event_type: &'static str,
    pub fields: Value,
}

#[derive(Debug)]
pub struct JsonlEventLogger {
    writer: Mutex<BufWriter<File>>,
}

impl JsonlEventLogger {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn append(&self, event: &ObservabilityEvent) -> std::io::Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("event log mutex poisoned"))?;
        serde_json::to_writer(&mut *guard, event)?;
        guard.write_all(b"\n")?;
        guard.flush()?;
        Ok(())
    }
}

static GLOBAL_EVENTS: OnceLock<Arc<JsonlEventLogger>> = OnceLock::new();

pub fn init_event_logger(path: impl AsRef<Path>) -> std::io::Result<Arc<JsonlEventLogger>> {
    let logger = Arc::new(JsonlEventLogger::open(path)?);
    Ok(match GLOBAL_EVENTS.set(logger.clone()) {
        Ok(()) => logger,
        Err(_) => GLOBAL_EVENTS.get().expect("already set").clone(),
    })
}

pub fn event_logger_handle() -> Option<Arc<JsonlEventLogger>> {
    GLOBAL_EVENTS.get().cloned()
}

pub fn emit_event(event_type: &'static str, fields: Value) {
    let Some(logger) = event_logger_handle() else {
        return;
    };
    let event = ObservabilityEvent {
        ts: Utc::now(),
        event_type,
        fields,
    };
    if let Err(e) = logger.append(&event) {
        warn!(error = %e, event_type, "observability event append failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn jsonl_logger_appends_one_event_per_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let logger = JsonlEventLogger::open(&path).unwrap();
        logger
            .append(&ObservabilityEvent {
                ts: Utc::now(),
                event_type: "test_event",
                fields: json!({"signal_id":"s1"}),
            })
            .unwrap();

        let body = std::fs::read_to_string(path).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let value: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(value["event_type"], "test_event");
        assert_eq!(value["fields"]["signal_id"], "s1");
    }
}
