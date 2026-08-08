//! The periodic event stream store - streams the CSMS has opened against a variable monitor, the
//! state behind OCPP 2.1's `OpenPeriodicEventStream`/`ClosePeriodicEventStream`/
//! `AdjustPeriodicEventStream`/`GetPeriodicEventStream`, and the source
//! [`crate::periodic_event_stream::run_periodic_event_streams`] drives `NotifyPeriodicEventStream`
//! from. See `docs/PRODUCTION-ROADMAP.md` B5.6.
//!
//! **2.1-only.** Neither 1.6J nor 2.0.1 has any periodic event stream concept - a spec boundary,
//! not a gap - so this model exists purely for the 2.1 adapter
//! (`crate::periodic_event_stream::ocpp_2_1`) to project onto.

use alloc::vec::Vec;

use crate::state::VariableMonitorId;

/// A periodic event stream's CSMS-assigned identifier (OCPP `ConstantStreamData.id`). Unlike
/// [`VariableMonitorId`], OCPP has the CSMS choose this id up front in `OpenPeriodicEventStream`
/// itself - there is no charge-point-side allocation to model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeriodicEventStreamId(pub i64);

/// A stream's tunable parameters (OCPP `PeriodicEventStreamParams`), shared by
/// `OpenPeriodicEventStream` and `AdjustPeriodicEventStream`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodicEventStreamParams {
    /// How often (in seconds) stream data should be sent. `None` falls back to
    /// [`DEFAULT_PERIODIC_EVENT_STREAM_INTERVAL_SECS`] - mirrors
    /// [`crate::state::MonitorType::Periodic`]'s `value` field, which OCPP also lets a CSMS leave
    /// implicit.
    pub interval_secs: Option<i64>,
    /// How many data elements to batch into one `NotifyPeriodicEventStream` before sending it.
    /// `None` means "send as soon as one is available" (batch size 1) - the conservative default,
    /// since a charge point that doesn't yet buffer multiple samples cannot honour a larger batch
    /// without inventing data.
    pub values: Option<i64>,
}

/// [`PeriodicEventStreamParams::interval_secs`]'s fallback when the CSMS doesn't specify one - the
/// same 60 s cadence [`crate::builder::ChargePointBuilder::variable_monitor_events`] and
/// [`crate::builder::ChargePointBuilder::reservation_status_updates`] already default their own
/// sweeps to.
pub const DEFAULT_PERIODIC_EVENT_STREAM_INTERVAL_SECS: i64 = 60;

/// One stream open on the charge point.
///
/// Owned by [`PeriodicEventStreamStore`], itself owned by [`crate::state::ChargePointState`] and
/// mutated only through [`crate::state::ChargePointEvent`], like every other piece of state in
/// this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenPeriodicEventStream {
    /// This stream's id.
    pub id: PeriodicEventStreamId,
    /// The variable monitor whose value this stream reports (OCPP
    /// `ConstantStreamData.variableMonitoringId`). Does not have to name a monitor that currently
    /// exists - see [`crate::periodic_event_stream::handle_open_periodic_event_stream`]'s docs for
    /// why the *open* check is stricter than what the store itself enforces.
    pub variable_monitoring_id: VariableMonitorId,
    /// The stream's current parameters - installed by `OpenPeriodicEventStream`, replaceable by
    /// `AdjustPeriodicEventStream`.
    pub params: PeriodicEventStreamParams,
}

/// Why [`PeriodicEventStreamStore::open`] refused a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodicEventStreamOpenRejection {
    /// The store is already holding
    /// [`crate::state::StateLimits::max_periodic_event_streams`] streams and this one is not
    /// replacing any of them (mirrors [`crate::state::TariffSetRejection::TooManyTariffs`]'s
    /// reasoning - a bound a remote peer can push past is not a bound).
    TooManyStreams,
}

/// Every periodic event stream currently open on the charge point.
///
/// Bounded by [`crate::state::StateLimits::max_periodic_event_streams`] - an MCU cannot afford an
/// unbounded table of live streams, each of which drives its own recurring
/// `NotifyPeriodicEventStream` traffic. Owned by [`crate::state::ChargePointState`] and mutated
/// only through [`crate::state::ChargePointEvent::PeriodicEventStreamOpened`]/
/// [`PeriodicEventStreamClosed`](crate::state::ChargePointEvent::PeriodicEventStreamClosed)/
/// [`PeriodicEventStreamAdjusted`](crate::state::ChargePointEvent::PeriodicEventStreamAdjusted).
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodicEventStreamStore {
    streams: Vec<OpenPeriodicEventStream>,
    max_streams: usize,
}

