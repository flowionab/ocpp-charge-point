//! Static description of what a piece of hardware can do, so the crate can adapt its behaviour
//! (and, eventually, its protocol advertisements - see `docs/PRODUCTION-ROADMAP.md` §5.3's C3)
//! to the real machine rather than assuming every optional OCPP functional block is present.

/// The level of ISO 15118 support a charge point's hardware provides, from no support at all up
/// to the newest (`-20`) generation of the standard. `None` is the conservative
/// [`Default`](Capabilities::default) - hardware must opt into a higher level explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Iso15118SupportLevel {
    /// No ISO 15118 support - only "plug and charge" via a physically presented token
    /// (RFID, app, etc.) is available.
    #[default]
    None,
    /// ISO 15118-2 (Plug & Charge / High Level Communication over the original standard).
    Iso15118_2,
    /// ISO 15118-20 (the current generation, adding e.g. bidirectional power transfer
    /// negotiation and wireless charging support at the protocol level).
    Iso15118_20,
}

/// What a piece of hardware can do: the fixed facts about a charge point's physical and
/// electrical capabilities that this crate's behaviour - and, eventually, its OCPP
/// advertisements (device model, `SupportedFeatureProfiles`, `*Ctrlr.Available` variables - see
/// `docs/PRODUCTION-ROADMAP.md` §5.3) - should be derived from, rather than assuming every
/// optional functional block is present.
///
/// Returned by [`ChargePoint::capabilities`](crate::hardware::ChargePoint::capabilities).
/// `no_std`-friendly: every field is a `bool`, a small `Copy` enum, or a small numeric type -
/// nothing here allocates.
///
/// `#[non_exhaustive]` and equipped with a conservative [`Default`] (see its docs) so this
/// struct can grow new fields later without that being a breaking change for integrators who
/// construct it with `..Default::default()`, as recommended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// The hardware has a user-facing display (OCPP `DisplayMessage` functional block, plus
    /// `ClearedChargingLimit`/message-related UX in general).
    pub has_display: bool,
    /// The hardware can both import and export power at a connector (vehicle-to-grid /
    /// vehicle-to-home), not just import it.
    pub supports_bidirectional_power: bool,
    /// The connector lock can be safely released while the contactor is still closed and
    /// current is flowing, rather than requiring the contactor to open first. Charge points
    /// without this must always stop energy flow before honouring an unlock.
    pub can_unlock_under_load: bool,
    /// The hardware has a real-time clock that keeps time across power loss, so timestamps
    /// are trustworthy immediately after boot rather than only after the CSMS has supplied
    /// `SetSystemTime`/`Heartbeat`-driven correction.
    pub has_rtc: bool,
    /// The hardware exposes non-volatile storage that survives a power cycle (see
    /// [`crate::hardware::Storage`]). A charge point without persistent storage still runs -
    /// state that would otherwise survive a restart (offline transaction queue, cached
    /// configuration, etc.) does not.
    pub has_persistent_storage: bool,
    /// The level of ISO 15118 (Plug & Charge) support the hardware provides.
    pub iso15118_support: Iso15118SupportLevel,
    /// The maximum current, in amps, any single connector can be asked to deliver. `None` means
    /// unknown/unbounded - the hardware binding is trusted to reject or clamp out-of-range
    /// requests itself in that case.
    pub max_current_per_connector_amps: Option<u16>,
    /// The hardware/integration supports reservations (OCPP `ReserveNow`/`CancelReservation`).
    pub reservation: bool,
    /// The hardware/integration supports the local authorization list functional block.
    pub local_auth_list: bool,
    /// The hardware supports smart charging (charging profiles / current or power limiting -
    /// see [`crate::hardware::Connector::set_current_limit`]).
    pub smart_charging: bool,
    /// The hardware supports firmware management (`UpdateFirmware` and related messages).
    pub firmware_management: bool,
    /// The charge point acts as an OCPP local controller and can serve a downloaded firmware
    /// image to other charge points on the local network (`PublishFirmware`/`UnpublishFirmware`/
    /// `PublishFirmwareStatusNotification` - `docs/PRODUCTION-ROADMAP.md` B3.4). Needs a
    /// [`crate::hardware::FirmwarePublisher`], which the overwhelming majority of charge points -
    /// anything that isn't itself acting as a local controller - do not have, so this defaults to
    /// absent like every other capability.
    pub firmware_publishing: bool,
    /// The hardware supports the diagnostics functional block (log upload, etc.).
    pub diagnostics: bool,
    /// The charge point can hold and manage X.509 certificates - see
    /// [`crate::hardware::CertificateStore`]. Needed for `InstallCertificate`/`DeleteCertificate`/
    /// `GetInstalledCertificateIds`, and for security profiles 2 and 3 to have a trust chain to
    /// verify against.
    pub certificate_management: bool,
    /// The hardware supports variable monitoring (thresholds/periodic monitors on device model
    /// variables).
    pub variable_monitoring: bool,
    /// The hardware/integration supports tariff and cost reporting (`CostUpdated`, running
    /// total cost, display of tariff information).
    pub tariff_and_cost: bool,
    /// The hardware supports OCPP-integrated payment (e.g. contactless payment terminals).
    pub payment: bool,
    /// The hardware supports DER (Distributed Energy Resource) control.
    pub der_control: bool,
    /// The hardware supports battery swap.
    pub battery_swap: bool,
    /// The hardware/integration supports periodic event streaming (2.1 `NotifyPeriodicEventStream`).
    pub periodic_event_stream: bool,
    /// The hardware/integration supports certificate management (install/delete certificates,
    /// signing, ISO 15118 certificate exchange).
    pub certificates: bool,
}

