#!/usr/bin/env bash
# ThreadSanitizer on the threaded pipeline tests (nightly, about 12 min on M1).
set -euo pipefail
cd "$(dirname "$0")/.."
target=$(rustc -vV | sed -n 's/^host: //p')
RUSTFLAGS=-Zsanitizer=thread cargo +nightly test -Zbuild-std --target "$target" \
    --target-dir target/tsan --lib parallel "$@"
