# Security Policy

This crate implements firmware behaviour for an EV charge point, including
its OCPP-facing network surface. Vulnerabilities here can have real-world
safety and availability impact (e.g. contactor control, charging sessions,
connectivity to a CSMS), so we treat security reports seriously and ask that
you do too.

## Supported Versions

This project is pre-1.0 and under active development. Security fixes are
made against the latest release on the `main` branch; older `0.x` releases
are not backported.

| Version | Supported          |
| ------- | ------------------ |
| latest  | :white_check_mark: |
| older   | :x:                |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Report vulnerabilities privately using one of the following channels:

1. **Preferred**: [GitHub Security Advisories](https://github.com/flowionab/ocpp-charge-point/security/advisories/new)
   for this repository.
2. **Email**: [joatin@granlund.io](mailto:joatin@granlund.io). If the report
   is sensitive, you may encrypt it — ask for a PGP key in your initial
   message.

Please include as much of the following as you can:

* A description of the vulnerability and its potential impact (e.g. denial
  of service, unauthorized state transitions, unsafe contactor/relay
  behaviour, protocol spoofing).
* Steps to reproduce, or a proof-of-concept.
* The affected version/commit and, if relevant, which hardware backend or
  OCPP protocol version (1.6J / 2.0.1 / 2.1) was involved.
* Any suggested mitigation, if you have one.

### What to expect

* We aim to acknowledge new reports within 5 business days.
* We will keep you updated as we investigate and work on a fix.
* We will credit reporters in the release notes/advisory unless you request
  otherwise.
* Please give us reasonable time to address the issue and publish a fix
  before any public disclosure.

## Scope

In scope: this crate's protocol handling, state machines, transaction
lifecycle, and networking behaviour as they relate to security (e.g.
authentication/authorization to a CSMS, message validation, state-machine
safety, resource exhaustion).

Vulnerabilities in dependencies (e.g. `ocpp-client`, `ocpp-types`) should
generally be reported to those projects directly, but we're happy to help
route a report if you're unsure where it belongs.