impl Default for Capabilities {
    /// Conservative by construction: every capability defaults to absent (`false`/`None`), so
    /// declaring one requires opting in explicitly rather than accidentally advertising
    /// something the hardware doesn't actually support. Integrators should construct this with
    /// struct-update syntax - `Capabilities { has_display: true, ..Default::default() }` - so
    /// that a future new field defaults safely rather than requiring every call site to be
    /// updated.
    fn default() -> Self {
        Self {
            has_display: false,
            supports_bidirectional_power: false,
            can_unlock_under_load: false,
            has_rtc: false,
            has_persistent_storage: false,
            iso15118_support: Iso15118SupportLevel::None,
            max_current_per_connector_amps: None,
            reservation: false,
            local_auth_list: false,
            smart_charging: false,
            firmware_management: false,
            firmware_publishing: false,
            diagnostics: false,
            certificate_management: false,
            variable_monitoring: false,
            tariff_and_cost: false,
            payment: false,
            der_control: false,
            battery_swap: false,
            periodic_event_stream: false,
            certificates: false,
        }
    }
}

/// One row of the single source of truth mapping a boolean [`Capabilities`] field to the Cargo
/// feature that should gate it, and to the OCPP-visible names every advertisement surface derives
/// its answer from - see `docs/PRODUCTION-ROADMAP.md` §5.3 (C3). Every surface (handler
/// registration in [`crate::setup::setup`]/[`crate::connect::connect_and_setup`], the 2.x device
/// model's `*Ctrlr.Available` variables, and 1.6J's `SupportedFeatureProfiles`) reads this table
/// rather than re-deriving the mapping itself, so a capability added to one surface can't be
/// forgotten in another.
///
/// Deliberately limited to the capabilities that map onto a real, spec-named OCPP surface today.
/// Blocks with no implementation yet (payment, DER control, battery swap, ...) aren't listed -
/// there's nothing for them to be inconsistent about yet; extend this table as they grow one.
pub struct CapabilityGate {
    /// A short, stable, snake_case identifier for the capability (matches the [`Capabilities`]
    /// field name).
    pub name: &'static str,
    /// The Cargo feature that must be enabled for this capability to be usable at all.
    pub cargo_feature: &'static str,
    /// Reads the capability's current value out of a [`Capabilities`] instance.
    pub enabled: fn(&Capabilities) -> bool,
    /// The OCPP 2.x device model component whose `Available` variable should mirror this
    /// capability, if the spec defines one (see `docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv`).
    /// `None` for capabilities with no dedicated `*Ctrlr` component in the 2.1 appendix (e.g.
    /// firmware management, which the spec models via plain `ChargingStation`/generic variables
    /// rather than a `FirmwareUpdateCtrlr` component).
    pub ctrlr_component: Option<&'static str>,
    /// The 1.6J `SupportedFeatureProfiles` name this capability corresponds to, if any. `None` for
    /// capabilities 1.6J has no profile for at all (tariff/cost, display messages).
    pub feature_profile_1_6: Option<&'static str>,
    /// Whether this capability currently gates a real handler registration in
    /// [`crate::builder::ChargePointBuilder`]/[`crate::setup::setup`] - `false` for capabilities
    /// that are declared and advertised but have no message handling implemented yet.
    pub has_handler: bool,
}

