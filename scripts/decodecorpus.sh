#!/usr/bin/env bash
# Builds tests/decodecorpus.c of zstd v1.5.7 (downloaded once into target/upstream, or
# the tree in $ZSTD_SRC) and fills
# corpus/decodecorpus/{zst,orig} with valid frames using every format feature
# and their originals.
#
#   [SET=-name] scripts/decodecorpus.sh [COUNT] [SEED] [extra decodecorpus flags...]
#
# SET picks the directories zst$SET and orig$SET (default: zst and orig), so
# several sets can coexist, e.g. SET=-bs10 ... --max-block-size-log=10.
# With --use-dict=SIZE the frames use a generated dictionary of SIZE bytes: they go to
# dict$SET/{zst,orig}, with the dictionary next to the frames (dict$SET/zst/dictionary).
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/upstream.sh
count=${1:-2000}
seed=${2:-1}
shift $(( $# > 2 ? 2 : $# ))

src=$(zstd_src)
L=$src/lib
P=$src/programs
bin=target/decodecorpus

mkdir -p target
# zstd_compress.c is included by decodecorpus.c itself
cc -O2 -w -DZSTD_DISABLE_ASM=1 -DZSTD_LEGACY_SUPPORT=0 -DZSTD_MULTITHREAD=0 \
  -I$L -I$L/common -I$L/compress -I$L/decompress -I$L/dictBuilder -I$P \
  $L/common/*.c $(ls $L/compress/*.c | grep -v '/zstd_compress.c') $L/decompress/*.c \
  $L/dictBuilder/*.c $P/util.c $P/timefn.c \
  $src/tests/decodecorpus.c -o $bin -lm

out=corpus/decodecorpus
set_name=${SET:-}
zst=$out/zst$set_name
orig=$out/orig$set_name
if [[ " $* " == *" --use-dict"* ]]; then
  zst=$out/dict$set_name/zst
  orig=$out/dict$set_name/orig
fi
rm -rf "$zst" "$orig"
mkdir -p "$zst" "$orig"
$bin -p"$zst/" -o"$orig/" -n"$count" -s"$seed" "$@"
echo "$(ls "$zst" | wc -l) files in $zst"
