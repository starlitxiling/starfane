//! Minimal stderr logger for the `log` facade (no extra crates).

use log::{Level, LevelFilter, Log, Metadata, Record};

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = starfane_proto::id::now_ms();
        let lvl = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        eprintln!("{ts} {lvl} {}: {}", record.target(), record.args());
    }

    fn flush(&self) {}
}

/// Install the logger; level from `$STARFANE_LOG` (default `info`).
pub fn init() {
    let level = match std::env::var(starfane_proto::paths::ENV_LOG)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        "off" => LevelFilter::Off,
        _ => LevelFilter::Info,
    };
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}
