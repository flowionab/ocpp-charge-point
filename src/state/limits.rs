/// Default maximum number of [`crate::state::LocalListEntry`]s the local authorization list holds
/// (see [`StateLimits::max_local_authorization_list_entries`]).
///
/// 100 entries bounds the list to a few KB (an entry is an id token string plus a binary
/// accept/reject decision - well under 100 bytes each once heap overhead is counted) while still
/// covering a private/fleet site's whole population of cards, which is the case offline
/// authorization actually exists for. Public sites cannot usefully cache "every driver who might
/// arrive" at any bound, so a bigger default would buy nothing.
///
/// Like [`crate::offline_queue::DEFAULT_CAPACITY`], this is a documented estimate rather than a
/// measured RAM budget (that's G2.3 in `docs/PRODUCTION-ROADMAP.md` §9.2); integrators with a
/// larger card population, or a tighter RAM budget, should pick their own via
/// [`StateLimits::with_max_local_authorization_list_entries`].
pub const DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES: usize = 100;

/// Default maximum number of `(Component, Variable)` pairs the device model holds (see
/// [`StateLimits::max_device_model_variables`]).
///
/// 256 leaves generous room above this crate's own registrations - the built-in defaults plus one
/// `*Ctrlr.Available` variable per [`crate::hardware::CAPABILITY_GATES`] entry, a couple of dozen
/// in total - for whatever components a hardware binding adds for its real hardware, while still
/// bounding the model to tens of KB. A binding that legitimately exposes more (a large multi-EVSE
/// site registering per-connector components, say) should raise it via
/// [`StateLimits::with_max_device_model_variables`] rather than have registrations silently
/// refused.
pub const DEFAULT_MAX_DEVICE_MODEL_VARIABLES: usize = 256;

/// Default maximum number of charging profiles the store holds (see
/// [`StateLimits::max_charging_profiles`]).
///
/// 16 covers what a real installation actually stacks - an installation limit, a couple of
/// TxDefault profiles at different stack levels, one TxProfile per simultaneously-charging
/// connector, and an external-constraints profile - across a small-to-mid-size charge point,
/// while bounding the store to a few KB (a profile is a handful of scalars plus its schedule
/// periods). A CSMS that legitimately drives more (a large DC site with per-connector schedules)
/// should raise it via [`StateLimits::with_max_charging_profiles`] rather than have
/// `SetChargingProfile` refused; see `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2) for why the bound
/// exists at all.
pub const DEFAULT_MAX_CHARGING_PROFILES: usize = 16;

/// Default maximum number of variable monitors the store holds (see
/// [`StateLimits::max_variable_monitors`]).
///
/// 32 covers a generous handful of thresholds/deltas/periodics per variable a CSMS actually wants
/// watched (a temperature or voltage upper bound, a meter delta, an occasional periodic check) on
/// a small-to-mid-size charge point, while bounding the store to a few KB (a monitor is a handful
/// of scalars, no schedule attached). A CSMS driving a larger fleet-monitoring deployment should
/// raise it via [`StateLimits::with_max_variable_monitors`] rather than have
/// `SetVariableMonitoring` refused; see `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2) for why the
/// bound exists at all.
pub const DEFAULT_MAX_VARIABLE_MONITORS: usize = 32;

/// Default maximum number of default tariffs the tariff store holds (see
/// [`StateLimits::max_tariffs`]).
///
/// 8 covers a charge-point-wide default plus one override per EVSE on a small-to-mid-size
/// installation, while bounding the store to a couple of KB - a tariff, deliberately shallow (see
/// [`crate::state::Tariff`]), is just an id, a currency code and an optional timestamp. A site
/// with more EVSEs than that needing per-EVSE tariffs should raise it via
/// [`StateLimits::with_max_tariffs`] rather than have `SetDefaultTariff` refused.
pub const DEFAULT_MAX_TARIFFS: usize = 8;

/// Default maximum number of concurrently open periodic event streams (see
/// [`StateLimits::max_periodic_event_streams`]).
///
/// 4 is deliberately small: each open stream drives its own recurring `NotifyPeriodicEventStream`
/// traffic (see [`crate::periodic_event_stream::run_periodic_event_streams`]), unlike the mostly
/// passive stores elsewhere in this table - a handful of concurrently monitored variables is
/// already a lot of continuous outbound chatter for a small-to-mid-size charge point to sustain. A
/// site legitimately streaming more variables at once should raise it via
/// [`StateLimits::with_max_periodic_event_streams`] rather than have `OpenPeriodicEventStream`
/// refused.
pub const DEFAULT_MAX_PERIODIC_EVENT_STREAMS: usize = 4;

