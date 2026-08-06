---
name: Bug report
about: Report a problem with the crate's behaviour
title: ""
labels: bug
assignees: ""
---

## Description

A clear and concise description of what the bug is.

## Steps to reproduce

1. ...
2. ...
3. ...

A minimal reproduction (code snippet, test case, or link to a branch) is
very helpful.

## Expected behaviour

What you expected to happen.

## Actual behaviour

What actually happened. Include panics, logs, or `StatusNotification` /
`SecurityEventNotification` output if relevant.

## Environment

* Crate version / commit:
* OCPP protocol version in use (1.6J / 2.0.1 / 2.1):
* Target (`std` / `tokio-runtime` / `no_std`, and hardware platform if
  embedded):
* Rust version (`rustc --version`):

## Additional context

Anything else relevant: hardware backend involved, connector/EVSE state at
the time, related issues, etc.
