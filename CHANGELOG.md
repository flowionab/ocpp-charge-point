# Changelog

All notable changes to this crate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows the pre-1.0 reading
of [Semantic Versioning](https://semver.org/) described in [`docs/SEMVER.md`](docs/SEMVER.md).

`0.1.0` is this crate's first release, so everything below is the history that led to it rather
than a delta against a previous version. It is grouped by development milestone (see [`docs/PRODUCTION-ROADMAP.md`](docs/PRODUCTION-ROADMAP.md))
rather than by date, since milestones are the unit this project actually plans and completes in.
**Breaking** entries are the ones that change what an integrator's existing code must do to keep
compiling or behaving the same way (see [`docs/SEMVER.md`](docs/SEMVER.md) for exactly what that
means for a trait) - each was checked against its actual diff, not inferred from a commit
subject line. This is a reconstruction from `git log`, not every commit: entries below are the
ones that would break or surprise an integrator, or that materially describe what the crate can
now do; purely internal refactors, test additions, and documentation-only commits are omitted
unless they're the easiest way to explain a milestone's scope.

## [Unreleased]

OCPP 2.1 conformance work, planned in [`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md`](docs/OCPP-2.1-COMPLIANCE-ROADMAP.md)
and grounded in the requirement-level audit in [`docs/OCPP-2.1-COMPLIANCE-AUDIT.md`](docs/OCPP-2.1-COMPLIANCE-AUDIT.md).

### Breaking

- `hardware::ChargePoint` gained `electrical()`, returning the phase counts, connector types and
  per-EVSE power OCPP requires as device-model variables (CV1.5). **Default-implemented**, unlike
  `capabilities()`, so existing implementations keep compiling — a station that ignores it still
  registers the required variables, empty.
- The four OCPP 2.x meter-data notifiers — `transactions::Ocpp2_1TransactionNotifier`,
  `transactions::Ocpp2_0_1TransactionNotifier`, `meter_values::Ocpp2_1MeterValuesNotifier` and
  `meter_values::Ocpp2_0_1MeterValuesNotifier` — take a `ChargePointActor` on `with_clock` and
  `new` (CV2.6). They read `SampledDataCtrlr`/`AlignedDataCtrlr`'s measurand lists per message, so
  a CSMS narrowing or clearing one takes effect on the next event rather than the next boot. The
  1.6J notifiers are unchanged: `MeterValuesSampledData` is a different mechanism.
- `ChargePointBuilder::status_notifications` and `status_notifications_persisted` take a `Backoff`
  (CV2.7). Honouring `MinimumStatusDuration` means holding a status back until it has settled, and
  holding something back needs a timer — this crate's only `no_std`-friendly one is `Backoff`. The
  alternatives were an implicit dependency on `provisioning()` having been called first, or a
  second set of debounced-variant methods; both trade a compile error for a station that silently
  ignores the variable, which is the failure mode CV2 exists to remove. Untouched on a station
  that has not set the variable.
- `remote_control::handle_request_start_transaction` takes the request's `remoteStartId`, and
  `ConnectorEvent::RemoteStartRequested` became a struct variant carrying it (CV6).
- `TransactionNotifier::notify_transaction_event` takes an `offline` flag and returns a
  `TransactionEventOutcome`, so a CSMS rejecting an identifier mid-session can drive E05 (CV2.5,
  CV6.1).
- New `ConnectorState::StoppingLocked` and `ConnectorEvent::LockFailed` variants; exhaustive
  matches over either must handle them (CV2.4, CV11).
- New `ConnectorEvent::RunningCostAdvanced` variant, carrying the boxed
  `pricing::TransactionCost` the local cost engine computed for a connector's transaction (CV8,
  I07/I08/I11/I12); exhaustive matches over `ConnectorEvent` must handle it.
- `TriggeredMonitor::monitor_id` is now `Option` — `None` marks a hard-wired notification, which
  OCPP requires for a lock failure (CV11).
- **Local generation now widens the composite limit instead of capping it** (CV17, K27, 2.1 Part 2
  §K.3.6). New `ChargingProfilePurpose::LocalGeneration` variant, which *adds* its capacity on top
  of the composed result rather than bounding it — exhaustive matches over the purpose must handle
  it, and a station whose CSMS sends `LocalGeneration` profiles will now charge *higher* than
  before. That is the fix: the purpose previously arrived through a `_` arm as
  `ExternalConstraints`, so 2 kW of local generation under a 5 kW `TxDefaultProfile` composed to
  2 kW where the spec composes 7 kW. New `ChargingProfilePurpose::adds_to_the_result` alongside
  `caps_the_result`.
