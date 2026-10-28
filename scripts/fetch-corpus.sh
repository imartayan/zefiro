#!/usr/bin/env bash
# Fills corpus/ (gitignored) from ref/: the zstd -19 fixtures of rust-zstd,
# decompressed, and silesia-small.tar. Needs the zstd CLI. Idempotent.
set -euo pipefail

cd "$(dirname "$0")/.."
fixtures=ref/rust-zstd/tests/fixtures
tarball=ref/libzstd-rs-sys/silesia-small.tar

mkdir -p corpus
for zst in "$fixtures"/*.zst; do
  out=corpus/$(basename "${zst%.zst}")
  [ -s "$out" ] || zstd -dq -o "$out" "$zst"
done
[ -s corpus/silesia-small.tar ] || cp "$tarball" corpus/silesia-small.tar
ls -l corpus
