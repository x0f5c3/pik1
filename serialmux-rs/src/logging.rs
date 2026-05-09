//! Structured logging initialisation.
//!
//! Call [`init`] once at the very start of `main` (before any tracing macros
//! are used).  The subscriber composes up to three output layers:
//!
//! | Layer | Active | Description |
//! |---|---|---|
//! | **stderr** | always | Compact human-readable lines picked up by systemd / journald. |
//! | **rolling file** | opt-in | JSON lines rotated daily; path set by `SERIALMUX_LOG_DIR` (default `/var/log/serialmux`). |
//! | **journald** | `journald` feature | Sends structured key-value records via `sd_journal_sendv`. |
//!
//! The active log level is controlled by the `RUST_LOG` environment variable
//! (e.g. `RUST_LOG=debug`).  Absent the variable the default is `info`.
//!
//! # Routing logs to Grafana / OpenTelemetry
//!
//! Set `SERIALMUX_LOG_DIR` to a directory watched by a log-shipping agent
//! (Vector, Promtail, Fluent Bit).  The JSON file layer's output is
//! directly consumable by Grafana Loki and any OpenTelemetry Collector
//! that supports the OTLP logs receiver — no extra binary overhead in
//! this process is required.

use std::path::PathBuf;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialise the global tracing subscriber.
///
/// Safe to call multiple times — subsequent calls are no-ops because the
/// global default subscriber is set only once.
pub fn init(program: &'static str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // ── stderr layer (compact, no ANSI — safe inside systemd units) ──────
    let stderr_layer = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr);

    // ── rolling JSON file layer ───────────────────────────────────────────
    // Enabled when SERIALMUX_LOG_DIR exists or can be created.
    // JSON format is directly parseable by Vector / Promtail / Fluent Bit
    // for forwarding to Grafana Loki or an OTLP Collector.
    let log_dir = std::env::var("SERIALMUX_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/log/serialmux"));

    let file_layer = if ensure_log_dir(&log_dir) {
        let appender = tracing_appender::rolling::daily(&log_dir, program);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        // Leak the guard so its background flush thread lives until process
        // exit.  Dropping it early would stop the background writer.
        std::mem::forget(guard);
        Some(fmt::layer().json().with_target(true).with_writer(non_blocking))
    } else {
        None
    };

    // ── compose layers ────────────────────────────────────────────────────
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer);

    // Optional journald layer — compiled in only when the `journald` feature
    // is enabled.  Uses cfg blocks so the tracing-journald crate is never
    // referenced when the feature is absent.
    #[cfg(feature = "journald")]
    let registry = registry.with(tracing_journald::layer().ok());

    let _ = registry.try_init();
}

/// Return `true` if `dir` exists or was successfully created.
fn ensure_log_dir(dir: &std::path::Path) -> bool {
    if dir.is_dir() {
        return true;
    }
    match std::fs::create_dir_all(dir) {
        Ok(()) => true,
        Err(e) => {
            // tracing is not yet initialised at this point — fall back to eprintln.
            eprintln!(
                "serialmux: cannot create log dir {}: {} (file logging disabled)",
                dir.display(),
                e
            );
            false
        }
    }
}
