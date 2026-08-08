//! Measures the heap a charge point retains at its configured bounds, per configuration - the
//! measurement half of G2.3 (`docs/PRODUCTION-ROADMAP.md` §9.2). `docs/MEMORY.md` is written from
//! this test's output; re-run it with
//!
//! ```text
//! cargo test --test memory_budget -- --nocapture
//! ```
//!
//! to regenerate the table after changing anything that owns memory.
//!
//! # Why an integration test rather than a unit test
//!
//! The numbers come from a counting [`GlobalAlloc`] wrapper, which sees *every* allocation in its
//! binary. In the lib test binary that would mean 500-odd tests allocating concurrently on the same
//! counter, so a reading taken around one structure would include whatever the rest of the suite
//! happened to be doing. An integration test gets its own binary; this file deliberately contains a
//! single `#[test]` so the counter is never shared, and every measurement is taken on the test's own
//! thread with nothing else running.
//!
//! # What is counted, and what is not
//!
//! Counted: the exact number of bytes the crate *requests* from the allocator and has not freed,
//! while the measured structure is alive - `Vec`/`BTreeMap`/`String` backing storage, i.e. the part
//! that scales with the configured bounds. Allocator bookkeeping (per-chunk headers, size-class
//! rounding, fragmentation) is *not* counted, because it belongs to the integrator's allocator, not
//! to this crate; budget headroom for it.
//!
//! Also not counted, and out of scope for this test on purpose: task stacks (the integrator's
//! `Executor` owns those), the transport's buffers and TLS state (`ocpp-client`'s, and any
//! transport it dials), and the transient allocations wire (de)serialization makes per message -
//! those peak with the largest single message, not with the retained state, and bounding them is
//! F5.2's job (`docs/PRODUCTION-ROADMAP.md` §8.5).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ocpp_charge_point::offline_queue::OfflineQueue;
use ocpp_charge_point::security::{SecurityEventLog, SecurityLogEntry};
use ocpp_charge_point::state::{
    AuthorizationStatus, ChargePointEvent, ChargingProfile, ChargingProfileId, ChargingProfileKind,
    ChargingProfilePurpose, ChargingProfileScope, ChargingRateUnit, ChargingSchedule,
    ChargingSchedulePeriod, Component, ConnectorState, ConnectorStatus, ConnectorStatusChanged,
    DeviceModelEvent, IdToken, IdTokenKind, LocalListEntry, MeterSample, Reservation,
    ReservationId, SecurityEvent, SecurityEventType, StateLimits, Transaction,
    TransactionChargingState, TransactionEventKind, TransactionEventOccurred, TransactionId,
    Variable, VariableAttribute, VariableAttributeType, VariableCharacteristics, VariableDataType,
    VariableMutability,
};
use ocpp_charge_point::state::{ChargePointState, DeviceModel, EvseState, VariableDefinition};

/// Live (allocated minus freed) bytes requested through [`Counting`].
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// A [`GlobalAlloc`] that forwards to the system allocator while tracking live requested bytes.
/// `realloc`/`alloc_zeroed` are deliberately left to the trait's default implementations, which
/// route back through this type's own `alloc`/`dealloc` and are therefore counted too.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Builds a value and reports how many bytes of heap it still holds, keeping it alive across the
/// second reading so nothing it owns has been dropped yet. The value is returned rather than
/// dropped inside, so a caller measuring a structure in stages doesn't pay for rebuilding it.
fn retained<T>(build: impl FnOnce() -> T) -> (usize, T) {
    let before = LIVE.load(Ordering::Relaxed);
    let value = build();
    let after = LIVE.load(Ordering::Relaxed);
    // `after < before` would mean the closure freed more than it allocated, which for a
    // build-something closure means the reading is meaningless rather than negative.
    (after.saturating_sub(before), value)
}

/// A charge point configuration to measure: its topology, its [`StateLimits`], and its offline
/// queue capacity. One row of `docs/MEMORY.md`'s table.
struct Configuration {
    name: &'static str,
    /// How many device model variables share each component - see [`fill_device_model`]. 8 is
    /// representative of OCPP's own standardized model.
    variables_per_component: usize,
    /// Connectors per EVSE, exactly as [`ChargePointState::with_limits`] takes them.
    connector_counts: &'static [usize],
    limits: StateLimits,
    /// The capacity each of the three offline queues (status, transaction, security) is configured
    /// with - `crate::offline_queue::DEFAULT_CAPACITY` (100) unless the integrator picks otherwise.
    queue_capacity: usize,
    /// How many schedule periods each installed charging profile carries. Eight covers a
    /// day-shaped tariff (overnight cheap, morning peak, midday, evening peak, ...) without
    /// pretending a CSMS sends one period per minute.
    periods_per_profile: usize,
    /// The capacity the durable security log (E2.10) is configured with -
    /// `crate::security::DEFAULT_SECURITY_LOG_CAPACITY` (50) unless the integrator picks
    /// otherwise. Sized independently of `queue_capacity`: the log retains history whether or not
    /// it was ever delivered, so its bound answers "how much history do we keep", not "how long an
    /// outage do we absorb".
    security_log_capacity: usize,
}

