# Threat model

This is the threat model for the `ocpp-charge-point` crate: the firmware application layer that
sits on top of `ocpp-client` and a set of integrator-supplied hardware bindings
(`crate::hardware`). It is written for a certification auditor or an integrator's security
reviewer, and every claim below is grounded in a specific module, type, or test in this repo as
of this writing — not in what OCPP or good practice generally recommends. Where this crate does
not defend against something, that is stated as plainly as where it does; an auditor who catches
one overclaim discounts the whole document, so the goal here is a model that survives scrutiny,
not one that reads well.

Task ID: **F5.1** (`docs/PRODUCTION-ROADMAP.md` §8.5). Roadmap task IDs are cross-referenced
throughout so this document stays a live index into current work rather than a snapshot that
rots the day a gap closes.

## 1. Assets

What this charge point is worth attacking, roughly in the order a CSMS operator would rank them:

1. **Billing integrity.** The energy and duration a `TransactionEvent`/`MeterValues` report to
   the CSMS must reflect what was actually delivered. A charge point that under- or over-reports
   is a direct financial loss to the operator or the driver.
2. **The CSMS credential.** The HTTP Basic password (security profile 1/2) or client private key
   (profile 3) that authenticates this station to its CSMS. Losing it lets an attacker impersonate
   the station or, if profile 3's password is randomised per Basic-auth policy, tamper with
   another station's session data at the CSMS.
3. **Firmware authenticity.** Nothing about a charge point's other guarantees holds if arbitrary
   firmware can run on it.
4. **Customer identifiers.** `IdToken` values (RFID UIDs, ISO 15118 EMAID, etc.) presented at the
   connector. These are the closest thing this crate handles to PII.
5. **Availability.** A charge point that cannot charge, or cannot report that it is charging, is
   a lost sale and — for higher-power DC — potentially a safety-relevant stuck state if it also
   cannot fail safe.

## 2. Trust boundaries

| Boundary | What crosses it | Who controls the far side |
|---|---|---|
| CSMS connection (OCPP-J WebSocket) | Every CALL/CALLRESULT/CALLERROR in both directions | The CSMS operator, or whoever holds the CSMS credential / is on-path to the connection |
| Hardware bindings (`crate::hardware::*` traits) | Contactor commands, meter readings, connector sensor state, storage reads/writes, key operations | The integrator who implemented the trait, and whatever silicon sits behind it |
| Local network (before/around the WebSocket, e.g. a LAN or the vehicle's own comms) | Nothing in-scope for this crate directly — `ocpp-client` owns the socket; ISO 15118 vehicle communication is out of scope entirely | The network operator / the EV |
| Physical access to the station | Tamper switches, connector locks, service ports, the flash the firmware and any `Storage` implementation live on | Whoever can reach the enclosure |

The crate's own code runs entirely inside the "charge point" node in this picture. Everything
this document calls "not mitigated" is either a claim about one of the other three boundaries, or
an honest statement that this side of the boundary has no defence to offer yet.

## 3. Threat actors

- **A network attacker on the CSMS connection's path** (no TLS, or TLS with a trust failure the
  operator ignores). Can read, inject, or drop frames.
