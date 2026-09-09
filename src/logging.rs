//! Tracing subscriber setup: human-readable console (ANSI when stderr is a TTY) and optional JSON
//! lines to a daily-rotating log file.

use std::fmt::{self, Write as _};
use std::io::{stderr, IsTerminal};
use std::path::Path;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt as tracing_fmt;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

use crate::error::OxidriveError;

static FILE_LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

const DEFAULT_FILTER: &str = "warn";

/// Resolves the console `EnvFilter` from CLI flags, config, and optional `RUST_LOG`.
///
/// `--quiet` and `--verbose` win over `RUST_LOG`. `--verbose` / `--verbose --verbose` map to
/// `oxidrive=info` / `oxidrive=debug` so third-party crates stay at the EnvFilter default (`error`).
/// With no flags, a non-empty `RUST_LOG` wins. Otherwise the console stays at [`DEFAULT_FILTER`]
/// (`warn`) even if an old config still has `log_level = "info"` / `"debug"` — those levels are
/// `--verbose`, not the default. Config may only make the console *quieter* (`error`).
#[must_use]
pub fn resolve_log_filter(quiet: bool, verbose: u8, config_log_level: &str) -> String {
    resolve_log_filter_with_env(
        quiet,
        verbose,
        config_log_level,
        std::env::var("RUST_LOG").ok(),
    )
}

fn resolve_log_filter_with_env(
    quiet: bool,
    verbose: u8,
    config_log_level: &str,
    rust_log: Option<String>,
) -> String {
    if quiet {
        return DEFAULT_FILTER.to_string();
    }
    match verbose {
        0 => {
            if let Some(rust_log) = rust_log {
                let rust_log = rust_log.trim();
                if !rust_log.is_empty() {
                    return rust_log.to_string();
                }
            }
            let level = config_log_level.trim().to_ascii_lowercase();
            if level == "error" || level == "off" {
                level
            } else {
                DEFAULT_FILTER.to_string()
            }
        }
        1 => "oxidrive=info".to_string(),
        _ => "oxidrive=debug".to_string(),
    }
}

/// Initializes tracing using a pre-resolved filter string (CLI + config already applied) and
/// optional JSON file output.
pub fn init_logging(resolved_filter: &str, log_file: Option<&Path>) -> Result<(), OxidriveError> {
    let filter = {
        let t = resolved_filter.trim();
        if t.is_empty() {
            DEFAULT_FILTER
        } else {
            t
        }
    };
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let is_tty = stderr().is_terminal();
    let console_layer = tracing_fmt::layer()
        .event_format(HumanFormat { ansi: is_tty })
        .with_ansi(is_tty)
        .with_writer(std::io::stderr)
        .with_filter(env_filter);

    if let Some(path) = log_file {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;

        let prefix = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("oxidrive");

        let appender = RollingFileAppender::new(Rotation::DAILY, dir, prefix);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        let _ = FILE_LOG_GUARD.set(guard);

        let file_layer = tracing_fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_filter(EnvFilter::new("debug"));

        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init()
            .map_err(|e| OxidriveError::other(format!("tracing init failed: {e}")))?;
    } else {
        tracing_subscriber::registry()
            .with(console_layer)
            .try_init()
            .map_err(|e| OxidriveError::other(format!("tracing init failed: {e}")))?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct HumanFormat {
    ansi: bool,
}

impl<S, N> FormatEvent<S, N> for HumanFormat
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = HumanVisitor::default();
        event.record(&mut visitor);
        let line = format_human_line(
            event.metadata().level(),
            &visitor.message,
            &visitor.fields,
            self.ansi,
        );
        writeln!(writer, "{line}")
    }
}