/// The single source of truth for capability propagation (`docs/PRODUCTION-ROADMAP.md` §5.3,
/// C3) - see [`CapabilityGate`]'s docs. Component names are sourced from
/// `docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv`; 1.6J profile names from OCPP 1.6J
/// Appendix - "Configuration Key Names" / `SupportedFeatureProfiles`' standard values (Core,
/// FirmwareManagement, LocalAuthListManagement, Reservation, SmartCharging, RemoteTrigger).
pub const CAPABILITY_GATES: &[CapabilityGate] = &[
    CapabilityGate {
        name: "reservation",
        cargo_feature: "reservation",
        enabled: |c| c.reservation,
        ctrlr_component: Some("ReservationCtrlr"),
        feature_profile_1_6: Some("Reservation"),
        has_handler: true,
    },
    CapabilityGate {
        name: "local_auth_list",
        cargo_feature: "local-auth-list",
        enabled: |c| c.local_auth_list,
        ctrlr_component: Some("LocalAuthListCtrlr"),
        feature_profile_1_6: Some("LocalAuthListManagement"),
        has_handler: true,
    },
    CapabilityGate {
        name: "tariff_and_cost",
        cargo_feature: "tariff-cost",
        enabled: |c| c.tariff_and_cost,
        ctrlr_component: Some("TariffCostCtrlr"),
        feature_profile_1_6: None,
        has_handler: true,
    },
    CapabilityGate {
        name: "smart_charging",
        cargo_feature: "smart-charging",
        enabled: |c| c.smart_charging,
        ctrlr_component: Some("SmartChargingCtrlr"),
        feature_profile_1_6: Some("SmartCharging"),
        has_handler: true,
    },
    CapabilityGate {
        name: "has_display",
        cargo_feature: "display-message",
        enabled: |c| c.has_display,
        ctrlr_component: Some("DisplayMessageCtrlr"),
        feature_profile_1_6: None,
        has_handler: false,
    },
    CapabilityGate {
        name: "certificate_management",
        cargo_feature: "certificate-management",
        enabled: |c| c.certificate_management,
        // No dedicated `*Ctrlr` component: OCPP puts certificate counts on `SecurityCtrlr`, which
        // exists whether or not a store does.
        ctrlr_component: None,
        // 1.6J's certificate messages live in the Security Whitepaper, not in a core feature
        // profile, so there is nothing to advertise there.
        feature_profile_1_6: None,
        // Builder-only, like diagnostics and firmware: registering these needs a
        // `hardware::CertificateStore`, which `setup()`'s signature cannot receive.
        has_handler: false,
    },
    CapabilityGate {
        name: "diagnostics",
        cargo_feature: "diagnostics",
        enabled: |c| c.diagnostics,
        // No dedicated `*Ctrlr` component for diagnostics in the 2.1 appendix.
        ctrlr_component: None,
        feature_profile_1_6: Some("FirmwareManagement"),
        // `false` even though B5.1 landed a real `GetLog` handler, because this field means
        // specifically "`setup()` registers it". It cannot: registering log upload needs a
        // `hardware::FileTransfer` binding, and `setup()`'s signature has no way to receive one -
        // exactly the position `Storage` is in, where durability is also
        // `ChargePointBuilder`-only (see E1/M2's "durability is opt-in per concern"). An
        // integrator wanting log upload calls `ChargePointBuilder::log_uploads`.
        has_handler: false,
    },
    CapabilityGate {
        name: "firmware_management",
        cargo_feature: "firmware-management",
        enabled: |c| c.firmware_management,
        // No dedicated `*Ctrlr` component for firmware management in the 2.1 appendix - see
        // `ctrlr_component`'s docs.
        ctrlr_component: None,
        feature_profile_1_6: Some("FirmwareManagement"),
        has_handler: false,
    },
    CapabilityGate {
        name: "firmware_publishing",
        cargo_feature: "firmware-publishing",
        enabled: |c| c.firmware_publishing,
        // The 2.1 appendix names a `LocalController` component (`components.csv`) but defines no
        // variables for it in `dm_components_vars.csv` - nothing there to mirror an `Available`
        // flag onto, so this is `None` for the same reason `firmware_management` is.
        ctrlr_component: None,
        // 1.6J predates the local-controller concept entirely - no feature profile covers it.
        feature_profile_1_6: None,
        // Builder-only, like `firmware_management`/`diagnostics`/`certificate_management`:
        // registering it needs a `hardware::FirmwarePublisher` (on top of the
        // `hardware::FileTransfer` `firmware_management` already needs), which `setup()`'s
        // signature cannot receive. An integrator wanting it calls
        // `ChargePointBuilder::publish_firmware`.
        has_handler: false,
    },
    CapabilityGate {
        name: "periodic_event_stream",
        cargo_feature: "periodic-event-stream",
        enabled: |c| c.periodic_event_stream,
        // No dedicated `*Ctrlr` component for periodic event streams in the 2.1 appendix - see
        // `ctrlr_component`'s docs; the closest thing, `MonitoringCtrlr`, already has its own
        // `Available` variable governing variable monitoring itself, and OCPP does not name a
        // separate one for streams built on top of it.
        ctrlr_component: None,
        // 2.1-only - neither 1.6J nor 2.0.1 has a periodic event stream concept, so there is no
        // feature profile to advertise.
        feature_profile_1_6: None,
        has_handler: true,
    },
    CapabilityGate {
        name: "battery_swap",
        cargo_feature: "battery-swap",
        enabled: |c| c.battery_swap,
        // No vendored OCPP 2.1 device-model appendix in this checkout to confirm a dedicated
        // `*Ctrlr` component name for battery swap against - recorded as `None` rather than
        // guessing at a name this crate can't verify (see `ctrlr_component`'s docs).
        ctrlr_component: None,
        // 2.1-only - no battery-swap messages exist before 2.1, so 1.6J has no profile for it.
        feature_profile_1_6: None,
        // Builder-only, like `certificate_management`/`diagnostics`/`firmware_management`: needs
        // a `hardware::BatterySwapStation`, which `setup()`'s signature cannot receive.
        has_handler: false,
    },
];

