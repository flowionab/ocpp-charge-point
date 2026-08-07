#!/usr/bin/env bash
#
# Measures the flash cost of this crate per Cargo feature set - G2.4 in
# docs/PRODUCTION-ROADMAP.md §9.2. Results are tabulated in docs/MEMORY.md.
#
# Each row builds tools/flash-probe (a real bare-metal firmware image that exercises the enabled
# features - see its module docs) for thumbv7em-none-eabihf with LTO, opt-level=z and
# --gc-sections, then reports the size of the flashable image: exactly the bytes
# `objcopy -O binary` puts in a .bin, which is what you program onto the part.
#
# Usage:
#   scripts/flash-cost.sh            # measure every feature set
#   scripts/flash-cost.sh --quick    # core + all-features only
#
# The figures move with the resolved dependency versions (this repository ignores Cargo.lock, as a
# library should), so re-run this rather than trusting a stale table after a dependency bump.
#
# Requires the thumbv7em-none-eabihf target:
#   rustup target add thumbv7em-none-eabihf
# No other tooling: rust-objcopy ships with the toolchain.

set -euo pipefail

TARGET=thumbv7em-none-eabihf
PROBE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../tools/flash-probe" && pwd)"
HOST="$(rustc -vV | sed -n 's/^host: //p')"

# `rust-objcopy` ships inside the toolchain on most platforms; where it doesn't, it arrives with the
# `llvm-tools` rustup component, and a system `llvm-objcopy`/`objcopy` works just as well as long as
# it understands ELF. Try all of them before giving up, so this script needs no setup beyond the
# target itself on a normal install.
OBJCOPY=""
for candidate in \
    "$(rustc --print sysroot)/lib/rustlib/${HOST}/bin/rust-objcopy" \
    "$(command -v rust-objcopy || true)" \
    "$(command -v llvm-objcopy || true)" \
    "$(command -v objcopy || true)"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
        OBJCOPY="$candidate"
        break
    fi
done

if [[ -z "$OBJCOPY" ]]; then
    echo "no objcopy found - install one with: rustup component add llvm-tools" >&2
    exit 1
fi

if ! rustc --print target-list | grep -qx "$TARGET"; then
    echo "unknown target $TARGET" >&2
    exit 1
fi

# label:features pairs. An empty feature list is the version-independent core: the state machine,
# the actor, the hardware binding and the functional blocks that aren't feature-gated.
SETS=(
    "core (no protocol version):"
    "core + 1.6J:ocpp_1_6"
    "core + 2.0.1:ocpp_2_0_1"
    "core + 2.1:ocpp_2_1"
    "core + all three versions:ocpp_1_6,ocpp_2_0_1,ocpp_2_1"
    "core + 2.1 + reservation:ocpp_2_1,reservation"
    "core + 2.1 + local-auth-list:ocpp_2_1,local-auth-list"
    "core + 2.1 + tariff-cost:ocpp_2_1,tariff-cost"
    "core + 2.1 + declared capabilities:ocpp_2_1,declared-capabilities"
    "everything:ocpp_1_6,ocpp_2_0_1,ocpp_2_1,reservation,local-auth-list,tariff-cost,declared-capabilities"
)

if [[ "${1:-}" == "--quick" ]]; then
    # Negative subscripts need bash 4.3; macOS still ships 3.2, so index the hard way.
    SETS=("${SETS[0]}" "${SETS[$((${#SETS[@]} - 1))]}")
fi

measure() {
    local features="$1"
    local elf="$PROBE_DIR/target/$TARGET/release/flash-probe"
    local args=(build --release --quiet --target "$TARGET")
    if [[ -n "$features" ]]; then
        args+=(--features "$features")
    fi

    # Delete the previous image first: a build that fails would otherwise leave the last feature
    # set's binary in place and this function would happily measure *that*, reporting a wrong
    # number for a configuration that doesn't even compile.
    rm -f "$elf"
    if ! (cd "$PROBE_DIR" && cargo "${args[@]}"); then
        echo "build failed for features '${features:-<none>}'" >&2
        exit 1
    fi
    if [[ ! -f "$elf" ]]; then
        echo "no image produced for features '${features:-<none>}'" >&2
        exit 1
    fi

    local bin
    bin="$(mktemp)"
    "$OBJCOPY" -O binary "$elf" "$bin"
    wc -c <"$bin" | tr -d ' '
    rm -f "$bin"
}

printf '\n%-40s %10s %12s\n' "Feature set" "Flash" "vs core"
printf '%-40s %10s %12s\n' "----------------------------------------" "----------" "------------"

baseline=""
for entry in "${SETS[@]}"; do
    label="${entry%%:*}"
    features="${entry#*:}"
    bytes="$(measure "$features")"
    if [[ -z "$baseline" ]]; then
        baseline="$bytes"
        delta="-"
    else
        delta="+$(((bytes - baseline) / 1024)) KB"
    fi
    printf '%-40s %7s KB %12s\n' "$label" "$((bytes / 1024))" "$delta"
done
printf '\nFlash = bytes in the objcopy -O binary image for %s (LTO, opt-level=z, gc-sections).\n' "$TARGET"