/// The id token length this test fills with: OCPP 2.x's `IdTokenType.idToken` is a 36-character
/// `identifierString`, so a list full of maximum-length tokens is the honest worst case. A charge
/// point seeing only 1.6J's 20-character `idTag`s, or shorter RFID uids, holds correspondingly less
/// - each entry's heap scales with its token's length.
const ID_TOKEN_LEN: usize = 36;

/// Component and variable name lengths to fill the device model with. OCPP's standardized names
/// run to roughly this length (`"SampledDataCtrlr"`, `"TxUpdatedMeasurands"`), and this crate
/// stores them as-is.
const DEVICE_MODEL_NAME_LEN: usize = 20;

/// The attribute value length to fill the device model with - long enough to cover the
/// member-list-shaped variables (a comma-separated measurand list) that dominate the real model.
const ATTRIBUTE_VALUE_LEN: usize = 48;

fn id_token(index: usize) -> IdToken {
    let mut value = format!("{index:0ID_TOKEN_LEN$}");
    value.truncate(ID_TOKEN_LEN);
    IdToken {
        value,
        kind: IdTokenKind::ISO14443,
    }
}

fn filler(prefix: &str, index: usize, len: usize) -> String {
    let mut value = format!("{prefix}{index}");
    while value.len() < len {
        value.push('x');
    }
    value.truncate(len);
    value
}

/// A worst-case local authorization list: every entry a maximum-length id token.
fn full_local_list(entries: usize) -> Vec<LocalListEntry> {
    (0..entries)
        .map(|index| LocalListEntry {
            id_token: id_token(index),
            status: AuthorizationStatus::Accepted,
        })
        .collect()
}

/// A worst-case active transaction: an id token plus a meter sample with every measurand present.
fn full_transaction(index: usize) -> Transaction {
    Transaction {
        id: TransactionId(index as u64),
        id_token: Some(id_token(index)),
        charging_state: TransactionChargingState::Charging,
        stop_reason: None,
        seq_no: 42,
        last_meter_sample: Some(MeterSample {
            energy_wh: 12_345,
            power_w: Some(7_400),
            current_ma: Some(32_000),
            voltage_v: Some(230),
            soc_percent: Some(64),
        }),
        priority_charging: true,
    }
}

/// Registers `variables` device model variables through the documented event path, spread
/// `per_component` variables to a component, so what's measured is what a hardware binding's
/// registrations actually cost. Returns how many the model accepted - fewer than asked for means
/// the configured maximum refused the rest, which is G2.2's bound doing its job.
///
/// `per_component` matters far more than it looks: the model is a `BTreeMap<Component,
/// BTreeMap<Variable, VariableDefinition>>`, and a `BTreeMap` node is allocated at its full
/// branching factor however few entries it holds. One variable per component therefore pays for a
/// whole inner node per variable, while OCPP's own model clusters many variables onto each
/// standardized `*Ctrlr` component and amortizes that node across all of them. See
/// `device_model_shape_comparison`, and the note in `docs/MEMORY.md`.
fn fill_device_model(
    state: &mut ChargePointState,
    variables: usize,
    per_component: usize,
) -> usize {
    for index in 0..variables {
        state.apply(ChargePointEvent::DeviceModel(
            DeviceModelEvent::VariableRegistered {
                component: Component {
                    name: filler("Ctrlr", index / per_component.max(1), DEVICE_MODEL_NAME_LEN),
                    instance: None,
                    evse: None,
                },
                variable: Variable {
                    name: filler("Var", index, DEVICE_MODEL_NAME_LEN),
                    instance: None,
                },
                characteristics: VariableCharacteristics {
                    data_type: VariableDataType::String,
                    unit: None,
                    min_limit: None,
                    max_limit: None,
                    values_list: None,
                    supports_monitoring: false,
                },
                attributes: vec![VariableAttribute {
                    attribute_type: VariableAttributeType::Actual,
                    value: filler("value", index, ATTRIBUTE_VALUE_LEN),
                    mutability: VariableMutability::ReadWrite,
                    persistent: true,
                    constant: false,
                    requires_reboot: false,
                }],
            },
        ));
    }
    state.device_model.len()
}

