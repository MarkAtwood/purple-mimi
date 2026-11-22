// SPDX-License-Identifier: GPL-3.0-or-later
//! The library's log as events (docs/design/purple-mimi.md §5.4): a
//! `tracing` layer that turns each log event of purple-mimi and mimid into
//! an [`EventKind::Log`], which the glue writes with `purple_debug_*` on
//! the main thread.
//!
//! mimid's secret-bearing types redact their `Debug` output (AGENTS.md
//! "Secrets never reach logs"), and nothing here logs a token.
//!
//! # Limitations
//!
//! - The layer is the process's global `tracing` subscriber. In a plugin
//!   built as one shared object with its symbols hidden, that global is
//!   the plugin's own. If something else in the same Rust image set a
//!   global subscriber first (a test harness), nothing is forwarded.
//! - Only the targets of purple-mimi and mimid are forwarded; the HTTP,
//!   TLS and WebSocket crates' logs are dropped.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{Event as TracingEvent, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use crate::events::{Event, EventKind, EventQueue, LogLevel};

/// The level from which events are forwarded: [`LogLevel`] as a number, 0
/// for none.
static LEVEL: AtomicI32 = AtomicI32::new(LogLevel::Warn as i32);

/// Where events go: the running process's queue.
static SINK: Mutex<Option<Weak<EventQueue>>> = Mutex::new(None);

static INSTALLED: OnceLock<bool> = OnceLock::new();

const TARGETS: [&str; 5] = ["purple_mimi", "mimid", "mimi_", "mls_didwebvh", "openmls"];

/// Sets the level from which log events are forwarded: 0 for none, 1
/// (errors) to 5 (everything). Values outside are clamped.
pub fn set_level(level: i32) {
    LEVEL.store(level.clamp(0, LogLevel::Trace as i32), Ordering::Relaxed);
}

/// The current level, as [`set_level`] takes it.
#[must_use]
pub fn level() -> i32 {
    LEVEL.load(Ordering::Relaxed)
}

/// Forwards log events to `queue` from now on, installing the layer as the
/// global subscriber the first time. Whether it is installed.
pub(crate) fn attach(queue: &Arc<EventQueue>) -> bool {
    *SINK.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(queue));
    *INSTALLED.get_or_init(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(QueueLayer))
            .is_ok()
    })
}

/// Stops forwarding.
pub(crate) fn detach() {
    *SINK.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

fn number(level: Level) -> LogLevel {
    match level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warn,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        Level::TRACE => LogLevel::Trace,
    }
}

fn wanted_target(target: &str) -> bool {
    TARGETS.iter().any(|t| target.starts_with(t))
}

/// The layer.
struct QueueLayer;

impl<S: Subscriber> Layer<S> for QueueLayer {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        // The level changes at run time, so no callsite is cached as
        // always or never, except other crates' targets.
        if wanted_target(metadata.target()) {
            Interest::sometimes()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        wanted_target(metadata.target())
            && (number(*metadata.level()) as i32) <= LEVEL.load(Ordering::Relaxed)
    }

    fn on_event(&self, event: &TracingEvent<'_>, _: Context<'_, S>) {
        let Some(queue) = SINK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
        else {
            return;
        };
        let metadata = event.metadata();
        let mut line = Line::default();
        event.record(&mut line);
        let mut text = format!("{}: {}", metadata.target(), line.message);
        text.push_str(&line.fields);
        queue.push(Event {
            account: None,
            kind: EventKind::Log {
                level: number(*metadata.level()),
                text,
            },
        });
    }
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let _ = write!(self.fields, " {}={value}", field.name());
        let mut source = value.source();
        while let Some(next) = source {
            let _ = write!(self.fields, ": {next}");
            source = next.source();
        }
    }
}
