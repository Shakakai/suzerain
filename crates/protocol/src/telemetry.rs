//! OTEL wiring for the daemons themselves (agents get theirs via the
//! manifest observability block, fanned out as env — see provision.rs).
//!
//! Enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (standard OTEL envs
//! honored: endpoint, headers). Otherwise plain local tracing only.

use std::fs::OpenOptions;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Cheaply-cloneable writer over one shared, already-open file — tracing's
/// `with_writer` asks its `MakeWriter` for a fresh writer per event, so
/// this hands out clones of the same handle instead of reopening the path
/// (and losing the append position) each time.
#[derive(Clone)]
struct SharedFile(Arc<Mutex<std::fs::File>>);

impl io::Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// `log_file`: `None` logs to stderr (the default — visible on the
/// original CLI); `Some(path)` appends plain (no ANSI) lines to that file
/// instead, creating its parent directory if needed.
pub fn init(
    default_filter: &str,
    service_name: &'static str,
    log_file: Option<&Path>,
) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into());

    let fmt = match log_file {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating log directory {}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))?;
            let shared = SharedFile(Arc::new(Mutex::new(file)));
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || shared.clone())
                .boxed()
        }
        // Standalone mode's parent redirects the agent-worker child's OS
        // stderr to a file without telling the child — so the child must
        // decide ANSI-or-not from its own fd, not just default to on. This
        // also fixes plain-text output whenever anyone else pipes/redirects
        // us (systemd journal, `| tee`, etc.), not just our own --logdir.
        None => tracing_subscriber::fmt::layer()
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .boxed(),
    };

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;
        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(service_name)
            .build();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();
        let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, service_name);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .with(otel_layer)
            .init();
        tracing::info!(service = service_name, "OTEL export enabled");
    } else {
        tracing_subscriber::registry().with(filter).with(fmt).init();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_file_write_appends_and_flushes() {
        let dir = std::env::temp_dir().join(format!("telemetry-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shared.log");
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut shared = SharedFile(Arc::new(Mutex::new(file)));

        // Two clones write to the same underlying handle, and both see each
        // other's bytes appended (not overwritten) — the whole point of
        // sharing one already-open file across `MakeWriter` calls.
        let mut other = shared.clone();
        io::Write::write_all(&mut shared, b"first\n").unwrap();
        io::Write::write_all(&mut other, b"second\n").unwrap();
        io::Write::flush(&mut shared).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first\nsecond\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `init` with a `log_file` must create the parent directory (it may
    /// not exist yet — e.g. a fresh `--logdir`) and then be appendable by a
    /// subsequent tracing event. This is the only `init` path exercised
    /// here: `init` installs a *global* tracing subscriber via
    /// `tracing_subscriber::registry().init()`, which panics if called more
    /// than once per process, so this must remain the sole call to `init`
    /// in this crate's test suite.
    #[test]
    fn init_creates_missing_log_directory_and_writes_events() {
        // SAFETY: single-threaded test setup, before any other thread reads
        // this var.
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        let dir = std::env::temp_dir().join(format!("telemetry-init-test-{}", std::process::id()));
        let nested = dir.join("nested").join("logs");
        std::fs::remove_dir_all(&dir).ok();
        assert!(!nested.exists());
        let log_path = nested.join("agent.log");

        init("info", "telemetry-test", Some(&log_path)).expect("init should succeed");
        assert!(nested.is_dir(), "init should create missing parent dirs");

        tracing::info!("hello from telemetry test");
        // The fmt layer's writer is unbuffered per-event (`fmt::layer()`
        // flushes on each write), so the line should be visible immediately.
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            contents.contains("hello from telemetry test"),
            "log file missing expected line: {contents:?}"
        );
        assert!(
            !contents.contains("\x1b["),
            "file output should have no ANSI escapes: {contents:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