/// Default maximum number of display messages the store holds - see
/// [`crate::state::DEFAULT_MAX_DISPLAY_MESSAGES`], which documents the reasoning; this re-export
/// exists so every bound in [`StateLimits`] has a `DEFAULT_*` constant beside it.
pub use crate::state::display_message::DEFAULT_MAX_DISPLAY_MESSAGES;

/// Default maximum number of cached authorization decisions - see
/// [`crate::state::DEFAULT_MAX_AUTHORIZATION_CACHE_ENTRIES`], which documents the reasoning; this
/// re-export exists so every bound in [`StateLimits`] has a `DEFAULT_*` constant beside it.
pub use crate::state::authorization_cache::DEFAULT_MAX_AUTHORIZATION_CACHE_ENTRIES;

/// Default maximum number of occupied network profile slots - see
/// [`crate::state::DEFAULT_MAX_NETWORK_PROFILE_SLOTS`].
pub use crate::state::network_profile::DEFAULT_MAX_NETWORK_PROFILE_SLOTS;

/// The configured maxima for every growable collection in [`crate::state::ChargePointState`], so a
/// charge point's peak memory is a function of its configuration rather than of how much a CSMS or
/// a hardware binding decides to push at it - see `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2). Passed
/// once at construction ([`crate::state::ChargePointState::with_limits`],
/// [`crate::actor::ChargePointActor::spawn_with_limits`],
/// [`crate::runtime::ChargePointRuntime::new_with_limits`],
/// [`crate::builder::ChargePointBuilder::start_with_limits`]) and thereafter enforced by the
/// collections themselves; there is no way to raise a limit at runtime, deliberately - a bound that
/// a remote peer can move is not a bound.
///
/// [`Default`] uses this crate's documented defaults ([`DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES`],
/// [`DEFAULT_MAX_DEVICE_MODEL_VARIABLES`]), which is what
/// [`crate::state::ChargePointState::new`] and every other non-`_with_limits` constructor uses.
///
/// The collections *not* listed here are already bounded by construction rather than by a
/// configured maximum, and need no entry: the EVSE/connector/transaction/reservation/running-cost
/// vectors are all sized once from the hardware binding's topology and never grow (one active
/// transaction and one reservation per connector, by the state machine's own invariants), and the
/// offline report queues carry their own bound - see [`crate::offline_queue::OfflineQueue`] (G2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateLimits {
    /// The most entries the local authorization list may hold. A CSMS `SendLocalList` that would
    /// exceed it is refused with
    /// [`SendLocalListOutcome::TooManyEntries`](crate::local_authorization_list::SendLocalListOutcome::TooManyEntries)
    /// rather than applied in part; a list restored from durable storage that exceeds it (written
    /// by a build configured with a larger maximum) is truncated, and that truncation raises a
    /// `MemoryExhaustion` security event. Clamped to at least 1.
    pub max_local_authorization_list_entries: usize,
    /// The most `(Component, Variable)` pairs the device model may hold, counting this crate's own
    /// built-in defaults and capability-gate variables. A
    /// [`VariableRegistered`](crate::state::DeviceModelEvent::VariableRegistered) beyond it is
    /// logged and ignored. Clamped to at least 1, and never below what
    /// [`crate::state::DeviceModel::new`] itself registers - a limit low enough to drop this
    /// crate's own defaults would break `GetVariables`/`GetBaseReport` rather than bound anything
    /// worth bounding.
    pub max_device_model_variables: usize,
    /// The most charging profiles the store may hold across every scope. A `SetChargingProfile`
    /// that would exceed it is refused with
    /// [`ChargingProfileRejection::TooManyProfiles`](crate::state::ChargingProfileRejection::TooManyProfiles)
    /// rather than evicting one the CSMS still believes is installed - unlike the offline queues,
    /// where dropping the oldest message is the lesser evil, silently forgetting a *limit* would
    /// let the charge point draw more current than the CSMS last told it to. Replacing an
    /// already-installed profile is always allowed, even at the bound. Clamped to at least 1.
    pub max_charging_profiles: usize,
    /// The most default tariffs the tariff store may hold across every scope (OCPP 2.1
    /// `SetDefaultTariff`). A `SetDefaultTariff` that would exceed it is refused with
    /// [`TariffSetRejection::TooManyTariffs`](crate::state::TariffSetRejection::TooManyTariffs)
    /// rather than evicting one the CSMS still believes is installed. Replacing the tariff already
    /// installed at a scope is always allowed, even at the bound. Clamped to at least 1.
    pub max_tariffs: usize,
    /// The most authorization decisions the cache may remember. Reaching it evicts the least
    /// recently authorized entry rather than refusing the new one - unlike
    /// [`Self::max_charging_profiles`], where forgetting is a safety problem: a forgotten *cache*
    /// entry just means the next `Authorize` for that token goes to the CSMS, which is the normal
    /// path anyway. Clamped to at least 1.
    pub max_authorization_cache_entries: usize,
    /// The most network profile configuration slots that may be occupied. A `SetNetworkProfile`
    /// for a *new* slot beyond it is refused with OCPP's `Rejected`; replacing an already-occupied
    /// slot always succeeds. Clamped to at least 1.
    pub max_network_profile_slots: usize,
    /// The most variable monitors the store may hold. A `SetVariableMonitoring` request for a
    /// *new* monitor (see [`crate::state::VariableMonitorStore::precheck`]) beyond it is refused
    /// with OCPP's `Rejected`; replacing an already-installed monitor's id always succeeds.
    /// Clamped to at least 1.
    pub max_variable_monitors: usize,
    /// The most [`crate::state::DisplayedMessage`]s the display message store may hold. A
    /// `SetDisplayMessage` for a *new* id beyond it is refused (see
    /// [`crate::state::DisplayMessageStore::set`]); replacing an already-stored id always
    /// succeeds. Clamped to at least 1.
    pub max_display_messages: usize,
    /// The most periodic event streams ([`crate::state::PeriodicEventStreamStore`]) that may be
    /// open at once. An `OpenPeriodicEventStream` for a *new* id beyond it is refused (see
    /// [`crate::state::PeriodicEventStreamStore::open`]); re-opening an already-open id always
    /// succeeds. Clamped to at least 1.
    pub max_periodic_event_streams: usize,
}