#[derive(Default)]
struct HumanVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl HumanVisitor {
    fn record_pair(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for HumanVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_pair(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_pair(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_pair(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_pair(field, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_pair(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_pair(field, strip_debug_quotes(&format!("{value:?}")));
    }
}

fn strip_debug_quotes(rendered: &str) -> String {
    if rendered.len() >= 2 && rendered.starts_with('"') && rendered.ends_with('"') {
        rendered[1..rendered.len() - 1].replace("\\\"", "\"")
    } else {
        rendered.to_string()
    }
}

fn paint_prefix(label: &str, ansi_code: &str, ansi: bool) -> String {
    if !ansi {
        return label.to_string();
    }
    format!("\x1b[{ansi_code}m{label}\x1b[0m")
}

fn field_value<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn append_quoted_path(body: &mut String, fields: &[(String, String)]) {
    let Some(path) = field_value(fields, "path") else {
        return;
    };
    if path.is_empty() || body.contains(path) {
        return;
    }
    let _ = write!(body, " '{path}'");
}

fn format_info_fields(message: &str, fields: &[(String, String)]) -> String {
    let mut body = String::from(message);
    if fields.is_empty() {
        return body;
    }
    body.push(':');
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, " {key}: {value}");
    }
    body
}

/// Formats one console line in a Git-like style.
///
/// Info/debug keep optional `key: value` details. Warnings and errors are a short sentence
/// (plus `'path'` when present); extra tracing fields stay out of the console.
#[must_use]
fn format_human_line(
    level: &Level,
    message: &str,
    fields: &[(String, String)],
    ansi: bool,
) -> String {
    match *level {
        Level::ERROR => {
            let mut body = String::from(message);
            append_quoted_path(&mut body, fields);
            format!("{}{body}", paint_prefix("error: ", "31", ansi))
        }
        Level::WARN => {
            let mut body = String::from(message);
            append_quoted_path(&mut body, fields);
            format!("{}{body}", paint_prefix("warning: ", "33", ansi))
        }
        _ => format_info_fields(message, fields),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn format_human_line_puts_message_before_fields_on_one_line() {
        let line = format_human_line(
            &Level::INFO,
            "Computing sync actions",
            &[("paths".into(), "12".into())],
            false,
        );
        assert_eq!(line, "Computing sync actions: paths: 12");
        assert!(!line.contains('\n'));
        assert!(
            !line.contains("INFO"),
            "info lines have no level prefix like git: {line}"
        );
        assert!(
            line.find("Computing").expect("message") < line.find("paths:").expect("field"),
            "{line}"
        );
    }

    #[test]
    fn format_human_line_uses_git_warning_prefix_without_ansi() {
        let line = format_human_line(&Level::WARN, "could not clean local trash", &[], false);
        assert_eq!(line, "warning: could not clean local trash");
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn format_human_line_uses_ansi_for_git_error_prefix() {
        let line = format_human_line(&Level::ERROR, "sync cycle failed", &[], true);
        assert!(line.contains("\x1b[31merror: \x1b[0m"), "{line}");
        assert!(line.contains("sync cycle failed"));
        assert!(!line.contains("ERROR"));
    }

    #[test]
    fn format_human_line_quotes_warning_path_and_drops_extra_fields() {
        let line = format_human_line(
            &Level::WARN,
            "could not read saved sync metadata",
            &[
                ("path".into(), "notes.md".into()),
                ("error".into(), "unexpected end of file".into()),
                ("owner".into(), "device-abc".into()),
            ],
            false,
        );
        assert_eq!(
            line,
            "warning: could not read saved sync metadata 'notes.md'"
        );
        assert!(!line.contains("unexpected end of file"));
        assert!(!line.contains("owner"));
        assert!(!line.contains("device-abc"));
    }

    #[test]
    fn format_human_line_does_not_repeat_path_already_in_the_message() {
        let line = format_human_line(
            &Level::WARN,
            "two files named 'foo.md'; using 'foo (1).md'",
            &[("path".into(), "foo (1).md".into())],
            false,
        );
        assert_eq!(
            line,
            "warning: two files named 'foo.md'; using 'foo (1).md'"
        );
    }

    #[test]
    fn resolve_log_filter_default_is_warn() {
        assert_eq!(resolve_log_filter_with_env(false, 0, "", None), "warn");
        assert_eq!(resolve_log_filter_with_env(false, 0, "   ", None), "warn");
    }

    #[test]
    fn resolve_log_filter_quiet_and_verbose_mapping() {
        assert_eq!(resolve_log_filter_with_env(true, 0, "info", None), "warn");
        assert_eq!(resolve_log_filter_with_env(false, 0, "debug", None), "warn");
        assert_eq!(resolve_log_filter_with_env(false, 0, "info", None), "warn");
        assert_eq!(
            resolve_log_filter_with_env(false, 0, "error", None),
            "error"
        );
        assert_eq!(
            resolve_log_filter_with_env(false, 1, "warn", None),
            "oxidrive=info"
        );
        assert_eq!(
            resolve_log_filter_with_env(false, 2, "warn", None),
            "oxidrive=debug"
        );
        assert_eq!(
            resolve_log_filter_with_env(false, 3, "warn", None),
            "oxidrive=debug"
        );
    }

    #[test]
    fn resolve_log_filter_rust_log_is_escape_hatch_without_flags() {
        assert_eq!(
            resolve_log_filter_with_env(false, 0, "warn", Some("oxidrive=debug".into())),
            "oxidrive=debug"
        );
        assert_eq!(
            resolve_log_filter_with_env(true, 0, "info", Some("debug".into())),
            "warn"
        );
        assert_eq!(
            resolve_log_filter_with_env(false, 1, "warn", Some("trace".into())),
            "oxidrive=info"
        );
    }

    #[derive(Clone)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    struct BufferHandle(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferHandle {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = BufferHandle;

        fn make_writer(&'a self) -> Self::Writer {
            BufferHandle(self.0.clone())
        }
    }

    #[test]
    fn file_layer_emits_parseable_json_without_ansi() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(BufferWriter(buf.clone()))
            .with_env_filter(EnvFilter::new("debug"))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(paths = 12, "Computing sync actions");
        });

        let text = String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
        let first = text.lines().next().expect("one json line");
        assert!(!first.contains('\x1b'), "{first}");
        let value: serde_json::Value = serde_json::from_str(first).expect("json");
        assert!(value.is_object(), "{value}");
        let dumped = value.to_string();
        assert!(
            dumped.contains("Computing sync actions"),
            "json should include the message: {dumped}"
        );
        assert!(
            first.trim_start().starts_with('{'),
            "file output must stay JSON, got {first}"
        );
    }
}