impl PeriodicEventStreamStore {
    /// An empty store holding at most `max_streams` streams (clamped to at least 1).
    pub fn with_limit(max_streams: usize) -> Self {
        Self {
            streams: Vec::new(),
            max_streams: max_streams.max(1),
        }
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_periodic_event_streams`].
    pub fn max_streams(&self) -> usize {
        self.max_streams
    }

    /// Every open stream, in the order they were opened.
    pub fn iter(&self) -> impl Iterator<Item = &OpenPeriodicEventStream> {
        self.streams.iter()
    }

    /// How many streams are open.
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Whether nothing is open.
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// The open stream with this id, if any.
    pub fn get(&self, id: PeriodicEventStreamId) -> Option<&OpenPeriodicEventStream> {
        self.streams.iter().find(|stream| stream.id == id)
    }

    /// Opens `stream`, replacing whatever stream currently occupies its id - `OpenPeriodicEventStream`
    /// is defined against a CSMS-chosen id, so re-opening the same id is treated as a fresh
    /// definition rather than a duplicate error, mirroring [`crate::state::TariffStore::set_default`]'s
    /// per-scope replacement.
    pub fn open(
        &mut self,
        stream: OpenPeriodicEventStream,
    ) -> Result<(), PeriodicEventStreamOpenRejection> {
        let replacing = self.streams.iter().any(|existing| existing.id == stream.id);
        if !replacing && self.streams.len() >= self.max_streams {
            return Err(PeriodicEventStreamOpenRejection::TooManyStreams);
        }
        self.streams.retain(|existing| existing.id != stream.id);
        self.streams.push(stream);
        Ok(())
    }

    /// Closes the stream named `id`, returning whether one was actually open.
    pub fn close(&mut self, id: PeriodicEventStreamId) -> bool {
        let before = self.streams.len();
        self.streams.retain(|stream| stream.id != id);
        self.streams.len() != before
    }

    /// Replaces the parameters of the open stream named `id`, returning whether one was found -
    /// `AdjustPeriodicEventStream` never changes which monitor a stream reads.
    pub fn adjust(&mut self, id: PeriodicEventStreamId, params: PeriodicEventStreamParams) -> bool {
        match self.streams.iter_mut().find(|stream| stream.id == id) {
            Some(stream) => {
                stream.params = params;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(id: i64) -> OpenPeriodicEventStream {
        OpenPeriodicEventStream {
            id: PeriodicEventStreamId(id),
            variable_monitoring_id: VariableMonitorId(1),
            params: PeriodicEventStreamParams {
                interval_secs: Some(30),
                values: None,
            },
        }
    }

    #[test]
    fn opening_a_new_id_adds_it() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        store.open(stream(1)).unwrap();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn opening_an_existing_id_replaces_it() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        store.open(stream(1)).unwrap();
        let mut replacement = stream(1);
        replacement.params.interval_secs = Some(99);
        store.open(replacement).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .get(PeriodicEventStreamId(1))
                .unwrap()
                .params
                .interval_secs,
            Some(99)
        );
    }

    #[test]
    fn the_bound_refuses_a_new_stream_but_never_a_replacement() {
        let mut store = PeriodicEventStreamStore::with_limit(1);
        store.open(stream(1)).unwrap();

        assert_eq!(
            store.open(stream(2)),
            Err(PeriodicEventStreamOpenRejection::TooManyStreams)
        );
        assert_eq!(store.open(stream(1)), Ok(()));
    }

    #[test]
    fn closing_an_open_stream_removes_it_and_reports_true() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        store.open(stream(1)).unwrap();

        assert!(store.close(PeriodicEventStreamId(1)));
        assert!(store.is_empty());
    }

    #[test]
    fn closing_an_unknown_stream_reports_false() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        assert!(!store.close(PeriodicEventStreamId(99)));
    }

    #[test]
    fn adjusting_an_open_stream_replaces_its_params() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        store.open(stream(1)).unwrap();

        let adjusted = store.adjust(
            PeriodicEventStreamId(1),
            PeriodicEventStreamParams {
                interval_secs: Some(5),
                values: Some(3),
            },
        );

        assert!(adjusted);
        let installed = store.get(PeriodicEventStreamId(1)).unwrap();
        assert_eq!(installed.params.interval_secs, Some(5));
        assert_eq!(installed.params.values, Some(3));
    }

    #[test]
    fn adjusting_an_unknown_stream_reports_false() {
        let mut store = PeriodicEventStreamStore::with_limit(10);
        assert!(!store.adjust(
            PeriodicEventStreamId(99),
            PeriodicEventStreamParams {
                interval_secs: Some(5),
                values: None,
            }
        ));
    }
}
