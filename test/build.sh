#!/usr/bin/env bash
# Run by stormcentral's test runner on the build box before `podman build`
# (docs/test-standard.md), and usable by hand: builds the static test binary
# the Containerfile copies in as /test.
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true
T=x86_64-unknown-linux-musl
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
# Frame pointers: the crash report walks them (crates/qa-test/src/crash.rs).
# Remapped paths: the same commit builds the same binary, so
# tools/symbolize-crash.sh can name a crash's addresses afterwards.
export RUSTFLAGS="${RUSTFLAGS:-} -C force-frame-pointers=yes --remap-path-prefix=$PWD=/src --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
cargo build --release --locked --target "$T" --quiet -p stormcos-qa-test -p must-gather
mkdir -p test/out
cp "$CARGO_TARGET_DIR/$T/release/stormcos-qa-test" test/out/test
# must-gather rides along: `/test must-gather` runs it, and its per-node
# collector pods run this same image (#45).
cp "$CARGO_TARGET_DIR/$T/release/must-gather" test/out/must-gather
# Empty /proc and /sys for the scratch image: stormpump mounts them there,
# and warns on every container's stderr when they are missing (#26).
mkdir -p test/out/rootfs/proc test/out/rootfs/sys
echo "test/out/test: $(du -h test/out/test | cut -f1), test/out/must-gather: $(du -h test/out/must-gather | cut -f1)"