/// One `(capability name, Cargo feature name, whether the capability is set)` row consulted by
/// [`warn_on_feature_mismatches`] - covers every boolean/enum capability, not just the ones in
/// [`CAPABILITY_GATES`] that map onto a named OCPP surface (C2.4 is about catching integrator
/// mistakes early, so it deliberately checks more than C3 needs to propagate).
fn all_capability_feature_pairs(
    capabilities: &Capabilities,
) -> [(&'static str, &'static str, bool); 16] {
    [
        (
            "smart_charging",
            "smart-charging",
            capabilities.smart_charging,
        ),
        (
            "firmware_management",
            "firmware-management",
            capabilities.firmware_management,
        ),
        (
            "firmware_publishing",
            "firmware-publishing",
            capabilities.firmware_publishing,
        ),
        ("diagnostics", "diagnostics", capabilities.diagnostics),
        (
            "certificate_management",
            "certificate-management",
            capabilities.certificate_management,
        ),
        (
            "variable_monitoring",
            "variable-monitoring",
            capabilities.variable_monitoring,
        ),
        ("has_display", "display-message", capabilities.has_display),
        ("reservation", "reservation", capabilities.reservation),
        (
            "local_auth_list",
            "local-auth-list",
            capabilities.local_auth_list,
        ),
        (
            "tariff_and_cost",
            "tariff-cost",
            capabilities.tariff_and_cost,
        ),
        ("payment", "payment", capabilities.payment),
        (
            "iso15118_support",
            "iso15118",
            capabilities.iso15118_support != Iso15118SupportLevel::None,
        ),
        ("der_control", "der-control", capabilities.der_control),
        ("battery_swap", "battery-swap", capabilities.battery_swap),
        (
            "periodic_event_stream",
            "periodic-event-stream",
            capabilities.periodic_event_stream,
        ),
        ("certificates", "certificates", capabilities.certificates),
    ]
}

