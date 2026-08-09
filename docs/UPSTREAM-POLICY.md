# Upstream dependency policy (D3.2)

This answers [D3.2 in `PRODUCTION-ROADMAP.md` §6.3](./PRODUCTION-ROADMAP.md#63-d3--dependency-policy):
"Vendor-or-fork contingency if upstream PRs stall." It is a decision aid,
not a plan of record — consult it when `ocpp-client` or `ocpp-types` is
blocking a task, decide, write the outcome next to the roadmap row that
hit the block, and move on.

`CLAUDE.md` is explicit that networking and wire-protocol concerns belong
in `ocpp-client`, not here: *"Delegate OCPP networking and transport
concerns to the ocpp-client crate. Do not duplicate connection-management
or wire-protocol functionality here."* That rule stands. This document is
about what to do in the gap between "upstream should have this" and
"upstream has this" — not a license to route around it.

## What "pinned" means today

`Cargo.toml` requires `ocpp-client = "0.5.0"` (Cargo's default caret range:
`>=0.5.0, <0.6.0`), pulling `ocpp-types` transitively — it is not a direct
dependency of this crate. `Cargo.lock` currently resolves both to the exact
versions [D3.1](./PRODUCTION-ROADMAP.md#63-d3--dependency-policy) targeted:
`ocpp-client` 0.5.0 / `ocpp-types` 0.3.0. The range matches D3.1's intent —
"a version range this crate has actually tested against" — and the lockfile
pins the tested point inside it. A `cargo update -p ocpp-client` within
`0.5.x`-compatible bounds could move the lockfile without a `Cargo.toml`
edit; treat that as a real upgrade requiring the same re-verification as a
manual version bump (full suite, MCU target, the `UPSTREAM-GAPS.md` audit
re-run), not a no-op.

## Decision: upstream first, always — the question is how long to wait

1. **Default: upstream.** If the gap is a missing type, action wrapper, or
   transport capability that belongs in `ocpp-client`/`ocpp-types` by this
   crate's own architecture rule, write the change against upstream's
   source, open the PR, and consume it once released. A9 and D1 are the
   model: A9 wrote `ConnectOptions::reconnector` and public
   `websocket_transport()` upstream and waited for 0.2.2; D1 wrote six
   action wrappers upstream and 0.2.2 shipped all 64 2.0.1 actions as a
   result. Waiting is not idleness — while blocked, do the rest of the
   task's scope and say plainly what's deferred and why, per this round's
   ground rules.
2. **Escalate only when the wait has a cost the schedule can't absorb** —
   a milestone genuinely blocked with no other open task to do, a security
   fix that can't wait for a release cadence, or a wait that has already
   run past the checkpoint below with no maintainer response at all (not
   just "not yet merged"). Escalating earlier than that trades a temporary
   local cost for a permanent maintenance liability (see the cost table
   below) — don't make that trade to save a few weeks.
3. **Re-verify before escalating, every time.** This is D2.2's failure
   mode, and it is cheap to avoid — see the mechanism below.

## Escalation options, ranked by cost (cheapest first)

| Option | What it means | Cost | When it's right |
|---|---|---|---|
| **Wait** | Do other work; re-check on the cadence below. | Opportunity cost only, no code debt. | Default. Almost always right if nothing is actually on fire. |
| **Pin to a git rev** | `ocpp-client = { git = "...", rev = "..." }` pointing at an open PR or a maintainer's branch. | Loses crates.io provenance and `cargo-deny`'s advisory/licence checking for that dependency (D3.3); must be swapped back to a released version before any tagged release of this crate. | The fix exists and is reviewable, just not released yet, and the wait is past the escalation trigger above. |
| **Vendor a patched copy** | Fork the crate into this workspace (path dependency), diffed from the upstream release, with the diff and its removal condition documented at the top of the vendored crate. | Ongoing merge burden on every upstream release; the fork silently drifts if nobody re-diffs it against new releases; doubles the surface `cargo audit`/`cargo-deny` must reason about. | The fix is small, well-understood, and upstream has gone genuinely silent (no maintainer response, not just unmerged) — and the alternative is blocking a release of this crate. |
| **Fork outright** | Publish and depend on an independent crate under this project's control. | Full ongoing maintenance of someone else's crate: every future upstream fix must be manually ported or the fork permanently diverges; this contradicts `CLAUDE.md`'s "delegate to ocpp-client" architecture and should be treated as abandoning that rule, not a tactic within it. | Only if upstream is abandoned outright (no releases, no response, for a sustained period) and the dependency is load-bearing enough that this crate cannot ship without it. Requires an explicit decision recorded here and in `CHANGELOG.md`, not a quiet workaround. |
| **Implement locally, with a migration path** | Model the missing behaviour inside this crate's own version-independent core, with a comment marking it as a stand-in and the upstream issue/PR it should be replaced by. | Duplicates logic `CLAUDE.md` says belongs upstream; must be actively deleted once upstream lands, or it silently becomes permanent (this is closer to D2.2's failure than a fix for it). | Last resort, and only for internal state/behaviour this crate would own anyway if the version-independent model demanded it — never for wire-protocol or transport, which `CLAUDE.md` reserves for `ocpp-client` categorically. |

Vendoring and forking are not a way to avoid the wait — they are a way to
pay for skipping it, in ongoing maintenance rather than schedule time. Pick
the wait unless one of the trigger conditions above is actually true.

## Avoiding D2.2's failure: don't re-cite a blocked assumption, re-check it

D2.2 recorded the 1.6J security whitepaper types as absent upstream and
framed the choice as "contribute or declare out of scope." `ocpp-client`
0.4.0 added them, and the "blocked" framing kept being quoted as current
for months because nobody re-ran the check. The fix that would have caught
this is cheap and mechanical, not a process — do this before treating any
`UPSTREAM-GAPS.md` finding as still true:

```bash
# What's pinned right now, vs. what this crate's Cargo.toml range allows:
cargo tree -p ocpp-client -p ocpp-types
# Latest published release, to see if the range would already pick up a fix:
cargo search ocpp-client   # or check crates.io/crates/ocpp-client directly
```

Concretely: **any time a task's brief cites `UPSTREAM-GAPS.md` (or this
document) as grounds for "blocked" or "out of scope," re-run that specific
check against the *currently pinned* version before repeating the
conclusion** — not against whatever version the finding was originally
measured on. `UPSTREAM-GAPS.md`'s own header now does this (it re-verified
D2.3 against 0.3.0 directly rather than repeating the 0.2.0-era note), and
that pattern — restate the finding's version, re-measure, then decide — is
what to copy. A finding that is silently three releases stale is
indistinguishable, to a reader, from one that's still true; the only fix is
re-measuring, not remembering to be skeptical.

