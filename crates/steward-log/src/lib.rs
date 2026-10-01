//! Logging shared by stewardd, the CLI and the GUI: `tracing` events written
//! to stderr one line each, as `program: warning: span{fields}: message`.
//!
//! When stderr is the journal (`JOURNAL_STREAM` names it) each line starts with its syslog
//! priority (`<4>`) so the journal records the level, and carries no
//! timestamp, which the journal adds. Elsewhere `timestamps` prefixes local
//! time, and on a terminal warnings and errors are coloured.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::IsTerminal;
use std::sync::{Mutex, PoisonError};

pub use tracing::Level;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Steward's own crates; dependencies log only warnings unless `RUST_LOG`
/// says otherwise.
const OURS: &[&str] = &[
    "stewardd",
    "steward",
    "steward_cli",
    "steward_ui",
    "steward_index",
    "steward_classify",
    "steward_contentid",
    "steward_proto",
];

/// Install the logger. Steward's crates log at `default` and each `-v`
/// (`verbosity`) shows one level more; `RUST_LOG` overrides both.
pub fn init(default: Level, verbosity: u8, timestamps: bool) {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| {
            let levels = ["error", "warn", "info", "debug", "trace"];
            let base = match default {
                Level::ERROR => 0,
                Level::WARN => 1,
                Level::INFO => 2,
                Level::DEBUG => 3,
                Level::TRACE => 4,
            };
            let level = levels[(base + usize::from(verbosity)).min(4)];
            let ours: Vec<_> = OURS.iter().map(|c| format!("{c}={level}")).collect();
            EnvFilter::new(format!("warn,{}", ours.join(",")))
        });
    let journald = stderr_is_journal();
    let line = Line {
        program: program(),
        journald,
        timestamps: timestamps && !journald,
        colour: !journald && std::io::stderr().is_terminal(),
    };
    let lines = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        // Colour is ours (warning and error labels); fields stay plain.
        .with_ansi(false)
        .event_format(line);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(lines)
        .with(Recent)
        .try_init();
}

/// A warning or error kept in memory, so a program can show what went wrong
/// lately (stewardd reports them in `status`).
#[derive(Clone, Debug)]
pub struct Problem {
    /// Seconds since the Unix epoch.
    pub time: f64,
    /// `error` or `warning`.
    pub level: &'static str,
    /// The spans it happened in, outermost first: `hash{path=/x}`.
    pub context: String,
    pub message: String,
}

const KEEP: usize = 200;

static RECENT: Mutex<VecDeque<Problem>> = Mutex::new(VecDeque::new());

/// The last warnings and errors logged, oldest first.
pub fn recent() -> Vec<Problem> {
    RECENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

struct Recent;

impl<S> Layer<S> for Recent
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => "error",
            Level::WARN => "warning",
            _ => return,
        };
        let mut message = Message(String::new());
        event.record(&mut message);
        let mut context = String::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if !context.is_empty() {
                    context.push_str(": ");
                }
                context.push_str(span.name());
                if let Some(fields) = span.extensions().get::<FormattedFields<DefaultFields>>()
                    && !fields.is_empty()
                {
                    let _ = write!(context, "{{{fields}}}");
                }
            }
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let mut recent = RECENT.lock().unwrap_or_else(PoisonError::into_inner);
        if recent.len() == KEEP {
            recent.pop_front();
        }
        recent.push_back(Problem {
            time,
            level,
            context,
            message: message.0,
        });
    }
}

/// An event's text: its message, then any other fields as `name=value`.
struct Message(String);

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.insert_str(0, value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// Whether stderr is the journal stream systemd connected, not merely a
/// process that inherited `JOURNAL_STREAM` (a terminal opened from a
/// systemd-managed session does).
fn stderr_is_journal() -> bool {
    let Some(stream) = std::env::var("JOURNAL_STREAM").ok() else {
        return false;
    };
    let Some((dev, ino)) = stream.split_once(':') else {
        return false;
    };
    rustix::fs::fstat(std::io::stderr())
        .is_ok_and(|st| dev.parse() == Ok(st.st_dev) && ino.parse() == Ok(st.st_ino))
}

/// The name this program was run as, as error messages conventionally show.
pub fn program() -> String {
    let argv0 = std::env::args().next().unwrap_or_default();
    std::path::Path::new(&argv0)
        .file_name()
        .map_or_else(|| "steward".into(), |n| n.to_string_lossy().into_owned())
}

struct Line {
    program: String,
    journald: bool,
    timestamps: bool,
    colour: bool,
}

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = *event.metadata().level();
        if self.journald {
            let priority = match level {
                Level::ERROR => 3,
                Level::WARN => 4,
                Level::INFO => 6,
                _ => 7,
            };
            write!(w, "<{priority}>")?;
        } else if self.timestamps {
            write!(w, "{} ", chrono::Local::now().format("%H:%M:%S%.3f"))?;
        }
        write!(w, "{}: ", self.program)?;
        let label = match level {
            Level::ERROR => Some(("error", "\x1b[1;31m")),
            Level::WARN => Some(("warning", "\x1b[1;33m")),
            _ => None,
        };
        match label {
            Some((name, ansi)) if self.colour => write!(w, "{ansi}{name}:\x1b[0m ")?,
            Some((name, _)) => write!(w, "{name}: ")?,
            None => {}
        }
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                w.write_str(span.name())?;
                let ext = span.extensions();
                if let Some(fields) = ext.get::<FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(w, "{{{fields}}}")?;
                }
                w.write_str(": ")?;
            }
        }
        ctx.field_format().format_fields(w.by_ref(), event)?;
        writeln!(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_and_errors_are_kept_with_their_spans() {
        init(Level::WARN, 0, false);
        let span = tracing::warn_span!("hash", path = "/media/tv");
        span.in_scope(|| {
            tracing::warn!(attempt = 2, "saving content ids: stale; retrying");
            tracing::info!("not kept");
        });
        tracing::error!("hash /media/tv: gave up");
        let got = recent();
        let warn = got.iter().find(|p| p.level == "warning").unwrap();
        assert_eq!(warn.context, "hash{path=\"/media/tv\"}");
        assert_eq!(
            warn.message,
            "saving content ids: stale; retrying attempt=2"
        );
        assert!(
            got.iter()
                .any(|p| p.level == "error" && p.message == "hash /media/tv: gave up")
        );
        assert!(!got.iter().any(|p| p.message == "not kept"));
    }
}
