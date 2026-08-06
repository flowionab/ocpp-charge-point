//! A minimal abstraction over "what time is it", so OCPP-facing timestamps don't force a hard
//! dependency on chrono's `clock` feature (which needs an OS/wasm time source, i.e. `std`).
//! See `docs/ROADMAP.md` §0 and `docs/PRODUCTION-ROADMAP.md` §9.3 (G3 - Time).

use chrono::{DateTime, TimeZone, Utc};
use core::time::Duration;

/// Supplies the current time for OCPP-facing timestamps (e.g. StatusNotification,
/// TransactionEvent timestamps). Implement this on embedded targets without a
/// `clock`-capable chrono - typically backed by an RTC peripheral.
///
/// A charge point with no RTC (see [`crate::hardware::Capabilities::has_rtc`])
/// still needs *some* `now()` to satisfy this trait - callers cannot assume every `Clock`
/// implementation is trustworthy. Such an implementation should return a value before
/// [`unsynchronized_before`] (e.g. the Unix epoch, or whatever a stopped/uninitialized RTC
/// peripheral naturally reads) until real time is established, rather than fabricating a
/// plausible-looking date - see [`is_synchronized`], which callers use to detect this rather
/// than trusting every `now()` at face value.
pub trait Clock {
    /// The current time. May be wrong or unset (see this trait's docs) until the charge point
    /// has a trustworthy time source, either an RTC or a CSMS-supplied `currentTime`.
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

/// The earliest wall-clock time treated as plausibly real by [`is_synchronized`]. Hardware
/// without an RTC ([`crate::hardware::Capabilities::has_rtc`] is `false`)
/// conventionally boots reading the Unix epoch or some other fixed, implausible date rather than
/// the actual current time - see [`Clock`]'s docs. This constant is set comfortably after any
/// such default and before this crate existed, so a genuinely synchronized clock (RTC-backed, or
/// corrected from a CSMS's `currentTime` - see `crate::provisioning`) can never be mistaken for
/// an unset one, and an unset one can never be mistaken for real.
///
/// A function rather than a `const DateTime<Utc>` because `DateTime`'s constructors aren't
/// `const fn`; the parse is infallible (a fixed, valid literal) so this never panics in practice.
pub fn unsynchronized_before() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .expect("2024-01-01T00:00:00Z is a valid, unambiguous UTC timestamp")
}

/// Whether `now` looks like real wall-clock time rather than an unset RTC's default reading -
/// see [`unsynchronized_before`]. G3.1: callers about to timestamp something CSMS-visible (e.g.
/// a transaction record) should check this first, and honestly represent an unsynchronized clock
/// rather than sending a plausible-looking but fabricated timestamp - see `crate::provisioning`'s
/// time-sync helpers for how a real time value later arrives (from `BootNotification`/
/// `Heartbeat`'s `currentTime`) and corrects it.
pub fn is_synchronized(now: &DateTime<Utc>) -> bool {
    *now >= unsynchronized_before()
}

/// An opaque reading from a [`MonotonicClock`]: a tick count with no relationship to wall-clock
/// time, and no meaning except relative to another `MonotonicInstant` from the *same* clock
/// instance. Use [`MonotonicInstant::duration_since`] to turn two readings into an elapsed
/// [`Duration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicInstant(u64);