## Open items this policy applies to right now

- **[D2.3](./UPSTREAM-GAPS.md) (see `docs/MEMORY.md`'s "D2.3" section) —
  `ChargingProfile`'s ~50 KB by-value size.** An upstream report is drafted
  in [`docs/MEMORY.md`](./MEMORY.md) with a reproducible measurement,
  deliberately not filed (the project's last upstream recommendation was
  withdrawn for resting on an unreproduced number — see `docs/ROADMAP.md`'s
  hard-won lessons). Per this policy: **wait**, and file it once there's
  the access to do so, then track a response on the cadence above — it's a
  drafted report, not a stalled PR, so there is nothing to escalate yet.
  The local exposure is already bounded (`StateLimits`-backed storage holds
  a 96-byte reduced model, not the wire type; the ~50 KB cost is transient,
  inside `ocpp-client`'s own deserialization, before this crate's code
  runs), so there is no correctness or memory-budget reason to reach for
  vendoring.
- **[F1.3](./PRODUCTION-ROADMAP.md#81-f1--security-profiles) — needs an
  async `Signer` in `rustls`.** Currently bridged with `block_in_place` +
  `Handle::block_on`, which requires a multi-thread Tokio runtime and is
  documented as a live constraint at the module level
  (`crate::mutual_tls`). This gap is in `rustls`, not `ocpp-client`/
  `ocpp-types` — outside this crate's direct dependency-policy relationship
  with the OCPP crates, and a much larger, more heavily-used upstream where
  a fork is not remotely proportionate. Per this policy: **wait**; the
  workaround is already documented and functioning under its stated
  constraint, so there is no schedule pressure to escalate further than
  documenting the constraint, which F1.3 already does.

Neither open item meets the escalation bar above (no maintainer silence, no
undeferrable schedule cost) — both are correctly sitting in "wait," which
this document exists to keep visible rather than let go stale the way
D2.2 did.