- `state::ExternalChargingLimit` gained `is_local_generation`, and
  `ChargePointEvent::ExternalChargingLimitCleared` gained the same field: struct literals of either
  must add it (`false` preserves today's behaviour exactly). An integrator's energy-management
  binding that pushes locally generated capacity sets it `true`, which changes what the limit
  *does* — it adds rather than caps — and sets `isLocalGeneration` on the 2.1 wire (K27.FR.03).
- **Transaction limits are enforced** (CV15, E16). New `state::TransactionLimit` and
  `state::TransactionLimitKind`; `state::Transaction` gained `limit`, `csms_limit`,
  `limit_reached` and `energy_start_wh`, so struct literals of it must add all four (`None`
  preserves today's behaviour). New `ConnectorEvent::TransactionLimitSet` and
  `TransactionUpdateReason::{LimitSet, LimitReached}` variants; exhaustive matches over either
  must handle them. A CSMS setting `transactionLimit` on a `TransactionEventResponse`, or an
  integrator raising `TransactionLimitSet` for a driver-entered figure, now **stops energy
  transfer** when the ceiling is reached — the connector goes `SuspendedEVSE` and is commanded to
  0 A. `maxCost`, `maxEnergy` and `maxSoC` are enforced; `maxTime` is declared unsupported in
  `TxCtrlr.SupportedLimits` and is neither recorded nor confirmed.
- `state::Transaction`, `state::TransactionEventOccurred`, `state::RecoveredTransaction` and
  `transactions::TransactionEventOutcome` are no longer `Eq` (still `PartialEq`): a transaction
  limit carries `f64` costs, and rounding one to keep a derive would change the limit.
- `transactions::TransactionEventOutcome` gained `transaction_limit`, and
  `ConnectorEvent::RunningCostAdvanced` became a struct variant `{ cost, total }` — the total is
  what a `maxCost` is compared against, and deriving it needs the tariff and a clock the state
  machine does not have. `EvseState` gained `running_cost_totals` alongside.
- `ConnectorEvent::CurrentLimitComputed` is now a struct variant, `{ limit_ma, externally_caused }`
  (CV18): the projection sets the second field so the state machine can tell a rate change an
  energy manager caused from one the CSMS's own profiles caused. New
  `TransactionUpdateReason::ChargingRateChanged`, sent as `triggerReason = ChargingRateChanged`
  when an external limit is set or released, the composed rate actually moves, and a transaction is
  running (K11.FR.04, K13.FR.03). The CSMS-caused case stays unsent — K01.FR.61 makes it a `MAY`,
  and the CSMS installed the schedule whose boundaries it would be told about. Exhaustive matches
  over either enum must handle the new cases.
- `EvseState` gained `local_generation_limit` and `ChargePointState` gained
  `station_local_generation_limit`, each a second slot beside the existing external-limit one, so a
  constraint and locally generated capacity can be in force on the same scope at once (K27.FR.05)
  instead of evicting each other.
- Offline queues built through `ChargePointBuilder` no longer drain before the CSMS has accepted
  the charge point (CV4, B01.FR.08). Nothing is dropped; delivery is deferred until acceptance.
- A `SetVariables` for a variable this build does not act on is now `Rejected` rather than
  accepted and ignored (CV2.1, B05.FR.09). A CSMS that relied on the previous silent acceptance
  will start seeing refusals — which is the point.
- The same refusal now covers the capability-gated variables CV2.1 never reached (CV14,
  B05.FR.09): 24 of the 26 rows OCPP marks writable — every `TariffCostCtrlr`, `PaymentCtrlr`,
  `WebPaymentsCtrlr` and `V2XChargingCtrlr` setting, plus `SmartChargingCtrlr.
  LimitChangeSignificance` and `DisplayMessageCtrlr.Language` — are registered `ReadOnly` and
  `Rejected` on write. `ISO15118Ctrlr.ContractValidationOffline` and `.CentralContractValidation
  Allowed` stay writable, being the two this crate reads. The five `PaymentCtrlr.Merchant`
  instances are refused for a second reason: a payment terminal's status sweep owns them, so an
  accepted write would have been overwritten within the interval.
- A `SetVariables` writing `WebPaymentsCtrlr.SharedSecret` is `Rejected` (CV10): it is
  `WriteOnly`, which keeps `GetVariables` from disclosing it (A01.FR.12) but — unlike `ReadOnly` —
  does not by itself stop the write, and this crate has no consumer for it yet. An `Accepted`
  write would have kept the secret in `ChargePointState`, which `trace!` prints whole.
- `hardware::KeyStore` gained three methods — `store_credential`/`load_credential`/
  `delete_credential` — for an opaque, *retrievable* secret (CV10), as opposed to the asymmetric
  key material the rest of the trait deliberately never gives back. Every implementation must add
  them; `NoKeyStore` refuses all three and `SoftKeyStore` persists them through `Storage` the same
  way it does keys. See the trait's module docs, "Credentials are not key material", for why this
  does not undermine the no-export invariant the rest of the trait exists to protect.
- `remote_control::handle_request_start_transaction` takes the request's `groupIdToken`, and
  `reservation::handle_reserve_now` takes the reservation's, as a new `state::Reservation::
  group_id_token` field (CV7, F01.FR.22). Struct literals of `Reservation` must add the field;
  persisted records written before this decode as `None`.
- New `RequestStartTransactionOutcome::AcceptedPendingAuthorization` variant (CV7, F01.FR.01);
  exhaustive matches over that enum must handle it. It projects onto OCPP's `Accepted` exactly as
  its two sibling accepted outcomes do.
- A `RequestStartTransaction` is no longer refused outright on a `Reserved` connector (CV7,
  F01.FR.21/.22). The identity comparison those requirements are *about* was previously
  unreachable, so a reservation refused a remote start from the very driver it was made for. A
  reservation now admits a request whose `idToken` or `groupIdToken` matches, and a bay this
  driver reserved is preferred over a merely free one.
- `device_model::SetVariablesHandler::register_set_variables_handler` and
  `device_model::handle_set_variables` take a `hardware::KeyStore` (CV10) — see "Added" below for
  what it is now used for. `ChargePointBuilder::configuration`/`device_model` pass
  `hardware::NoKeyStore`, so a caller that does not opt into
  `ChargePointBuilder::basic_auth_password_rotation` sees no behavioural change.

### Added

- `NetworkConfiguration.BasicAuthPassword` writes are real (CV10, A01.FR.02): a `SetVariables`
  carrying a well-formed password (`security_profile::BasicAuthPassword`, A00.FR.205) is now
  `Accepted` rather than refused. The value is validated and persisted through
  `hardware::KeyStore::store_credential` — never through `Storage`/`ChargePointState`, which
  `crate::persistence` writes as a plain, `cat`-readable JSON blob and `trace!` prints whole — and
  the previous password is kept alongside it. A successful rotation is logged to the security log
  (`SecurityEventType::ReconfigurationOfSecurityParameters`) naming the slot and nothing else
  (A01.FR.11/.12); the value never reaches a log line, `GetVariables` (still blocked by
  `WriteOnly`), or `ChargePointState`. Opt in with the new
  `ChargePointBuilder::basic_auth_password_rotation`. Applying the rotation to the transport, and
  rolling back to the previous password after repeated authentication failure (A01.FR.04), is a
  separate opt-in on the redial side — the new
  `network_switch::ConnectionTarget::attach_basic_auth_credential` — which reads the current
  password fresh from the same `KeyStore` on every redial to the origin address (so "apply on next
  connect" needs no extra plumbing) and reverts to the previous one after
  `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts` consecutive failures, the same threshold and
  shape `ConnectionTarget::stage_tls_config` already uses for a staged TLS trust configuration.
  Scoped to the origin address only, mirroring this module's existing "credentials are not carried
  across" rule for a switched profile.
- All 122 device-model variables OCPP 2.1 marks required are now registered (CV1), including the
  per-EVSE and per-connector components and a per-slot `NetworkConfiguration` mirror.
- `SetVariables` validates values against their declared type, range and value list (CV3).
- Connector state is re-reported after a BootNotification is accepted and after a long outage
  (CV5); `TxStartPoint`/`TxStopPoint`, `EVConnectionTimeOut`, `StopTxOnEVSideDisconnect`,
  `UnlockOnEVSideDisconnect`, `StopTxOnInvalidId`/`MaxEnergyOnInvalidId` and
  `OfflineTxForUnknownIdEnabled` are honoured rather than merely stored (CV2).
- `RequestStartTransaction` with no cable yet is accepted and held until the driver plugs in —
  OCPP's F02, which previously did not work at all (CV7).
- `AuthCtrlr.AuthorizeRemoteStart` is honoured (CV7, F01.FR.01/.02). Off — the default — keeps the
  previous behaviour exactly: the CSMS's own request is the authorization decision. On, a
  `RequestStartTransaction` takes the same path a card presented at the reader does, and the
  contactor waits for the answer; the request is still answered `Accepted` immediately, as the
  requirement's own note describes. `Central` and `NoAuthorization` identifiers are exempt either
  way. The `remoteStartId` survives the round trip, so the resulting transaction still reports
  `triggerReason = RemoteStart` and quotes the id (F01.FR.25).
- `AuthCtrlr.DisableRemoteAuthorization` is registered and honoured (CV7). When set, the charge
  point issues no `AuthorizeRequest` at all and decides from the local authorization list and the
  authorization cache alone, refusing anything neither knows. Distinct from every offline switch:
  those describe what to do when the CSMS *cannot* be reached, this one is an instruction not to
  reach for it. A Plug & Charge presentation is refused rather than decided locally, since C07
  puts contract validation at the CSMS.
- `PaymentCtrlr`'s live status variables are driven by real hardware (CV2.11, C18–C24). New
  **default-implemented** `hardware::PaymentTerminal::status()` returns a `PaymentTerminalStatus`
  — `Connected`, `Problem`, `ICCID`, `IMSI` and the five `Merchant` instances — applied at
  `ChargePointBuilder::payment` registration and refreshed by the new
  `ChargePointBuilder::payment_status_updates`. Default-implemented, so no existing binding
  breaks; a station that does not implement it reports `Connected = false`, which is what this
  firmware actually knows.
- Message-size limits are enforced rather than merely declared (CV2.8). A `GetVariables`,
  `SetVariables`, `GetReport`, `SendLocalList` or `SetVariableMonitoring` carrying more items than
  `ItemsPerMessage` allows is refused with `OccurrenceConstraintViolation`, and one larger than
  `BytesPerMessage` allows with `FormatViolation` (B06.FR.16/.17, B08.FR.17/.18, D01.FR.11,
  N04.FR.09). The refusal names the variable and both numbers. New `message_limits` module.
- `ChargingStation.MinimumStatusDuration` is honoured (CV2.7, G01): a connector status is reported
  only once it has held for the configured window, so a bouncing latch no longer floods the CSMS
  with transitions that cancelled each other out. `0` — the default — keeps the previous
  report-everything behaviour exactly.
- Measurand configuration is honoured on OCPP 2.x (CV2.6, J01/J02): each `TransactionEvent` shape
  reports what its own `SampledDataCtrlr.Tx*Measurands` list names, standalone `MeterValues`
  reports what `AlignedDataCtrlr.Measurands` names, and a list cleared to empty produces no
  `meterValue` — and no standalone message at all — rather than a reading-free reading.
  `connect_and_setup` wires the actor-aware notifiers on both 2.1 and 2.0.1, so this is live on the
  default path: a CSMS `SetVariables` narrowing a list takes effect on the very next event.
- A `SignCertificate` the CSMS accepted but never answered is resent on OCPP's own doubling
  back-off (CV9, A02.FR.17–.19 / A03.FR.17–.19). New
  `certificate_renewal::run_sign_certificate_retries` is the loop: it waits
  `SecurityCtrlr.CertSigningWaitMinimum` seconds, resends the same CSR (signed with the key already
  recorded for that purpose), doubles the back-off on every expiry, and stops at
  `CertSigningRepeatTimes` until `certificates::PendingSignRequests::restart` is called for a
  `TriggerMessage` (`SignChargingStationCertificate`, `SignV2GCertificate`, `SignV2G20Certificate`
  or `SignCombinedCertificate`). A `SignCertificate` the CSMS *rejected* arms nothing (A02.FR.20).
  It consults no clock, so a station waiting on its first certificate — the one least likely to
  know the date — is still covered. Drive it with the same `PendingSignRequests` the
  `CertificateSigned` handler was registered against.
- Three new `SecurityCtrlr` variables, all honoured rather than decorative:
  `CertSigningWaitMinimum` (default `30` s), `CertSigningRepeatTimes` (default `3`) — read per pass
  by the loop above, so a CSMS write changes the back-off in force rather than the next boot's —
  and `MaxCertificateChainSize` (default `10000`, the `certificateChain` field's own wire maximum).
  A `CertificateSigned` whose chain exceeds it is now `Rejected` before the certificate store is
  touched, and the CSR stays outstanding so a resend can still get a chain that fits (A02.FR.16 /
  A03.FR.16, a `MAY` this build takes). Setting either of the first two to `0` switches resending
  off.
- New `setup_with_meter_data_notifiers`, which is `setup` with the `TransactionEvent` and
  `MeterValues` blocks driven by notifiers the caller supplies. They arrive as factories taking the
  `ChargePointActor`, because a notifier that honours measurand configuration must hold the actor
  and the actor does not exist until the charge point starts. `setup` itself is unchanged and
  passes `csms` for both — which means **`setup` called directly with a bare client does not filter
  measurands**, since such a client has no device model to read.
- A charge point with a tariff assigned now prices its own running cost locally, from the meter
  samples its transactions already record, using the `pricing` engine CV8 landed earlier —
  whichever tariff currently applies: a default tariff (`SetDefaultTariff`, I07), a driver tariff
  (`ChangeTransactionTariff`, I08), or one replaced mid-session by either (I11). New
  `EvseState::running_cost`, advanced by `tariff::advance_running_cost` and reported by
  `Ocpp2_1TransactionNotifier` as a `TransactionEvent`'s `costDetails` and
  `transactionInfo.tariffId` (I12). No tariff means no report, exactly as before. **Not covered**:
  a fixed fee or reservation dimension is only charged when a tariff was already assigned at a
  transaction's true start (no meter history exists to back-price against once it wasn't), and the
  bare `OCPP2_1Client` `TransactionNotifier` impl has no `ChargePointActor` to price against and
  reports no cost at all, mirroring its existing measurand-list limitation.

### Fixed

- **A reservation was erased by the next event that touched its connector.** The reservation
  record was reassigned on every event that left the connector `Reserved`, and only
  `ConnectorEvent::Reserved` carries one — so the first meter sample an idle connector's hardware
  pushed left the bay reserved with nothing recording who for. `GetCompositeSchedule`,
  `CancelReservation` and the CSMS-facing state all then saw an unreserved-looking bay that no
  driver could use. Only an event actually carrying a reservation records one now.
- **A held remote start awaiting authorization is no longer swept as a driver who never arrived.**
  `TxCtrlr.EVConnectionTimeOut` times how long to wait for the EV to connect; a connector whose
  cable is already latched is waiting on the CSMS instead, and releasing its hold would deauthorize
  a session for the one reason that demonstrably did not happen. Only reachable together with
  CV7's new `AuthorizeRemoteStart` path.

## [0.1.0] — 2026-08-10

First published release. The **Breaking** entries below are pre-release history — changes made
while the crate was unpublished, listed because anyone who tracked `main` lived through them.
Nothing here breaks an earlier *release*, because there was none.

### Packaging

- **`certificate-management` joined the `default` feature set.** It gates
  `ChargePointBuilder::certificates` (`InstallCertificate`/`DeleteCertificate`/
  `GetInstalledCertificateIds`), and was the one capability feature left out of `default` — which
  made that builder method invisible to anyone building with default features. A test
  (`every_capability_gate_feature_is_in_the_default_feature_set`) now fails if a gate drifts back
  out.
- **docs.rs builds with `--all-features`**, and editor-local `.idea/` no longer ships in the
  published tarball.

### Breaking

- **`ChargePoint::capabilities() -> Capabilities`** — the `ChargePoint` hardware trait gained a
  required method reporting a static [`Capabilities`](src/hardware/capabilities.rs) struct (display
  present, bidirectional power, RTC, persistent storage, ISO 15118 level, per-connector current
  ceiling, and one flag per optional functional block). Every existing `ChargePoint` impl needs
  this method added. (M1, capability model)
- **`Connector::set_current_limit`** — the `Connector` hardware trait gained a required method,
  dispatched through a new `HardwareCommand::SetCurrentLimit` at the same `(evse_id,
  connector_id)` granularity as other hardware commands, with failures routed to
  `FaultDetected` like the crate's other fail-safe commands. Landed with only its hardware surface
  wired at first — the smart-charging profile store/composition that calls it landed later in
  the same milestone. (M1, alongside `capabilities()` and `Storage`, deliberately batched into one
  break per M1's own plan)
- **`hardware::Storage`** — a new trait for durable, power-cut-surviving key/value storage, with
  `NoStorage` (default, no-op) and a `std`-gated `InMemoryStorage`. Threading it through
  `ChargePointBuilder`/`ChargePointActor::spawn` added a generic parameter integrators'
  construction code needs to account for even when using `NoStorage`. (M1)
- **`ChargePointEffect` dropped its `Eq` derive** (now `PartialEq` only, still `Debug`/`Clone`) —
  needed once a variant started carrying a `ChargingSchedule` payload, whose periods carry `f64`
  limits, and floats have no total order. Code that relied on `ChargePointEffect: Eq` (e.g. using
  it as a `HashSet`/`BTreeMap` key, or deriving `Eq` on a type that contains it) stops compiling.
  (B2.8, `NotifyChargingLimit`/`ClearedChargingLimit`/`NotifyEVChargingNeeds`/
  `NotifyEVChargingSchedule`)
- **`connect_and_setup` gained a `payload_limits: Option<PayloadLimits>` parameter** (defaulting
  like this function's other `Option` parameters) — part of F5.2's inbound-frame-size ceiling.
  Existing call sites need the new argument. (F5.2, memory-exhaustion hardening)
- **`ChargePointBuilder::firmware_updates` gained a `verifier: V` parameter** (and a `V:
  crate::hardware::FirmwareVerifier` bound), plus a `crate::firmware::SignedUpdateFirmwareHandler`
  bound on its existing CSMS type parameter — signed-firmware verification is now mandatory
  wiring for this registration method; pass `hardware::NoFirmwareVerifier` (fails closed) if the
  charge point never receives signed updates. (B3.3, signed firmware verification)
- **`ChargePoint::start` takes `self: Arc<Self>`** rather than `&self`. The command loop it spawns
  has to outlive the call, so with `&self` every implementation kept an `Arc` inside its own struct
  purely to have something to move into that task — both bundled examples did it. The runtime
  already holds an `Arc<T>` and now hands that same one to `start`, so bindings go back to plain
  owned fields; `ChargePointRuntime` stores `Arc<T>` to match. (H5.6, hardware trait
  implementability)
- **Five hardware accessors are plain `fn`, not `async fn`**: `ChargePoint::vendor_name`,
  `model_name`, `evses` and `capabilities`, plus `Evse::connectors`. None of them awaited or
  failed. `connectors()` is the one that mattered — `execute_hardware_command` calls it per
  command, so an `async fn` returning a fixed slice boxed a future per contactor operation. Impls
  need the `async` keyword removed. (H5.6)
- **`CertificateStore::has_private_key` is now `has_client_private_key`.** A real store holds
  several keys and only the client certificate's answers the question this method is asked. Rename
  the method in existing impls. (H5.6)
- **`hardware::Watchdog` lost its `Send + Sync` supertrait** — the only trait in `crate::hardware`
  to have had one. The bound now sits on the actor's `Arc<dyn Watchdog + Send + Sync>`, where the
  sharing actually happens. (H5.6)
- **`Authorizer` gained an `authorize_contract` method with a default implementation** — see
  Plug & Charge below. The default refuses locally, so an existing impl keeps compiling but
  declines every contract-certificate authorization until it implements the method.
  `run_authorization_requests` also gained a `Sync` bound, which `ChargePointBuilder` already
  required of every authorizer. (B4.6)

### Added — by milestone

- **M0 — Unblock**: per-functional-block builder registration (replacing one large multi-bound
  `setup()` signature with independent `ChargePointBuilder` methods), CI running clippy/fmt/a
  Cargo feature matrix, and the upstream (`ocpp-client`) gap list that shaped the rest of the
  roadmap.
- **M1 — Capability model**: one Cargo feature per optional OCPP functional block (`C1`); the
  runtime `Capabilities` struct and `CAPABILITY_GATES` single source of truth mapping a capability
  to its Cargo feature, 2.x `*Ctrlr` device-model component, and 1.6J feature-profile name (`C2`,
  `C3`); a data-driven test (`C3.5`) asserting all four advertisement surfaces agree; and
  protocol-correct refusal of unsupported messages, including CALLERROR for the responses that
  have no status field to carry a rejection in (`C5`). Also landed the three breaking hardware-trait
  changes above, deliberately batched into this one release.
- **M2 — Durability**: the `hardware::Storage`-backed persistence layer for in-flight
  transactions, the offline transaction-event/status/security-event queues, the authorization
  cache, local auth list, reservations, and device-model attributes; crash-consistency via an
  A/B-slot `AtomicStorage` adapter; a power-cut recovery test sweeping every point across a
  transaction session; bounded memory for every growable collection with measured ceilings
  (`docs/MEMORY.md`); and clock handling for a missing RTC, CSMS clock sync, and mid-transaction
  clock jumps.
- **M3 — Protocol completeness, core**: version negotiation across 1.6J/2.0.1/2.1; every
  Core-profile message on all three versions; smart charging (charging profiles, `GetCompositeSchedule`
  composition, hardware current limiting end to end); WebSocket ping-interval keepalive
  (`crate::keepalive`, once `ocpp-client` 0.3.0+ exposed the option); and reservation status
  updates.
- **M4 — Security and remote management**: security profiles 1–3 (profile 3 needs a multi-thread
  Tokio runtime — `rustls`'s synchronous `Signer` is bridged to the async `KeyStore::sign` via
  `block_in_place`); signed firmware update over the air, including 1.6J's Security Whitepaper
  `SignedUpdateFirmware`/`SignedFirmwareStatusNotification`; log upload; variable monitoring
  (`SetVariableMonitoring`/`NotifyEvent` and the rest of the 2.x monitoring engine); certificate
  install/delete/enumerate and CSR round-trip; OCSP status checking
  (`GetCertificateStatus`/`GetCertificateChainStatus`); and the inbound-frame-size ceiling
  (F5.2) that made `connect_and_setup`'s signature change above necessary.
- **Since M4 (pre-M5 polish)**: OCPP 2.1 payment (`NotifySettlement`/`NotifyWebPaymentStarted`/
  `VatNumberValidation`), DER control/V2X (`GetDERControl`/`SetDERControl`/`ClearDERControl`/
  `ReportDERControl`/`NotifyDERAlarm`/`NotifyDERStartStop`/`AFRRSignal`/
  `NotifyAllowedEnergyTransfer`), battery swap (`BatterySwap`/`RequestBatterySwap`), periodic
  event streams, `PublishFirmware`/`UnpublishFirmware`/`PublishFirmwareStatusNotification`, a
  secure-element/key-storage abstraction (`hardware::KeyStore`), the tariff store and
  per-transaction tariff assignment, and the display-message block.
- **Plug & Charge authorization (OCPP use case C07, 2.0.1 and 2.1)**: `Authorize` now carries the
  ISO 15118 contract certificate (`certificate`/`iso15118CertificateHashData`) and reads the
  response's `certificateStatus`, where both were previously hardcoded `None` and dropped
  respectively. The presentation is a first-class connector event —
  `ConnectorEvent::ContractCertificatePresented { id_token, certificate }` reaches `Authorizing`
  by the same transition a card tap uses, sent on the `HardwareEventSender` an integrator's HLC
  stack already has, so no new hardware trait and no change to `Iso15118Controller`. Acceptance
  needs both of the CSMS's answers: `ContractAuthorization` keeps token status and certificate
  status apart because C07.FR.13/FR.14 make them genuinely independent. Offline, C07.FR.07
  overrides this crate's own offline fallback — a contract presentation with no CSMS is refused
  and does not consult the authorization cache or local list.
  `ISO15118Ctrlr.CentralContractValidationAllowed` is registered and read (C07.FR.06), `false` by
  default like every other capability-gated value, with a withheld chain logged rather than
  silently dropped. 1.6J downgrades instead of refusing: its `Authorize` has no certificate
  fields, so the eMAID goes as a plain `idTag` with a warning saying plainly that nothing
  validated the contract. (B4.6)
- **Capability propagation for ISO 15118 and display messages**: `iso15118_support` gained its
  `CAPABILITY_GATES` row (`ISO15118Ctrlr`, charge-point-initiated so `has_handler: false`, no 1.6J
  profile, and the component's required `ContractValidationOffline` variable) — B4.5 had wired
  `Get15118EVCertificate` but left the capability out of the table, so a station with a PLC modem
  advertised nothing and one without it left the component unknown rather than honestly
  unavailable. It is the only gate whose capability is an enum; both non-`None` levels count as
  support. `DisplayMessageCtrlr` now registers all five of its required variables rather than only
  `DisplayMessages`: `SupportedPriorities`/`SupportedStates` are held in step with
  `MessagePriority::ALL`/`MessageState::ALL` by a test, and `SupportedFormats` is seeded empty and
  overwritten by `ChargePointBuilder::display_messages` from `Display::supported_formats`, which
  is what that trait's docs already claimed happened. A CSMS can read the format and state limits
  instead of discovering them through a refusal. (C3)
- **Logging and personal-data redaction**: `IdToken` has a hand-written `Debug` that redacts the
  card number, so anything containing one is safe to log by construction rather than by each call
  site remembering. The new off-by-default **`unredacted-logs`** Cargo feature restores full values
  for local bring-up against a bench CSMS; never ship an image with it on. Handlers carry
  `#[instrument(skip_all)]`, log levels follow the rules now written down in `CLAUDE.md` (an 8 KiB
  `{:?}` of `ChargePointState` belongs at `trace!`, not `info!`), and `tracing_test_support` makes
  log level and content testable rather than conventional.
- **`no_std` + `alloc` support**: `cargo check --no-default-features --lib` compiles under
  `#![no_std]`, backed by `embassy-sync` primitives (`src/sync.rs`) instead of `tokio::sync`;
  `tokio` is a fully optional dependency behind the `tokio-runtime` feature (in `default` for
  zero-config ergonomics on a normal host).

### Fixed

- **`--features iso15118` with neither `ocpp_2_0_1` nor `ocpp_2_1` failed to compile.** The
  module's three shared helpers are used only by its version adapters, so with both adapters gated
  out they tripped `dead_code` under `-D warnings`. They now carry the same `cfg` the adapters do.
- **The flash figures in [`docs/MEMORY.md`](docs/MEMORY.md), the README and the roadmap were
  measured against a stale `ocpp-client`.** `tools/flash-probe` — a separate workspace, so it
  resolves its own direct dependency — still pinned 0.2.1 after the library moved to 0.5.0, which
  put both majors in one graph and stopped the probe building at all. Bumped to 0.5.0, brought the
  probe up to the current hardware traits, and re-measured every row: the version-independent core
  is 92 KB (not 32 KB) and all three protocol versions together are 558 KB, which no longer fits a
  512 KB part before a transport and TLS.
- Migrated `ocpp-client` 0.2.2 → 0.5.0 (`ocpp-types` 0.1.3 → 0.3.0) in one change — see
  [`docs/MIGRATION-ocpp-client-0.4.md`](docs/MIGRATION-ocpp-client-0.4.md) for the measured diff.
  Closed **A4** (WebSocket keepalive, `crate::keepalive` driving `OCPPCommCtrlr.WebSocketPingInterval`
  live via `ocpp-client` 0.3.0+'s `Client::ping_interval()`/`set_ping_interval()`) and fixed
  `ConnectionCloser` to force a redial rather than a sticky disconnect on a network-profile switch.
  `ocpp-client` 0.5.0 also started generating 1.6J's `SignedUpdateFirmware`/
  `SignedFirmwareStatusNotification` (previously absent upstream, tracked as a documented gap);
  this crate wired both in a later commit — see B3.3 above.

### Documentation

- [`docs/INTEGRATORS.md`](docs/INTEGRATORS.md): which hardware traits are mandatory vs. opt-in
  (with a `No*` default), recommended Cargo-feature sets per hardware class, the distinction
  between a Cargo feature (compiles code out) and a runtime `Capabilities` flag (declares what
  the hardware can do), and when to use `setup()` vs. `ChargePointBuilder` directly.
- [`docs/THREAT-MODEL.md`](docs/THREAT-MODEL.md): the security posture this crate defends.
- This changelog and [`docs/SEMVER.md`](docs/SEMVER.md) (H5.4).
- README per-version message-coverage numbers and a corrected capability-feature table (H5.3),
  regenerable via `scripts/message-coverage.py`.

---

[0.1.0]: https://github.com/flowionab/ocpp-charge-point/releases/tag/v0.1.0