impl StateLimits {
    /// This crate's documented default maxima - the same values [`Default`] yields, in a `const`
    /// context.
    pub const fn new() -> Self {
        Self {
            max_local_authorization_list_entries: DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES,
            max_device_model_variables: DEFAULT_MAX_DEVICE_MODEL_VARIABLES,
            max_charging_profiles: DEFAULT_MAX_CHARGING_PROFILES,
            max_tariffs: DEFAULT_MAX_TARIFFS,
            max_authorization_cache_entries: DEFAULT_MAX_AUTHORIZATION_CACHE_ENTRIES,
            max_network_profile_slots: DEFAULT_MAX_NETWORK_PROFILE_SLOTS,
            max_variable_monitors: DEFAULT_MAX_VARIABLE_MONITORS,
            max_display_messages: DEFAULT_MAX_DISPLAY_MESSAGES,
            max_periodic_event_streams: DEFAULT_MAX_PERIODIC_EVENT_STREAMS,
        }
    }

    /// Overrides [`Self::max_periodic_event_streams`].
    pub const fn with_max_periodic_event_streams(mut self, max: usize) -> Self {
        self.max_periodic_event_streams = max;
        self
    }

    /// Overrides [`Self::max_tariffs`].
    pub const fn with_max_tariffs(mut self, max: usize) -> Self {
        self.max_tariffs = max;
        self
    }

    /// Overrides [`Self::max_network_profile_slots`].
    pub const fn with_max_network_profile_slots(mut self, max: usize) -> Self {
        self.max_network_profile_slots = max;
        self
    }

    /// Overrides [`Self::max_variable_monitors`].
    pub const fn with_max_variable_monitors(mut self, max: usize) -> Self {
        self.max_variable_monitors = max;
        self
    }

    /// Overrides [`Self::max_display_messages`].
    pub const fn with_max_display_messages(mut self, max: usize) -> Self {
        self.max_display_messages = max;
        self
    }

    /// Overrides [`Self::max_authorization_cache_entries`].
    pub const fn with_max_authorization_cache_entries(mut self, max: usize) -> Self {
        self.max_authorization_cache_entries = max;
        self
    }

    /// Overrides [`Self::max_charging_profiles`].
    pub const fn with_max_charging_profiles(mut self, max: usize) -> Self {
        self.max_charging_profiles = max;
        self
    }

    /// Overrides [`Self::max_local_authorization_list_entries`].
    pub const fn with_max_local_authorization_list_entries(mut self, max: usize) -> Self {
        self.max_local_authorization_list_entries = max;
        self
    }

    /// Overrides [`Self::max_device_model_variables`].
    pub const fn with_max_device_model_variables(mut self, max: usize) -> Self {
        self.max_device_model_variables = max;
        self
    }
}

impl Default for StateLimits {
    fn default() -> Self {
        Self::new()
    }
}