/// Validates `capabilities` (as declared by [`crate::hardware::ChargePoint::capabilities`])
/// against the capability Cargo features actually compiled into this build, and logs loudly
/// (`tracing::warn!`) on every contradiction found in either direction:
///
/// - hardware claims a capability whose feature is compiled out (it will never be advertised or
///   usable no matter what the hardware says), and
/// - a feature is compiled in but hardware declares the capability absent (it will be advertised
///   to the CSMS as unsupported despite flash/binary size having been spent on it).
///
/// Never panics and never fails startup - per `CLAUDE.md`'s error-handling guidance, an
/// integrator misconfiguration like this is contained and surfaced as a log line, not treated as
/// a fatal condition. Called from [`crate::builder::ChargePointBuilder::start`], the first place
/// hardware-declared capabilities are read.
///
/// Deliberately does not raise a `SecurityEvent`/diagnostic: a `SecurityEvent` is for the CSMS's
/// benefit (it's transmitted over the wire), and this is a firmware *build* misconfiguration -
/// the CSMS can't act on it (it never even had the feature to negotiate), and every affected
/// build already logs the contradiction on every boot. Wiring it into `SecurityEventNotifier`
/// would also require a live CSMS connection just to observe a purely local, static fact, and
/// would repeat on every reconnect. A future device-model-visible "was the build consistent
/// checksum" style variable is a more natural fit for a persistent record than a `SecurityEvent`,
/// if that's ever needed.
pub fn warn_on_feature_mismatches(capabilities: &Capabilities) {
    for (name, feature, hardware_claims) in all_capability_feature_pairs(capabilities) {
        let feature_compiled_in = feature_enabled(feature);
        if hardware_claims && !feature_compiled_in {
            tracing::warn!(
                capability = name,
                cargo_feature = feature,
                "hardware declares capability `{name}` present, but the `{feature}` Cargo \
                 feature is disabled - it will never be advertised or reachable in this build"
            );
        } else if !hardware_claims && feature_compiled_in {
            tracing::warn!(
                capability = name,
                cargo_feature = feature,
                "the `{feature}` Cargo feature is enabled but hardware declares capability \
                 `{name}` absent - it will be advertised to the CSMS as unsupported despite \
                 being compiled in"
            );
        }
    }
}

/// Whether `feature` (one of this crate's capability Cargo features) is compiled into this
/// build. A small indirection over `cfg!(feature = ...)` so [`warn_on_feature_mismatches`] can
/// stay data-driven over [`all_capability_feature_pairs`] instead of one `if` per feature.
// Not a `matches!` - each arm is its own `cfg!(feature = ...)` check, expanded to a per-build
// compile-time constant *before* clippy sees this function. Collapsing to `matches!` (as clippy
// suggests for whichever features happen to be enabled in a given build) would silently stop
// tracking `cfg!` altogether and hardcode that one build's feature set.
#[allow(clippy::match_like_matches_macro)]
fn feature_enabled(feature: &str) -> bool {
    match feature {
        "smart-charging" => cfg!(feature = "smart-charging"),
        "firmware-management" => cfg!(feature = "firmware-management"),
        "firmware-publishing" => cfg!(feature = "firmware-publishing"),
        "diagnostics" => cfg!(feature = "diagnostics"),
        "certificate-management" => cfg!(feature = "certificate-management"),
        "variable-monitoring" => cfg!(feature = "variable-monitoring"),
        "display-message" => cfg!(feature = "display-message"),
        "reservation" => cfg!(feature = "reservation"),
        "local-auth-list" => cfg!(feature = "local-auth-list"),
        "tariff-cost" => cfg!(feature = "tariff-cost"),
        "payment" => cfg!(feature = "payment"),
        "iso15118" => cfg!(feature = "iso15118"),
        "der-control" => cfg!(feature = "der-control"),
        "battery-swap" => cfg!(feature = "battery-swap"),
        "periodic-event-stream" => cfg!(feature = "periodic-event-stream"),
        "certificates" => cfg!(feature = "certificates"),
        _ => false,
    }
}

