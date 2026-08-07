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
}

impl StateLimits {
    /// This crate's documented default maxima - the same values [`Default`] yields, in a `const`
    /// context.
    pub const fn new() -> Self {
        Self {
            max_local_authorization_list_entries: DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES,
            max_device_model_variables: DEFAULT_MAX_DEVICE_MODEL_VARIABLES,
        }
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