- **A malicious or compromised CSMS.** Holds valid credentials and can send anything OCPP allows,
  including commands that legitimately reconfigure the station (`SetNetworkProfile`,
  `SetVariables`, firmware update triggers). This crate's stance throughout is that *some* CSMS
  actions are trusted by design — OCPP hands the CSMS real authority over the station — and the
  interesting question is which of those actions this crate refuses to let go too far (e.g.
  §4.1's security-profile downgrade rule).
- **An attacker with physical access to the station**, but not to the CSMS credential or the key
  store's private material.
- **A malicious or buggy integrator hardware binding.** Out of scope for defence (this crate
  trusts its own `hardware` trait implementations by construction — see §4.7) but in scope for
  *containment*: what a hardware fault can and cannot cascade into.
- **A remote party sending arbitrary WebSocket frames with no valid session at all** (before/around
  authentication) — relevant to payload-size and malformed-input handling.

## 4. Threats, mitigations, and gaps

### 4.1 CSMS credential compromise / connection downgrade

**Threat.** An attacker obtains the Basic-auth password or forces the station onto a weaker
security profile, e.g. by sending a crafted `SetNetworkProfile`.

**Mitigated today:**
- `src/security_profile.rs`'s `BasicAuthPassword` enforces OCPP's own 16–64 character bound
  (A00.FR.205) before a password can reach the wire, and its `Debug` impl prints only
  `BasicAuthPassword(<redacted>)` — a credential cannot leak through this crate's own trace/log
  output by accident. The accessor is named `expose()`, not `as_str()`, so every call site that
  actually reads the password out is visually distinct in review.
- `SecurityProfileChange::evaluate` enforces §A05's downgrade rule: raising a profile is always
  allowed; dropping *to* profile 1 is refused unconditionally, with no operator opt-out; dropping
  3→2 requires the operator's explicit `AllowSecurityProfileDowngrade`. This is the one place a
  single compromised CSMS credential is stopped from turning into a fleet-wide move to cleartext.
- `network_switch.rs` never carries the original connection's Basic-auth credentials to a new
  address reached via `SetNetworkProfile`/profile switching — a CSMS-supplied redirect cannot
  exfiltrate this station's password to a host of its choosing.
- Accepting a `SetNetworkProfile` raises `ReconfigurationOfSecurityParameters`
  (`src/network_profile.rs`), giving the CSMS's own back-office an audit trail of the change.

**Not mitigated / explicit gaps:**
- **Security profile 3 (mutual TLS) is not implemented.** `SecurityProfile::is_implemented()`
  returns `false` for `TlsMutualAuth` and says so in its own docs — this crate does not pretend a
  station is presenting a client certificate it cannot yet present. Blocked on F2.4 (key storage;
  done) and B4.3 (CSR round-trip; done) meeting in **F1.3**, which is genuinely blocked only on
  wiring the two together, per the roadmap's F1.3 entry.
- **A profile switch to an endpoint requiring Basic auth will simply fail and roll back** (the
  credential is deliberately not carried across, §4.1 above) — this is a documented limitation of
  A9/network-switching, not a security control, and it means an operator relying on
  `SetNetworkProfile` to move a station onto a Basic-auth endpoint needs a separate credential
  provisioning step. Tracked as part of workstream F (security profiles) rather than A9.
- **TLS itself is `ocpp-client`'s job, not this crate's.** `src/security_profile.rs`'s
  `TlsVersion`/`is_permitted()` can turn "the connection negotiated below TLS 1.2" into the
  `InvalidTLSVersion` security event, but only for a transport that reports what it negotiated —
  nothing in this crate observes or enforces the negotiated version or cipher suite itself. F2.2
  (trust-store management) is also open.
- **Basic-auth password entropy is not, and cannot be, checked.** `BasicAuthPassword::new` only
  bounds length; a 40-character string of the same repeated character passes every mechanical
  test there is. Generating a high-entropy password is the operator's responsibility.
- **Password rotation (F3.1) exists as a `SetVariables` write path but this crate does not itself
  force periodic rotation** — that is an operator policy, not a mechanism this crate enforces.

### 4.2 Firmware and certificate authenticity

**Threat.** Malicious or corrupted firmware is installed, or the station is tricked into trusting
a forged CSMS/charging-station certificate.

**Mitigated today:**
- `src/hardware/key_storage.rs`'s `KeyStore` trait has **no method that can return private key
  material**, by design (`generate_key_pair` returns only the public half and an opaque
  `KeyHandle`; `sign` takes a digest and hands back only a signature). An integrator with a real
  secure element (ATECC608, TPM, SE050) can implement this trait such that the private key
  physically never leaves the element. `KeyStore::backing()` reports
  `KeyStoreBacking::HardwareSecureElement` vs. `KeyStoreBacking::Software` so a caller — ultimately
  whatever checks whether security profile 3's "Advanced Security" expectations are actually
  met — can tell which promise is being kept, rather than trusting that *a* `KeyStore` is present
  at all.
- `SecurityEventType` models all six certificate/firmware-related standardized events
  (`InvalidFirmwareSignature`, `InvalidFirmwareSigningCertificate`, `InvalidCsmsCertificate`,
  `InvalidChargingStationCertificate`, `DiscardedRenewedClientCertificate`, plus `FirmwareUpdated`
  for a successful update), so the reporting pipeline exists end to end (`src/security.rs`,
  `src/state/security_event.rs`) the moment something detects one of these conditions.
- `src/certificates.rs` actually raises `InvalidChargingStationCertificate` today: when a CSMS
  sends an unsolicited `CertificateSigned` that matches no outstanding `SignCertificate` request,
  or one that does correlate but whose chain the certificate store refuses, both are reported.
  This is a real, wired defence against a CSMS pushing a certificate for a key the station never
  asked to have signed.

**Not mitigated / explicit gaps — this is the largest gap surface in the crate:**
- **`FirmwareUpdated`, `InvalidFirmwareSignature`, and `InvalidFirmwareSigningCertificate` are
  modelled but never raised anywhere in this crate.** There is no firmware-update functional
  block here at all yet (`docs/PRODUCTION-ROADMAP.md` workstream B3/B4 firmware rows), so nothing
  exists that could detect a bad signature to report it. An integrator wiring `UpdateFirmware`
  handling themselves is *expected* to call `report_security_event` for these, but nothing in
  this crate verifies a firmware signature on their behalf.
- **`InvalidCsmsCertificate`, `InvalidTlsVersion`, `InvalidTlsCipherSuite`,
  `DiscardedRenewedClientCertificate` are modelled but never raised by this crate's own code
  either** — see the full list in §5. These require either upstream `ocpp-client` TLS
  introspection this crate does not currently receive, or functional blocks (certificate renewal,
  F3.2) that are not yet built.
- **Secure boot is entirely out of scope (F5.4, open).** This crate has no way to verify, and does
  not attempt to verify, that the firmware it is running inside was itself loaded by a trusted
  bootloader. That is squarely the integrator's hardware/bootloader responsibility.
- **`SoftKeyStore` (the software fallback) stores private keys in `Storage` in whatever encoding
  the `SoftwareCrypto` backend produces, with no encryption layer of its own.** Its own module
  docs say so directly: "a key in flash is a key an attacker with the flash has." Confidentiality
  is entirely whatever the `Storage` implementation and the enclosure provide — see §4.6.

### 4.3 Malformed or oversized inbound messages

**Threat.** A remote party — CSMS or on-path attacker — sends malformed JSON or an
implausibly large frame to exhaust memory or crash the MCU.

**Mitigated today (`src/payload_limit.rs`, F5.2):**
- `SizeLimitedStream` wraps `ocpp_client::TransportStream` and refuses (drops, without
  forwarding) any inbound text frame over a configurable ceiling (`DEFAULT_MAX_INBOUND_FRAME_BYTES`
  = 32 KiB, override via `PayloadLimits`), raising `SecurityEventType::MemoryExhaustion` when it
  does. This stops the *second*, much larger allocation `ocpp-client` would otherwise perform —
  building the sized Rust structure for the decoded request (a 2.1 `ChargingProfile` is 56 KB by
  value per D2.3) — before it happens.

**Not mitigated / explicit gaps, and this module's docs are unusually direct about them:**
- **This only covers redials, not the initial dial.** `ocpp_client::connect`/`connect_1_6`/
  `connect_2_0_1`/`connect_2_1` build the very first connection internally and give this crate no
  hook to intervene before either of `ocpp-client`'s own allocations. Only
  `network_switch::ConnectionTarget::dial` (every connection *after* the first) goes through the
  public `websocket_transport` API this crate can wrap.
- **`tokio-tungstenite`'s own 64 MiB max-message-size default is not configurable from this
  crate at all** — `ocpp-client` 0.5.0 passes `None` for `WebSocketConfig` with nothing in
  `ConnectOptions` reaching it. Even on a wrapped redial, a frame under 64 MiB but over this
  crate's 32 KiB ceiling has already been fully assembled into one `String` by `tokio-tungstenite`
  before `SizeLimitedStream::recv` ever sees it and can refuse it. What is actually prevented is
  the subsequent, more expensive deserialization into typed Rust structures — not the initial
  frame-buffering allocation itself.
- **`ocpp-client`'s first parse step (`serde_json::from_str::<Value>`) is unconditional and
  untouchable from here** for any frame that does reach a `Client` (i.e. every frame on the
  initial dial, and every frame under the ceiling on a redial). A malformed-but-small JSON payload
  is a normal parse failure `ocpp-client` handles; this crate adds nothing on top of that path.
- **`SecurityEventType::InvalidMessages` is modelled and its `is_critical()` classification is
  tested, but nothing in this crate's production code actually raises it.** It appears only in
  test fixtures (`src/builder.rs`'s flood test, `src/security.rs`'s own unit test). There is
  currently no code path that detects "a malformed message was received" and reports it as this
  event — the closest live behaviour is `ocpp-client`'s own parse-failure handling, which this
  crate does not observe or translate into a `SecurityEventNotification`.

### 4.4 Replay of CSMS-initiated commands

**Threat.** A CSMS command whose effect is state-gated (only meaningful the first time) is
resubmitted after it already took effect — by a network attacker replaying a captured CALL body
under a fresh message id, a compromised/buggy CSMS, or a stuck retry loop.

**Mitigated today (`src/replay_protection.rs`, F5.3):** `ReplayGuard` records
`RequestStopTransaction`'s completed `TransactionId`s (bounded, 16 by default, oldest evicted
first) and raises `AttemptedReplayAttacks` when a stop is refused *and* the id is recognized as
already-completed. This never changes the outcome — the state machine's own refusal to
double-apply the effect already stood — the guard only adds a report on top of a refusal that was
always going to happen. `TransactionId` is used specifically because it is the one identifier in
this crate that is strictly monotonic and never reused, even across a restart, making false
positives structurally rare.