/// Computes the 1.6J `SupportedFeatureProfiles` standard `GetConfiguration` key's value: a
/// comma-separated list of every profile this build/hardware combination genuinely supports.
/// `Core` and `RemoteTrigger` are always present (this crate always implements the Core profile
/// plus remote-trigger-driven reconnection); every other name comes from
/// [`CAPABILITY_GATES`]' `feature_profile_1_6`, included only when both the Cargo feature is
/// compiled in *and* `capabilities` declares the capability present - the same "one source of
/// truth" rule every other C3 surface follows.
pub fn supported_feature_profiles_1_6(capabilities: &Capabilities) -> alloc::string::String {
    use alloc::string::String;

    let mut profiles = String::from("Core,RemoteTrigger");
    for gate in CAPABILITY_GATES {
        let Some(profile) = gate.feature_profile_1_6 else {
            continue;
        };
        if feature_enabled(gate.cargo_feature) && (gate.enabled)(capabilities) {
            profiles.push(',');
            profiles.push_str(profile);
        }
    }
    profiles
}

/// Builders for [`Capabilities`], because the struct is `#[non_exhaustive]`.
///
/// That attribute is right - adding a capability must not break every integrator - but it also
/// means external code cannot use struct-expression syntax at all, so before these existed the
/// only `Capabilities` anyone outside this crate could build was [`Capabilities::default`], which
/// is all-false. A charge point that genuinely supports reservations had no way to say so. The
/// crate's own tests never noticed, because in-crate code is exempt from `#[non_exhaustive]`.
///
/// ```
/// use ocpp_charge_point::hardware::Capabilities;
///
/// let capabilities = Capabilities::default()
///     .with_smart_charging(true)
///     .with_reservation(true);
/// assert!(capabilities.smart_charging);
/// ```
impl Capabilities {
    /// Sets `max_current_per_connector_amps` - see that field.
    #[must_use]
    pub fn with_max_current_per_connector_amps(mut self, amps: Option<u16>) -> Self {
        self.max_current_per_connector_amps = amps;
        self
    }

    /// Sets `iso15118_support` - see that field.
    #[must_use]
    pub fn with_iso15118_support(mut self, level: Iso15118SupportLevel) -> Self {
        self.iso15118_support = level;
        self
    }

    /// Sets `has_display` - see that field.
    #[must_use]
    pub fn with_has_display(mut self, enabled: bool) -> Self {
        self.has_display = enabled;
        self
    }

    /// Sets `supports_bidirectional_power` - see that field.
    #[must_use]
    pub fn with_supports_bidirectional_power(mut self, enabled: bool) -> Self {
        self.supports_bidirectional_power = enabled;
        self
    }

    /// Sets `can_unlock_under_load` - see that field.
    #[must_use]
    pub fn with_can_unlock_under_load(mut self, enabled: bool) -> Self {
        self.can_unlock_under_load = enabled;
        self
    }

    /// Sets `has_rtc` - see that field.
    #[must_use]
    pub fn with_has_rtc(mut self, enabled: bool) -> Self {
        self.has_rtc = enabled;
        self
    }

    /// Sets `has_persistent_storage` - see that field.
    #[must_use]
    pub fn with_has_persistent_storage(mut self, enabled: bool) -> Self {
        self.has_persistent_storage = enabled;
        self
    }

