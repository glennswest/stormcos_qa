#!/usr/bin/env bash
# Run by stormcentral's test runner on the build box before `podman build`
# (docs/test-standard.md), and usable by hand: builds the static test binary
# the Containerfile copies in as /test.
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true
T=x86_64-unknown-linux-musl
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
cargo build --release --locked --target "$T" --quiet -p stormcos-qa-test
mkdir -p test/out
cp "$CARGO_TARGET_DIR/$T/release/stormcos-qa-test" test/out/test
echo "test/out/test: $(du -h test/out/test | cut -f1)"
