//! Logging shared by stewardd, the CLI and the GUI: `tracing` events written
//! to stderr one line each, as `program: warning: span{fields}: message`.
//!
//! When stderr is the journal (`JOURNAL_STREAM` names it) each line starts with its syslog
//! priority (`<4>`) so the journal records the level, and carries no
//! timestamp, which the journal adds. Elsewhere `timestamps` prefixes local
//! time, and on a terminal warnings and errors are coloured.

use std::fmt;
use std::io::IsTerminal;

pub use tracing::Level;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

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
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // Colour is ours (warning and error labels); fields stay plain.
        .with_ansi(false)
        .event_format(line)
        .try_init();
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