**Not mitigated by design, stated in the module's own docs:**
- **Transport-level replay of a captured frame** is left to TLS's record-layer sequence numbers
  (Security Profile 2/3). Security Profile 1 (plain `ws://`) has no such protection, and this
  module builds nothing to compensate — that is a deployment/`ocpp-client` transport concern.
- **A replayed CALLRESULT/CALLERROR for one of the station's own outgoing CALLs** is already
  handled by `ocpp-client`'s `pending_responses` bookkeeping (the first match consumes the id; a
  second is silently dropped) — nothing to add here.
- **Duplicate/replayed incoming CALL message ids are not deduplicated at all**, and this crate
  states plainly that it *cannot* do so: `ocpp-client` 0.5.0's `Client::on` handler signature does
  not hand the registered handler the message id in the first place, so there is no way to see,
  let alone deduplicate, incoming CALL ids through its current public API. This is called out as a
  gap that belongs in `ocpp-client` (which owns CALL/CALLRESULT correlation per this crate's own
  architecture rule), not something patchable from above it.
- **`UnlockConnector` and `RequestStartTransaction` are deliberately not guarded** — their natural
  replay keys ((evse, connector) and (evse, idToken)) recur across ordinary, unrelated commands
  (a connector gets unlocked and reused; a fleet card starts a session on the same EVSE on a
  later day), so guarding them would flag routine repeats as attacks. The module's stated
  principle throughout is that a false rejection of a legitimate command is worse than missing an
  exotic replay.