/// Fills every connector with an active transaction and a held reservation - the state a fully busy
/// charge point holds. Constructed directly rather than driven through the connector state machine:
/// this measures what the collections cost when occupied, and a connector can't hold both a
/// transaction and a reservation through the live path (which is exactly why measuring the
/// worst case needs the fields set directly).
fn fill_connectors(state: &mut ChargePointState) {
    for (evse_id, evse) in state.evses.iter_mut().enumerate() {
        for connector_id in 0..evse.connectors.len() {
            let index = evse_id * 100 + connector_id;
            evse.transactions[connector_id] = Some(full_transaction(index));
            evse.reservations[connector_id] = Some(Reservation {
                id: ReservationId(index as i64),
                id_token: id_token(index),
                expires_at: None,
            });
            evse.running_costs[connector_id] = Some(12.75);
        }
    }
}

/// A worst-case status queue backlog: `capacity` status changes.
fn full_status_queue(capacity: usize) -> OfflineQueue<ConnectorStatusChanged> {
    let queue = OfflineQueue::with_capacity(capacity);
    queue.restore_backlog(
        (0..capacity)
            .map(|index| ConnectorStatusChanged {
                evse_id: 0,
                connector_id: index % 2,
                status: ConnectorStatus::Occupied,
                connector_state: ConnectorState::Charging,
            })
            .collect(),
    );
    queue
}

/// A worst-case transaction queue backlog: `capacity` transaction events, each carrying an id token
/// and a full meter sample.
fn full_transaction_queue(capacity: usize) -> OfflineQueue<TransactionEventOccurred> {
    let queue = OfflineQueue::with_capacity(capacity);
    queue.restore_backlog(
        (0..capacity)
            .map(|index| TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Updated(
                    ocpp_charge_point::state::TransactionUpdateReason::MeterValuePeriodic,
                ),
                transaction: full_transaction(index),
            })
            .collect(),
    );
    queue
}

/// A worst-case security queue backlog: `capacity` events, each carrying `techInfo` text. The
/// `MemoryExhaustion` events G2.1/G2.2 raise carry a sentence of detail, so an empty `tech_info`
/// would understate the queue that is most likely to be full.
fn full_security_queue(capacity: usize) -> OfflineQueue<SecurityEvent> {
    let queue = OfflineQueue::with_capacity(capacity);
    queue.restore_backlog(
        (0..capacity)
            .map(|index| SecurityEvent {
                event_type: SecurityEventType::MemoryExhaustion,
                tech_info: Some(filler("an offline-report queue overflowed: ", index, 80)),
            })
            .collect(),
    );
    queue
}

/// Fills the charging profile store to its configured bound (B2.1), each profile carrying
/// `periods_per_profile` schedule periods - the worst case a CSMS can drive the store to.
fn fill_charging_profiles(
    state: &mut ChargePointState,
    profiles: usize,
    periods_per_profile: usize,
) -> usize {
    for index in 0..profiles {
        state.apply(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::Evse(0),
            profile: Box::new(ChargingProfile {
                id: ChargingProfileId(index as i32),
                stack_level: index as u32,
                purpose: ChargingProfilePurpose::TxDefault,
                kind: ChargingProfileKind::Absolute,
                recurrency: None,
                valid_from: None,
                valid_to: None,
                transaction_id: None,
                schedules: vec![ChargingSchedule {
                    id: index as i32,
                    start_schedule: None,
                    duration_secs: Some(86_400),
                    rate_unit: ChargingRateUnit::Amps,
                    min_charging_rate: Some(6.0),
                    periods: (0..periods_per_profile)
                        .map(|period| ChargingSchedulePeriod {
                            start_period_secs: (period * 3_600) as u32,
                            limit: 16.0,
                            number_phases: Some(3),
                        })
                        .collect(),
                }],
                // A scheduled profile, so neither dynamic field applies - and the store would
                // refuse the profile outright if `dyn_update_interval_secs` were set here.
                dyn_update_interval_secs: None,
                dyn_update_time: None,
            }),
        });
    }
    state.charging_profiles.len()
}

