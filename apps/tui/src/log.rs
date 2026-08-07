//! Structured diagnostic logging (D-040 / stability §3, §4), kept strictly separate from the replay `Trace`:
//! the Trace is the deterministic command record (a product artifact); this is operator-facing telemetry for
//! "why did it misbehave". Off by default — set `RUSE_LOG=<file>` (optionally `RUSE_LOG_LEVEL=debug`) to
//! append structured events to a file. Never writes to the terminal (that surface belongs to the editor).

use std::sync::{Arc, Mutex};

/// A cloneable file sink so `tracing_subscriber`'s `MakeWriter` can hand out a writer per event.
#[derive(Clone)]
struct FileSink(Arc<Mutex<std::fs::File>>);

impl std::io::Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_or(Ok(buf.len()), |mut f| f.write(buf))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_or(Ok(()), |mut f| f.flush())
    }
}

/// Initialize file logging if `RUSE_LOG` is set. Best-effort: a bad path or level silently disables logging
/// rather than blocking startup (external-failure degrade, §7). Safe to call once at startup.
pub fn init() {
    let Ok(path) = std::env::var("RUSE_LOG") else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let level = std::env::var("RUSE_LOG_LEVEL").unwrap_or_else(|_| "info".into());
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let sink = FileSink(Arc::new(Mutex::new(file)));
    let _ = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(filter)
        .try_init();
}
