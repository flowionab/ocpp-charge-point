#!/usr/bin/env python3
"""Regenerates the per-version OCPP message-coverage numbers in README.md (H5.3).

Counts, per protocol version, how many of `ocpp-client`'s generated actions have a
corresponding `.on_*(`/`.send_*(` call somewhere in this crate's per-version adapter code
(the `mod ocpp_1_6`/`mod ocpp_2_0_1`/`mod ocpp_2_1` blocks and files under `src/`).

This is a *message-wired* count, not a certification-profile count: a message being wired
means some adapter calls the generated method for it, nothing more. Whether a build can
actually *claim* a certification profile is H3's job (OCTT + the part-6 test-case sweep),
not this script's.

Why not a naive `grep -c on_`: three traps undercount a naive sweep -
  1. Acronyms: `ocpp-client` keeps the acronym intact in the generated method name
     (`GetDERControl` -> `on_get_der_control`, not `on_get_d_e_r_control`;
     `AFRRSignal` -> `send_afrr_signal`). This script sidesteps the problem entirely by
     reading the actual `send_x`/`on_x` identifiers out of `ocpp-client`'s own
     `ocpp_client::ocpp_<ver>::actions::` macro invocations, rather than deriving them by
     snake-casing the action name itself.
  2. One 2.1 action, `NotifyPeriodicEventStream`, is generated via a different macro
     (`ocpp_2_1_send_action!`, a SEND-only shape) than every other action
     (`ocpp_2_1_action!`, a CALL/CALLRESULT shape) - both are parsed here.
  3. Several adapter modules are named with a prefix, not exactly `ocpp_2_1`/`ocpp_2_0_1`/
     `ocpp_1_6` (e.g. `mod clear_cache_ocpp_2_1`) - matched here too.

A correct sweep leaves no wired `on_x`/`send_x` call unmatched against the action list; this
script asserts that as a self-check and fails loudly if a future adapter introduces a call it
can't attribute (usually meaning `ocpp-client` renamed or added an action this script's
regexes don't understand yet).

Usage:
    python3 scripts/message-coverage.py [path-to-ocpp-client-0.5.0-source]

If the path is omitted, it's discovered from the local Cargo registry cache
(~/.cargo/registry/src/*/ocpp-client-0.5.0), which requires having built this crate at least
once against that exact version.
"""

from __future__ import annotations

import glob
import os
import re
import sys

VERSIONS = ["ocpp_1_6", "ocpp_2_0_1", "ocpp_2_1"]

ACTION_MACRO_RE = re.compile(
    r"(?:\w+_action!)\(\s*"
    r"(\w+),\s*(\w+),\s*(\w+),\s*"
    r'"([A-Za-z0-9]+)",\s*'
    r"(\w+),\s*(\w+),\s*(\w+)\s*\)"
)
# `ocpp_2_1_send_action!` has a different (5-arg, no wait_for) shape: name, payload, "Action",
# send, on.
SEND_ACTION_MACRO_RE = re.compile(
    r"ocpp_2_1_send_action!\(\s*(\w+),\s*(\w+),\s*\"([A-Za-z0-9]+)\",\s*(\w+),\s*(\w+)\s*\)"
)

CALL_RE = re.compile(r"\.(on_[a-z0-9_]+|send_[a-z0-9_]+)\s*\(")


def find_client_src(explicit: str | None) -> str:
    if explicit:
        return explicit
    matches = glob.glob(
        os.path.expanduser("~/.cargo/registry/src/*/ocpp-client-0.5.0/src")
    )
    if not matches:
        sys.exit(
            "Could not find ocpp-client-0.5.0 sources under ~/.cargo/registry. "
            "Run `cargo fetch` first, or pass the path explicitly."
        )
    return matches[0]