/// A worst-case security log: `capacity` recorded entries, each carrying `techInfo` text and a
/// timestamp - the same filling as [`full_security_queue`], since the log records exactly the
/// events that queue forwards.
fn full_security_log(capacity: usize) -> SecurityEventLog {
    let log = SecurityEventLog::with_capacity(capacity);
    log.restore(
        (0..capacity)
            .map(|index| SecurityLogEntry {
                event: SecurityEvent {
                    event_type: SecurityEventType::MemoryExhaustion,
                    tech_info: Some(filler("an offline-report queue overflowed: ", index, 80)),
                },
                recorded_at: chrono::DateTime::from_timestamp(1_800_000_000 + index as i64, 0),
            })
            .collect(),
    );
    log
}

/// One configuration's measured retained heap, in bytes, broken down by what holds it.
struct Measurement {
    empty_state: usize,
    local_list: usize,
    device_model: usize,
    device_model_variables: usize,
    connectors: usize,
    charging_profiles: usize,
    installed_profiles: usize,
    status_queue: usize,
    transaction_queue: usize,
    security_queue: usize,
    security_log: usize,
}

impl Measurement {
    fn state_total(&self) -> usize {
        self.empty_state
            + self.local_list
            + self.device_model
            + self.connectors
            + self.charging_profiles
    }

    fn queue_total(&self) -> usize {
        self.status_queue + self.transaction_queue + self.security_queue
    }

    fn total(&self) -> usize {
        self.state_total() + self.queue_total() + self.security_log
    }
}

fn measure(configuration: &Configuration) -> Measurement {
    let (empty_state, mut state) = retained(|| {
        ChargePointState::with_limits(
            configuration.connector_counts.iter().copied(),
            configuration.limits,
        )
    });

    let (local_list, _) = retained(|| {
        state.apply(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: full_local_list(configuration.limits.max_local_authorization_list_entries),
        })
    });

    let mut device_model_variables = 0;
    let (device_model, _) = retained(|| {
        device_model_variables = fill_device_model(
            &mut state,
            configuration.limits.max_device_model_variables,
            configuration.variables_per_component,
        );
    });

    let (connectors, _) = retained(|| fill_connectors(&mut state));

    let mut installed_profiles = 0;
    let (charging_profiles, _) = retained(|| {
        installed_profiles = fill_charging_profiles(
            &mut state,
            configuration.limits.max_charging_profiles,
            configuration.periods_per_profile,
        );
    });

    let (status_queue, status) = retained(|| full_status_queue(configuration.queue_capacity));
    let (transaction_queue, transactions) =
        retained(|| full_transaction_queue(configuration.queue_capacity));
    let (security_queue, security) = retained(|| full_security_queue(configuration.queue_capacity));
    let (security_log, log) = retained(|| full_security_log(configuration.security_log_capacity));

    // Nothing measured may be dropped before the last reading is taken.
    drop((state, status, transactions, security, log));

    Measurement {
        empty_state,
        local_list,
        device_model,
        device_model_variables,
        connectors,
        charging_profiles,
        installed_profiles,
        status_queue,
        transaction_queue,
        security_queue,
        security_log,
    }
}

/// The configurations `docs/MEMORY.md` tabulates - a tight single-connector AC wallbox, this
/// crate's defaults, and a large DC site.
fn configurations() -> Vec<Configuration> {
    vec![
        Configuration {
            name: "Tight AC wallbox (1 EVSE x 1 connector)",
            variables_per_component: 8,
            connector_counts: &[1],
            limits: StateLimits::default()
                .with_max_local_authorization_list_entries(25)
                .with_max_device_model_variables(64),
            queue_capacity: 25,
            periods_per_profile: 8,
            security_log_capacity: 25,
        },
        Configuration {
            name: "Crate defaults (1 EVSE x 2 connectors)",
            variables_per_component: 8,
            connector_counts: &[2],
            limits: StateLimits::default(),
            queue_capacity: 100,
            periods_per_profile: 8,
            security_log_capacity: ocpp_charge_point::security::DEFAULT_SECURITY_LOG_CAPACITY,
        },
        Configuration {
            name: "DC site (4 EVSEs x 2 connectors)",
            variables_per_component: 8,
            connector_counts: &[2, 2, 2, 2],
            limits: StateLimits::default()
                .with_max_local_authorization_list_entries(500)
                .with_max_device_model_variables(512),
            queue_capacity: 200,
            periods_per_profile: 8,
            security_log_capacity: 200,
        },
    ]
}

