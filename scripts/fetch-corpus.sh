#!/usr/bin/env bash
# Fills corpus/ (gitignored) with the benchmark files: elf_8m, rust_src_8m and words_1m
# (zstd -19 fixtures downloaded and decompressed) and silesia-small.tar (the first
# 1310690 bytes of each Silesia file). Needs curl, tar, unzip and the zstd CLI.
# Idempotent. FIXTURES_DIR and SILESIA_ZIP use local copies instead of downloading.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/upstream.sh

mkdir -p corpus
for name in elf_8m rust_src_8m words_1m; do
  out=corpus/$name
  [ -s "$out" ] && continue
  zst=$upstream/$name.zst
  if [ -n "${FIXTURES_DIR:-}" ]; then
    zst=$FIXTURES_DIR/$name.zst
  elif [ ! -s "$zst" ]; then
    curl -fsSL -o "$zst" "$FIXTURES_URL/$name.zst"
  fi
  zstd -dq -o "$out" "$zst"
done

if [ ! -s corpus/silesia-small.tar ]; then
  zip=${SILESIA_ZIP:-$upstream/silesia.zip}
  [ -s "$zip" ] || curl -fsSL -o "$zip" "$SILESIA_URL"
  work=$upstream/silesia-small
  rm -rf "$work"
  mkdir -p "$work/silesia-small"
  unzip -q -d "$work/full" "$zip"
  members=(silesia-small)
  for name in ooffice nci mozilla dickens webster samba osdb x-ray sao xml mr reymont; do
    head -c 1310690 "$work/full/$name" > "$work/silesia-small/$name"
    members+=("silesia-small/$name")
  done
  tar -c -n --no-recursion -f corpus/silesia-small.tar -C "$work" "${members[@]}"
  rm -rf "$work"
fi
ls -l corpus