- **The offline queue's own resend-after-reconnect is structurally excluded from detection**, by
  wiring rather than by a special case: `ReplayGuard` only observes inbound, CSMS-initiated
  handlers, and the offline queue lives entirely on the outbound reporting path.

### 4.5 Security event visibility to the CSMS

**Threat.** A security-relevant event happens but the CSMS never learns about it (silently
swallowed, or evicted by an unrelated flood).

**Mitigated today:** All 21 standardized OCPP security event types are modelled
(`src/state/security_event.rs`), each independently tested against the spec appendix's own
Critical/non-critical classification. `is_critical()` decides where an event goes per OCPP's A04:
a critical event both goes to the CSMS (queued for guaranteed delivery while offline, via
`crate::offline_queue`) and is retained in the durable, bounded `SecurityEventLog`
(`src/security.rs`, 50 entries default, oldest evicted first, survives a reboot via
`crate::persistence::SecurityLogStore`); a non-critical event goes to the log only.
`InvalidMessages` and `AttemptedReplayAttacks` are deliberately classified non-critical
specifically *because* a remote party can generate them at will — sharing the bounded
notification queue with critical events would let an attacker flood a queued
`TamperDetectionActivated` or `InvalidFirmwareSignature` out of the queue before the CSMS ever
saw it, silencing the report of their own intrusion. A vendor-specific `Other(_)` event is treated
as critical unconditionally, since this crate cannot know what an integrator meant by raising one
and over-reporting is the recoverable direction.

**Not mitigated / explicit gaps:**
- **Of the 21 modelled event types, only 8 are ever raised by this crate's own code path today**
  — see the exact list and call sites in §5. The other 13 are either purely integrator-raised
  (e.g. `TamperDetectionActivated`, `MaintenanceLoginAccepted`/`Failed` — this crate has no
  tamper switch or maintenance UI of its own to detect them from) or not raised by anything yet
  because the detecting functional block doesn't exist (firmware/certificate/TLS events — §4.2).