/// Measures what the same 256-variable device model costs at different clusterings, and returns the
/// per-variable cost for each. The spread is large enough that an integrator registering one
/// variable per component would blow a budget sized from the clustered figure, which is exactly why
/// `docs/MEMORY.md` quotes both.
fn device_model_shape_comparison(variables: usize, shapes: &[usize]) -> Vec<(usize, usize)> {
    shapes
        .iter()
        .map(|&per_component| {
            let mut state = ChargePointState::with_limits(
                [1],
                StateLimits::default().with_max_device_model_variables(variables),
            );
            let (bytes, _) = retained(|| fill_device_model(&mut state, variables, per_component));
            drop(state);
            (per_component, bytes)
        })
        .collect()
}

/// The ceiling each configuration's total retained heap must stay under, in bytes. Set roughly 25%
/// above the measured figure, so ordinary drift doesn't fail the build but a change that
/// meaningfully grows retained state does - the point of measuring at all (G2.3). Raise a ceiling
/// only together with `docs/MEMORY.md`'s table.
const CEILINGS: [usize; 3] = [79_000, 230_000, 505_000];

#[test]
fn retained_heap_per_configuration_stays_within_its_documented_budget() {
    println!("\nsize_of (this host, {}-bit pointers):", usize::BITS);
    println!(
        "  ChargePointState      {:>5} B",
        size_of::<ChargePointState>()
    );
    println!("  DeviceModel           {:>5} B", size_of::<DeviceModel>());
    println!("  EvseState             {:>5} B", size_of::<EvseState>());
    println!("  Transaction           {:>5} B", size_of::<Transaction>());
    println!("  Reservation           {:>5} B", size_of::<Reservation>());
    println!(
        "  LocalListEntry        {:>5} B",
        size_of::<LocalListEntry>()
    );
    println!("  Component             {:>5} B", size_of::<Component>());
    println!("  Variable              {:>5} B", size_of::<Variable>());
    println!(
        "  VariableDefinition    {:>5} B",
        size_of::<VariableDefinition>()
    );
    println!(
        "  ConnectorStatusChanged{:>5} B",
        size_of::<ConnectorStatusChanged>()
    );
    println!(
        "  TransactionEventOccurred {:>2} B",
        size_of::<TransactionEventOccurred>()
    );
    println!(
        "  SecurityEvent         {:>5} B",
        size_of::<SecurityEvent>()
    );

    println!("\nDevice model shape: 256 variables, clustered N to a component");
    for (per_component, bytes) in device_model_shape_comparison(256, &[1, 4, 8, 16]) {
        println!(
            "  {per_component:>2} variables/component  {bytes:>7} B  ({} B per variable)",
            bytes / 256
        );
    }

    for (configuration, ceiling) in configurations().iter().zip(CEILINGS) {
        let measured = measure(configuration);

        println!("\n{}", configuration.name);
        println!("  empty state                {:>7} B", measured.empty_state);
        println!(
            "  local auth list (max {:>3})  {:>7} B",
            configuration.limits.max_local_authorization_list_entries, measured.local_list
        );
        println!(
            "  device model ({:>3} vars)     {:>7} B",
            measured.device_model_variables, measured.device_model
        );
        println!("  busy connectors            {:>7} B", measured.connectors);
        println!(
            "  charging profiles ({:>2})     {:>7} B",
            measured.installed_profiles, measured.charging_profiles
        );
        println!(
            "  status queue               {:>7} B",
            measured.status_queue
        );
        println!(
            "  transaction queue          {:>7} B",
            measured.transaction_queue
        );
        println!(
            "  security queue             {:>7} B",
            measured.security_queue
        );
        println!(
            "  security log ({:>3})         {:>7} B",
            configuration.security_log_capacity, measured.security_log
        );
        println!(
            "  state subtotal             {:>7} B",
            measured.state_total()
        );
        println!(
            "  queue subtotal             {:>7} B",
            measured.queue_total()
        );
        println!(
            "  TOTAL                      {:>7} B  (ceiling {} B)",
            measured.total(),
            ceiling
        );

        assert!(
            measured.total() <= ceiling,
            "{} retains {} B at its configured bounds, over its {} B ceiling - if this growth is \
             intended, re-measure and update both `CEILINGS` here and the table in docs/MEMORY.md",
            configuration.name,
            measured.total(),
            ceiling
        );

        // The profile store's bound must bind too (G2.2), for the same reason.
        assert_eq!(
            measured.installed_profiles, configuration.limits.max_charging_profiles,
            "the charging profile store should be full at its configured maximum"
        );

        // Every bound must actually bind: the device model must have accepted exactly its
        // configured maximum (G2.2), or the figure above isn't a worst case at all.
        assert_eq!(
            measured.device_model_variables, configuration.limits.max_device_model_variables,
            "the device model should be full at its configured maximum"
        );
    }
}
