//! A minimal abstraction over "what time is it", so OCPP-facing timestamps don't force a hard
//! dependency on chrono's `clock` feature (which needs an OS/wasm time source, i.e. `std`).
//! See `docs/ROADMAP.md` §0.

use chrono::{DateTime, Utc};

/// Supplies the current time for OCPP-facing timestamps (e.g. StatusNotification,
/// TransactionEvent timestamps). Implement this on embedded targets without a
/// `clock`-capable chrono - typically backed by an RTC peripheral.
pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
}

/// A [`Clock`] backed by `chrono::Utc::now()` (the host/OS clock). Requires the `std` feature.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[cfg(feature = "std")]
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
