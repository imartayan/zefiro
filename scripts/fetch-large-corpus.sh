#!/usr/bin/env bash
# Fills corpus/large/ (gitignored) with the large benchmark inputs, which the bench runs
# with --large: silesia.tar (the 12 Silesia files, about 202 MiB). Needs curl, tar and unzip.
# Idempotent. SILESIA_ZIP uses a local copy instead of downloading.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/upstream.sh

mkdir -p corpus/large

if [ ! -s corpus/large/silesia.tar ]; then
  zip=${SILESIA_ZIP:-$upstream/silesia.zip}
  [ -s "$zip" ] || curl -fsSL -o "$zip" "$SILESIA_URL"
  work=$upstream/silesia-large
  rm -rf "$work"
  unzip -q -d "$work" "$zip"
  names=(dickens mozilla mr nci ooffice osdb reymont samba sao webster xml x-ray)
  tar -c -n --no-recursion -f corpus/large/silesia.tar -C "$work" "${names[@]}"
  rm -rf "$work"
fi
ls -l corpus/large
