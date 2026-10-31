use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;

// Return the log path used for file-backed tracing output.
fn log_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".local/state/fff-gpui/fff-gpui.log")
}

struct LogFileWriter;

impl<'a> MakeWriter<'a> for LogFileWriter {
    type Writer = Box<dyn Write + Send + 'static>;

    fn make_writer(&'a self) -> Self::Writer {
        let path = log_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(io::sink()),
        }
    }
}

// Build a filter from an env var, falling back to a sane default that keeps
// our own logs useful without pulling in dependency noise.
fn env_filter(var_name: &str, default_directives: &str) -> EnvFilter {
    std::env::var(var_name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            default_directives
                .parse()
                .expect("default tracing directives must be valid")
        })
}

// Initialize tracing for the optional terminal stream and the persistent log file.
pub fn init_tracing(print_to_stdout: bool) {
    let stdout_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "fff_gpui=info,fff_search=info,fff_query_parser=warn,fff_grep=warn,gpui=warn,ignore=warn,smol=warn"
            .parse()
            .expect("default tracing directives must be valid")
    });
    let file_filter = env_filter(
        "FFF_GPUI_FILE_LOG",
        "fff_gpui=debug,fff_search=info,fff_query_parser=warn,fff_grep=warn,gpui=info,ignore=warn,smol=warn",
    );

    let stdout_layer = print_to_stdout.then(|| {
        fmt::layer()
            .with_ansi(true)
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_writer(std::io::stdout)
            .with_filter(stdout_filter)
    });

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(LogFileWriter)
        .with_filter(file_filter);

    let registry = tracing_subscriber::registry().with(file_layer);
    if let Some(stdout_layer) = stdout_layer {
        registry.with(stdout_layer).init();
    } else {
        registry.init();
    }
}

// Return the log path as display text for user-facing errors.
pub fn path_for_display() -> String {
    log_path().display().to_string()
}