    /// Sets `reservation` - see that field.
    #[must_use]
    pub fn with_reservation(mut self, enabled: bool) -> Self {
        self.reservation = enabled;
        self
    }

    /// Sets `local_auth_list` - see that field.
    #[must_use]
    pub fn with_local_auth_list(mut self, enabled: bool) -> Self {
        self.local_auth_list = enabled;
        self
    }

    /// Sets `smart_charging` - see that field.
    #[must_use]
    pub fn with_smart_charging(mut self, enabled: bool) -> Self {
        self.smart_charging = enabled;
        self
    }

    /// Sets `firmware_management` - see that field.
    #[must_use]
    pub fn with_firmware_management(mut self, enabled: bool) -> Self {
        self.firmware_management = enabled;
        self
    }

    /// Sets `firmware_publishing` - see that field.
    #[must_use]
    pub fn with_firmware_publishing(mut self, enabled: bool) -> Self {
        self.firmware_publishing = enabled;
        self
    }

    /// Sets `diagnostics` - see that field.
    #[must_use]
    pub fn with_diagnostics(mut self, enabled: bool) -> Self {
        self.diagnostics = enabled;
        self
    }

    /// Sets `certificate_management` - see that field.
    #[must_use]
    pub fn with_certificate_management(mut self, enabled: bool) -> Self {
        self.certificate_management = enabled;
        self
    }

    /// Sets `variable_monitoring` - see that field.
    #[must_use]
    pub fn with_variable_monitoring(mut self, enabled: bool) -> Self {
        self.variable_monitoring = enabled;
        self
    }

    /// Sets `tariff_and_cost` - see that field.
    #[must_use]
    pub fn with_tariff_and_cost(mut self, enabled: bool) -> Self {
        self.tariff_and_cost = enabled;
        self
    }

    /// Sets `payment` - see that field.
    #[must_use]
    pub fn with_payment(mut self, enabled: bool) -> Self {
        self.payment = enabled;
        self
    }

    /// Sets `der_control` - see that field.
    #[must_use]
    pub fn with_der_control(mut self, enabled: bool) -> Self {
        self.der_control = enabled;
        self
    }

    /// Sets `battery_swap` - see that field.
    #[must_use]
    pub fn with_battery_swap(mut self, enabled: bool) -> Self {
        self.battery_swap = enabled;
        self
    }

    /// Sets `periodic_event_stream` - see that field.
    #[must_use]
    pub fn with_periodic_event_stream(mut self, enabled: bool) -> Self {
        self.periodic_event_stream = enabled;
        self
    }

    /// Sets `certificates` - see that field.
    #[must_use]
    pub fn with_certificates(mut self, enabled: bool) -> Self {
        self.certificates = enabled;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fully_conservative() {
        let capabilities = Capabilities::default();
        assert!(!capabilities.has_display);
        assert!(!capabilities.supports_bidirectional_power);
        assert!(!capabilities.can_unlock_under_load);
        assert!(!capabilities.has_rtc);
        assert!(!capabilities.has_persistent_storage);
        assert_eq!(capabilities.iso15118_support, Iso15118SupportLevel::None);
        assert_eq!(capabilities.max_current_per_connector_amps, None);
        assert!(!capabilities.reservation);
        assert!(!capabilities.local_auth_list);
        assert!(!capabilities.smart_charging);
        assert!(!capabilities.firmware_management);
        assert!(!capabilities.firmware_publishing);
        assert!(!capabilities.diagnostics);
        assert!(!capabilities.variable_monitoring);
        assert!(!capabilities.tariff_and_cost);
        assert!(!capabilities.payment);
        assert!(!capabilities.der_control);
        assert!(!capabilities.battery_swap);
        assert!(!capabilities.periodic_event_stream);
        assert!(!capabilities.certificates);
    }

    #[test]
    fn struct_update_syntax_opts_into_one_field() {
        let capabilities = Capabilities {
            has_display: true,
            ..Default::default()
        };
        assert!(capabilities.has_display);
        assert!(!capabilities.supports_bidirectional_power);
    }
}