- **1.6J reports none of these at all.** `SecurityEventNotification` is not part of core OCPP
  1.6J — it only exists via the 1.6 Security Whitepaper, whose message set `ocpp-types` does not
  currently generate (D2.2, open upstream). A 1.6J-connected station still records every event
  in the durable log; it simply cannot notify the CSMS in real time. This is a version capability
  difference, not a defect in this crate's own logic.
- **`GetLog`'s reader for the security log does not exist yet** (F4.3, partial) — the log itself
  is durable and bounded, but a CSMS cannot yet pull it on demand; an operator today can only see
  what already reached them as live `SecurityEventNotification`s.

### 4.6 Data at rest

**Threat.** Whatever this crate persists through `Storage` is read by someone with access to the
underlying medium.

**As implemented:** `crate::hardware::Storage` is a plain byte-oriented key-value trait with
**no encryption of its own, by contract** — its own module docs describe it as backed by "flash,
a filesystem, a database, or nothing at all," and put confidentiality entirely on whatever medium
the integrator chooses. Concretely, in the clear through `Storage` today:
- In-flight transaction records (`crate::persistence`) — includes the `IdToken` that authorized
  the session (§1's customer-identifier asset).
- The durable security event log (`SecurityLogStore`) — event types and free-text `techInfo`,
  which could itself contain identifying detail depending on what raised it.
- `SoftKeyStore`'s private keys, in whatever encoding its `SoftwareCrypto` backend produces (§4.2).
- Network connection profiles — **excluding** `basicAuthPassword`, which is deliberately *not*
  persisted (`src/state/network_profile.rs`) precisely because it is a credential this crate has
  no use for once dropped and no business holding at rest longer than necessary.

**Not mitigated:** This crate applies no encryption-at-rest layer of its own to any of the above.
An integrator whose `Storage` implementation writes to unencrypted flash accepts that anyone with
physical access to that flash reads transaction history, `IdToken`s, security log content, and
(for `SoftKeyStore`) private key material in the clear. This is the gap the "hardware-backed vs.
software fallback" distinction in `key_storage.rs` exists to make visible rather than paper over —
see `KeyStore::backing()`.

### 4.7 Hardware bindings and the integrator boundary

**Threat.** A hardware fault, or a malicious/buggy integrator binding, drives the charge point
into an unsafe or inconsistent state.

**Mitigated today:** Per `CLAUDE.md`'s error-handling stance, every hardware binding call is
treated as fallible; a failure drives the connector into an explicit `Faulted`/`FaultedSafe`
state rather than being swallowed or left to panic, and recovery prefers fail-safe transitions
(open the contactor, then unlock) over fail-open ones. This crate never lets a hardware error take
down the actor or the charge point process — failures are contained at the boundary and surfaced
as state transitions and OCPP-visible status.

**Explicitly out of scope, by design:** This crate trusts its own `hardware` trait implementations.
There is no defence here against an integrator binding that lies about what happened (reports a
contactor open when it is closed, reports a meter value that was never measured, etc.) — the
entire hardware abstraction exists so that the *integrator* owns hardware correctness, and this
crate's contract with them is "call these traits as documented and trust the answers." A
compromised or defective binding can therefore defeat billing integrity or safety guarantees that
this crate itself cannot see behind. This is stated as the boundary it is, not hidden as an
implicit assumption: an auditor evaluating a specific deployment must separately evaluate that
deployment's hardware binding, not just this crate.

### 4.8 A compromised CSMS

**Threat.** The CSMS itself — not just its channel — is malicious or compromised, and sends
otherwise well-formed, correctly-authenticated commands designed to damage the station or its
operator.

OCPP gives the CSMS broad, legitimate authority (reconfigure charging profiles, remotely stop
transactions, install certificates, change network profiles, trigger resets). This crate's stance
is not "distrust the CSMS" — that would be incompatible with OCPP's own protocol design — but to
hold a small number of specific lines a CSMS may not cross regardless of what it asks for:

- **§4.1**: a CSMS cannot use `SetNetworkProfile` to force the station to cleartext.
- **§4.2**: a CSMS cannot install a certificate the station never requested via a correlated
  `SignCertificate` (unsolicited `CertificateSigned` is refused and reported).
- Everything else a CSMS can legitimately ask for — starting/stopping charging, changing tariffs,
  updating firmware once that block exists — is treated as within its authority, because a CSMS
  that has been given the credential is, by OCPP's model, the operator's trusted control point.
  A CSMS that has gone rogue while still holding valid credentials is a business/operational
  problem (revoke the credential, audit the back office) that no amount of charge-point-side logic
  can fully substitute for.

## 5. Security event coverage: raised vs. modelled only

All 21 standardized types plus `Other` are modelled in `SecurityEventType`
(`src/state/security_event.rs`). As of this document, exactly **8** are ever raised by this
crate's own production code (not test fixtures):

| Event | Raised from |
|---|---|
| `StartupOfTheDevice` | `src/builder.rs`, on `ChargePointBuilder` startup |
| `ResetOrReboot` | `src/reset.rs`, on a completed reset |
| `SettingSystemTime` | `src/provisioning.rs`, when the CSMS's `currentTime` triggers a clock step past this crate's drift threshold |
| `MemoryExhaustion` | `src/payload_limit.rs` (oversized inbound frame refused) and `src/builder.rs` (offline queue overflow) |
| `ReconfigurationOfSecurityParameters` | `src/network_profile.rs`, on an accepted `SetNetworkProfile` |
| `AttemptedReplayAttacks` | `src/remote_control.rs`, on a `RequestStopTransaction` replay (§4.4) |
| `SecurityLogWasCleared` | `src/persistence.rs`, when the security log is durably cleared |
| `InvalidChargingStationCertificate` | `src/certificates.rs`, on an unsolicited or store-refused `CertificateSigned` (§4.2) |

The remaining 13 (`FirmwareUpdated`, `FailedToAuthenticateAtCsms`, `CsmsFailedToAuthenticate`,
`InvalidFirmwareSignature`, `InvalidFirmwareSigningCertificate`, `InvalidCsmsCertificate`,
`DiscardedRenewedClientCertificate`, `InvalidTlsVersion`, `InvalidTlsCipherSuite`,
`MaintenanceLoginAccepted`, `MaintenanceLoginFailed`, `InvalidMessages`, and vendor-specific
`Other`) are either purely integrator-raised (this crate has no tamper switch, maintenance login,
or TLS introspection of its own — `Other` is always integrator-supplied by definition) or not
raised by anything in this codebase yet because the detecting functional block does not exist
(firmware handling, certificate renewal/TLS observation — see §4.2). `report_security_event` is
the one documented entry point for both this crate and an integrator to raise any of them; the
reporting pipeline downstream of that call (queueing, logging, criticality routing) is identical
regardless of who calls it.

## 6. Summary for an auditor

- **Confidentiality of the channel**: delegated to `ocpp-client`'s TLS (profiles 2/3); profile 1
  is unsecured by spec design and this crate does not compensate for that at the application
  layer, consistent with OCPP's own guidance that profile 1 belongs on an already-trusted network.
- **Confidentiality at rest**: none provided by this crate; entirely the integrator's `Storage`
  and hardware choice (§4.6).
- **Integrity of CSMS-initiated commands**: enforced case by case — a security-profile downgrade
  floor (§4.1), an unsolicited-certificate refusal (§4.2), and state-gated replay detection for
  one command (§4.4) — not a blanket authorization model, because OCPP's own design gives the CSMS
  broad authority by default (§4.8).
- **Availability under hostile input**: partial, and honestly scoped — the redial path is guarded
  against oversized frames (§4.3), the initial dial is not, and malformed-but-small input is left
  to `ocpp-client`'s own parsing.
- **Firmware and certificate authenticity**: the private-key handling primitive
  (`KeyStore`'s no-export invariant) is solid; the surrounding verification logic (firmware
  signature checking, TLS introspection, CSMS certificate validation) is largely not yet built —
  this is the single largest area to track against the roadmap (workstream B4, F1.3, F2.2, F3.2,
  F5.4).
- **Physical/hardware trust**: explicitly out of this crate's scope; delegated to the integrator's
  `hardware` trait implementation and the enclosure around it (§4.7).

Track the open items above against `docs/PRODUCTION-ROADMAP.md` workstreams B4, F1–F5, and D2.2 —
this document should be revisited whenever one of the referenced task IDs changes status.