impl MonotonicInstant {
    /// Constructs a `MonotonicInstant` directly from a raw tick count. Exposed for
    /// [`MonotonicClock`] implementations (e.g. one backed by a hardware tick counter); most
    /// callers should obtain instances via [`MonotonicClock::now`] instead.
    pub fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// The elapsed time between `earlier` and `self`, both readings from the same
    /// [`MonotonicClock`]. Saturates to [`Duration::ZERO`] if `earlier` is later than `self`
    /// (e.g. readings passed in the wrong order, or from clocks that aren't actually the same
    /// source) rather than panicking or wrapping - this is the fail-safe counterpart to
    /// wall-clock subtraction, which a backwards clock step would corrupt (see G3.3 in
    /// `docs/PRODUCTION-ROADMAP.md` §9.3): a monotonic reading only ever moves forward, so a
    /// transaction's measured duration cannot be thrown off by a `SettingSystemTime` correction
    /// applied mid-session.
    pub fn duration_since(&self, earlier: MonotonicInstant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// Supplies a monotonically increasing time reading, independent of wall-clock time - see
/// [`MonotonicInstant`]. Distinct from [`Clock`] because the two must never be confused: `Clock`
/// can jump (an unset RTC catching up, a CSMS `SetSystemTime`/sync correction - see
/// `crate::provisioning`), while a `MonotonicClock` reading only ever moves forward. Anything
/// measuring an elapsed *duration* (rather than reporting a point-in-time timestamp) should use
/// this trait, not [`Clock`]. Implement this on embedded targets with a free-running hardware
/// timer/tick counter; [`SystemMonotonicClock`] is the `std` default, backed by
/// [`std::time::Instant`].
pub trait MonotonicClock {
    /// The current monotonic reading. Only meaningful relative to another reading from the same
    /// clock instance - see [`MonotonicInstant::duration_since`].
    fn now(&self) -> MonotonicInstant;
}

/// A [`MonotonicClock`] backed by [`std::time::Instant`]. Requires the `std` feature.
///
/// Ticks are nanoseconds elapsed since this clock type's first use in the process (lazily
/// established via a process-wide [`std::sync::OnceLock`]), not since any particular epoch -
/// consistent with [`MonotonicInstant`]'s "opaque, relative-only" contract.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMonotonicClock;

#[cfg(feature = "std")]
impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> MonotonicInstant {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = EPOCH.get_or_init(std::time::Instant::now);
        MonotonicInstant::from_ticks(epoch.elapsed().as_nanos() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_before_the_sentinel_is_not_synchronized() {
        let unset_rtc_reading = DateTime::<Utc>::from_timestamp(0, 0).unwrap(); // Unix epoch
        assert!(!is_synchronized(&unset_rtc_reading));
    }

    #[test]
    fn a_timestamp_after_the_sentinel_is_synchronized() {
        let plausible_now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).single().unwrap();
        assert!(is_synchronized(&plausible_now));
    }

    #[test]
    fn the_sentinel_itself_counts_as_synchronized() {
        assert!(is_synchronized(&unsynchronized_before()));
    }

    #[cfg(feature = "std")]
    #[test]
    fn system_clock_reports_a_synchronized_time() {
        assert!(is_synchronized(&SystemClock.now()));
    }

    #[test]
    fn monotonic_instant_duration_since_measures_forward_elapsed_time() {
        let earlier = MonotonicInstant::from_ticks(1_000_000_000); // 1s
        let later = MonotonicInstant::from_ticks(3_500_000_000); // 3.5s
        assert_eq!(later.duration_since(earlier), Duration::from_millis(2_500));
    }

    #[test]
    fn monotonic_instant_duration_since_saturates_rather_than_panicking_or_wrapping() {
        // A stand-in for what a naive wall-clock subtraction would get wrong: if a backwards
        // step ever produced a "later" reading smaller than "earlier", the elapsed duration must
        // not underflow to a huge bogus value or panic - it must clamp to zero.
        let earlier = MonotonicInstant::from_ticks(5_000_000_000);
        let later = MonotonicInstant::from_ticks(1_000_000_000);
        assert_eq!(later.duration_since(earlier), Duration::ZERO);
    }

    #[cfg(feature = "std")]
    #[test]
    fn system_monotonic_clock_never_goes_backwards_and_is_unaffected_by_wall_clock() {
        let clock = SystemMonotonicClock;
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(5));
        let second = clock.now();

        assert!(second >= first);
        assert!(second.duration_since(first) >= Duration::from_millis(5));
    }
}
