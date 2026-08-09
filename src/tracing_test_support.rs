//! A `tracing` subscriber that captures events, for tests that assert on what this crate logs.
//!
//! Log level and log content are behaviour here, not decoration - see `CLAUDE.md`'s logging
//! rules. A `{:?}` of `ChargePointState` at `INFO` is a real defect on an MCU, and an `IdToken`
//! value reaching any level at all is a privacy one, so both are worth a test rather than a
//! convention. This module is what those tests are written against.
//!
//! Note that a thread-local scoped subscriber does **not** reach work the actor runs on a spawned
//! task. Either drive the code under test on the current thread and attach the subscriber with
//! [`tracing::instrument::WithSubscriber`], or assert against a function called directly.

use alloc::string::String;
use alloc::vec::Vec;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

/// One captured `tracing` event: its level, and its fields and message rendered flat.
#[derive(Clone, Debug)]
pub(crate) struct Captured {
    /// The level the event was emitted at.
    pub(crate) level: tracing::Level,
    /// The event's fields and message, rendered as ` name=value` pairs.
    pub(crate) rendered: String,
}

/// A [`Layer`] that records every event it sees into a shared buffer.
#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<Captured>>>);

impl Capture {
    /// Everything captured so far.
    pub(crate) fn events(&self) -> Vec<Captured> {
        self.0.lock().expect("capture mutex").clone()
    }

    /// Every captured event's text joined, for "this string never appears anywhere" assertions.
    pub(crate) fn all_text(&self) -> String {
        use alloc::string::ToString;

        self.events()
            .iter()
            .map(|e| e.rendered.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct FlatVisitor(String);

impl Visit for FlatVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        use core::fmt::Write;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        use core::fmt::Write;
        let _ = write!(self.0, " {}={}", field.name(), value);
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FlatVisitor(String::new());
        event.record(&mut visitor);
        self.0.lock().expect("capture mutex").push(Captured {
            level: *event.metadata().level(),
            rendered: visitor.0,
        });
    }
}

/// Runs `f` with a capturing subscriber attached, and returns what it captured alongside its
/// output.
///
/// Attached to the future rather than to the thread, so it still applies across every `.await`
/// point inside it - and scoped rather than global, because several of these run concurrently
/// under `cargo test` and a global default can only be set once per process.
pub(crate) async fn capture_from_future<T>(
    max_level: tracing::level_filters::LevelFilter,
    f: impl core::future::Future<Output = T>,
) -> (Capture, T) {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry()
        .with(capture.clone())
        .with(max_level);
    let out = f.with_subscriber(subscriber).await;
    (capture, out)
}
