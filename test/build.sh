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
cargo build --release --locked --target "$T" --quiet -p stormcos-qa-test
mkdir -p test/out
cp "$CARGO_TARGET_DIR/$T/release/stormcos-qa-test" test/out/test
# must-gather rides along: `/test must-gather` runs it, and its per-node
# collector pods run this same image (#45). It lives in glennswest/stormgather
# (#48): built here at the rev crates/qa-test/Cargo.toml pins for the retry
# helper, with stormgather's own Cargo.lock (--locked).
rev=$(sed -n 's/^retry = .*stormgather.*rev = "\([0-9a-f]*\)".*/\1/p' crates/qa-test/Cargo.toml)
[[ $rev =~ ^[0-9a-f]{40}$ ]] || { echo "test/build.sh: no stormgather rev in crates/qa-test/Cargo.toml" >&2; exit 1; }
cargo install --locked --quiet --target "$T" --root "$CARGO_TARGET_DIR/stormgather" \
  --git https://github.com/glennswest/stormgather --rev "$rev" must-gather
cp "$CARGO_TARGET_DIR/stormgather/bin/must-gather" test/out/must-gather
# Empty /proc and /sys for the scratch image: stormpump mounts them there,
# and warns on every container's stderr when they are missing (#26).
mkdir -p test/out/rootfs/proc test/out/rootfs/sys
echo "test/out/test: $(du -h test/out/test | cut -f1), test/out/must-gather: $(du -h test/out/must-gather | cut -f1)"