def parse_actions(actions_path: str) -> dict[str, tuple[str, str]]:
    """Returns {action_name: (send_method, on_method)}."""
    text = open(actions_path, encoding="utf-8").read()
    actions: dict[str, tuple[str, str]] = {}
    for m in ACTION_MACRO_RE.finditer(text):
        _name, _req, _res, action_name, send, on, _wait_for = m.groups()
        actions[action_name] = (send, on)
    for m in SEND_ACTION_MACRO_RE.finditer(text):
        _name, _payload, action_name, send, on = m.groups()
        actions[action_name] = (send, on)
    return actions


def strip_test_modules(text: str) -> str:
    """Drop `mod tests { ... }` bodies - test-only mock usage shouldn't count as production
    wiring."""
    pat = re.compile(r"mod\s+tests\s*\{")
    out, i = [], 0
    for m in pat.finditer(text):
        if m.start() < i:
            continue
        out.append(text[i : m.start()])
        depth, j = 1, m.end()
        while depth > 0 and j < len(text):
            depth += {"{": 1, "}": -1}.get(text[j], 0)
            j += 1
        i = j
    out.append(text[i:])
    return "".join(out)


def inline_module_bodies(text: str, modname: str) -> list[str]:
    """`mod modname { ... }` or `mod <prefix>_modname { ... }` (any visibility), brace-matched."""
    bodies = []
    pat = re.compile(r"(?:pub(?:\(crate\))?\s+)?mod\s+(?:\w+_)?" + re.escape(modname) + r"\s*\{")
    for m in pat.finditer(text):
        depth, i = 1, m.end()
        while depth > 0 and i < len(text):
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        bodies.append(text[m.end() : i - 1])
    return bodies


def has_file_module_decl(text: str, modname: str) -> bool:
    pat = re.compile(r"(?:pub(?:\(crate\))?\s+)?mod\s+" + re.escape(modname) + r"\s*;")
    return bool(pat.search(text))


def collect_wired_calls(src_root: str) -> dict[str, set[str]]:
    wired: dict[str, set[str]] = {v: set() for v in VERSIONS}
    for path in glob.glob(os.path.join(src_root, "**", "*.rs"), recursive=True):
        text = strip_test_modules(open(path, encoding="utf-8").read())
        dirpath, base = os.path.dirname(path), os.path.basename(path)[:-3]
        for v in VERSIONS:
            chunks = inline_module_bodies(text, v)
            if has_file_module_decl(text, v):
                for candidate in (
                    os.path.join(dirpath, base, v + ".rs"),
                    os.path.join(dirpath, v + ".rs") if base == "mod" else None,
                ):
                    if candidate and os.path.isfile(candidate):
                        chunks.append(
                            strip_test_modules(open(candidate, encoding="utf-8").read())
                        )
            for chunk in chunks:
                wired[v].update(m.group(1) for m in CALL_RE.finditer(chunk))
    return wired


def main() -> int:
    explicit = sys.argv[1] if len(sys.argv) > 1 else None
    client_src = find_client_src(explicit)
    repo_src = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src")

    action_files = {
        "ocpp_1_6": os.path.join(client_src, "ocpp_1_6", "actions.rs"),
        "ocpp_2_0_1": os.path.join(client_src, "ocpp_2_0_1", "actions.rs"),
        "ocpp_2_1": os.path.join(client_src, "ocpp_2_1", "actions.rs"),
    }
    all_actions = {v: parse_actions(p) for v, p in action_files.items()}
    wired_calls = collect_wired_calls(repo_src)

    exit_code = 0
    for v in VERSIONS:
        actions = all_actions[v]
        calls = wired_calls[v]
        matched = {a for a, (send, on) in actions.items() if send in calls or on in calls}
        unmatched_calls = calls - {m for a in matched for m in actions[a]}
        missing = sorted(set(actions) - matched)
        print(f"{v}: {len(matched)}/{len(actions)} wired")
        if missing:
            print(f"  not wired: {', '.join(missing)}")
        if unmatched_calls:
            exit_code = 1
            print(
                f"  ERROR: {len(unmatched_calls)} call(s) didn't match any known action "
                f"(ocpp-client likely renamed/added actions this script doesn't parse yet): "
                f"{', '.join(sorted(unmatched_calls))}"
            )
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
