#!/usr/bin/env bash
# Build every driver and package it: dist/NAME-VERSION.ZIP, dist/CATALOG.TXT
# (unsigned) and dist/SHA256SUMS (FREEHOLD's DECISIONS.md, D-215).
#
# Needs a sibling FREEHOLD checkout (../freehold, or $FREEHOLD) for the
# runtime crates, and the UEFI target: `rustup target add
# x86_64-unknown-uefi`. As FREEHOLD's tools/build.sh does, a machine without
# the target's prebuilt library but with rust-src compiles core and alloc
# locally (FREEHOLD_BUILD_STD=1 forces that).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
FREEHOLD="${FREEHOLD:-$(cd "$ROOT/.." && pwd)/freehold}"
if [ ! -f "$FREEHOLD/programs/rt/Cargo.toml" ]; then
    echo "build: no FREEHOLD checkout at $FREEHOLD (clone Gerszcom/freehold next to this repository)" >&2
    exit 1
fi
if [ "$(cd "$FREEHOLD" && pwd)" != "$(cd "$ROOT/.." && pwd)/freehold" ]; then
    # The crates' paths name ../freehold; another checkout is linked there.
    echo "build: the drivers' Cargo.toml files expect ../freehold; FREEHOLD=$FREEHOLD is not it" >&2
    exit 1
fi

ARGS=(build --target x86_64-unknown-uefi --release)
SYSROOT="$(rustc --print sysroot)"
if [ "${FREEHOLD_BUILD_STD:-0}" = 1 ] || [ ! -d "$SYSROOT/lib/rustlib/x86_64-unknown-uefi/lib" ]; then
    if [ -d "$SYSROOT/lib/rustlib/src/rust/library" ]; then
        echo "build: compiling core/alloc for x86_64-unknown-uefi locally (build-std)"
        export RUSTC_BOOTSTRAP=1
        ARGS+=(-Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem)
    else
        echo "build: target x86_64-unknown-uefi is not installed; run: rustup target add x86_64-unknown-uefi" >&2
        exit 1
    fi
fi
for inf in drivers/*/DRIVER.INF; do
    ARGS+=(-p "$(basename "$(dirname "$inf")")")
done
cargo "${ARGS[@]}" "$@"
rm -rf dist
python3 tools/package.py --freehold "$FREEHOLD" --out dist
echo "build: dist/ — $(ls dist/*.ZIP | wc -l) driver zip(s), CATALOG.TXT, SHA256SUMS"
