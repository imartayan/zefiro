#!/usr/bin/env bash
# Seeds fuzz/corpus/{decompress,stream,round_trip,mutate,dict,parallel}: decodecorpus frames (with the
# 2 byte capacity prefix of `decompress`, the 5 byte chunk and window prefix of `stream`),
# golden files, decodecorpus frames with their dictionaries (`dict`, 5 byte prefix: dictionary
# length, capacity), and text with parameter prefixes for the generated targets.
#
#   scripts/fuzz-seeds.sh [MAX_FRAMES_PER_SET]
set -euo pipefail

cd "$(dirname "$0")/.."
max=${1:-150}
count=0
out=fuzz/corpus
mkdir -p "$out/decompress" "$out/stream" "$out/round_trip" "$out/mutate" "$out/dict" "$out/parallel"

# Writes the capacity prefix (little-endian u16, clamped) then the frame.
seed() {
  local frame=$1 size=$2 dest=$3
  (( size > 65535 )) && size=65535
  { printf "\\x$(printf %02x $((size & 255)))\\x$(printf %02x $((size >> 8)))"; cat "$frame"; } > "$dest"
}

# Chunk prefixes of `stream`: sizes 4001 and 4001, 1 and 4001, 6 and 6 with a 2^21 window limit.
stream_prefixes=('\x0c\xfa\x0c\xfa\x00' '\x03\x00\x0c\xfa\x00' '\x54\x00\x54\x00\x90')
stream_seed() {
  local frame=$1 index=$2 dest=$3
  { printf "${stream_prefixes[$((index % 3))]}"; cat "$frame"; } > "$dest"
}

for set_dir in corpus/decodecorpus/zst*; do
  [ -d "$set_dir" ] || continue
  set_name=$(basename "$set_dir")
  orig_dir=corpus/decodecorpus/orig${set_name#zst}
  for frame in $(ls "$set_dir" | head -n "$max"); do
    size=$(wc -c < "$orig_dir/${frame%.zst}")
    seed "$set_dir/$frame" "$size" "$out/decompress/dc-$set_name-${frame%.zst}"
    stream_seed "$set_dir/$frame" "$((++count))" "$out/stream/dc-$set_name-${frame%.zst}"
  done
done
for frame in tests/data/golden/*.zst; do
  [ -f "$frame" ] || continue
  seed "$frame" 4096 "$out/decompress/golden-$(basename "$frame" .zst)"
  stream_seed "$frame" "$((++count))" "$out/stream/golden-$(basename "$frame" .zst)"
done

# `dict`, frame bytes mode: 5 bytes (dictionary length, capacity as 2 bytes = selector and extra,
# flags 0), the dictionary, then the frame. Decodecorpus sets with a dictionary (under 64 KiB).
dict_seed() {
  local dictionary=$1 frame=$2 size=$3 dest=$4
  local length
  length=$(wc -c < "$dictionary")
  (( size > 65535 )) && size=65535
  {
    printf "\\x$(printf %02x $((length & 255)))\\x$(printf %02x $((length >> 8)))"
    printf "\\x$(printf %02x $((size & 255)))\\x00\\x$(printf %02x $((size >> 8)))"
    cat "$dictionary" "$frame"
  } > "$dest"
}
for set_dir in corpus/decodecorpus/dict*; do
  [ -f "$set_dir/zst/dictionary" ] || continue
  (( $(wc -c < "$set_dir/zst/dictionary") < 65536 )) || continue
  for frame in $(ls "$set_dir/zst" | grep '\.zst$' | head -n "$max"); do
    size=$(wc -c < "$set_dir/orig/${frame%.zst}")
    dict_seed "$set_dir/zst/dictionary" "$set_dir/zst/$frame" "$size" "$out/dict/dc-$(basename "$set_dir")-${frame%.zst}"
  done
done

# Generated targets: parameter bytes (level, flags, window, match length) + payload.
text=corpus/words_1m
if [ -f "$text" ]; then
  i=0
  for params in '\x03\x03\x00\x00' '\x01\x1a\x00\x00' '\x07\x43\x05\x81' '\x08\x87\x03\x00' '\x00\x02\x00\x83' '\x05\x0b\x00\x00'; do
    for offset in 0 40000 300000; do
      { printf "$params"; dd if="$text" bs=1 skip=$offset count=3000 2>/dev/null; } > "$out/round_trip/text-$i"
      { printf "$params"; printf '\x03\x10\x00\x41\x20\x00\x07\x00\xff'; dd if="$text" bs=1 skip=$offset count=3000 2>/dev/null; } > "$out/mutate/text-$i"
      # `dict`, generated mode: raw content dictionary (1 KiB of the text), mutations, payload.
      { printf '\x00\x04\x03\x0b\x02\x10\x00\x41\x20\x00\x07'; dd if="$text" bs=1 skip=$((offset + 500)) count=1024 2>/dev/null; dd if="$text" bs=1 skip=$offset count=3000 2>/dev/null; } > "$out/dict/text-$i"
      i=$((i + 1))
    done
  done
fi
# `parallel`: the decodecorpus and golden seeds of `decompress` behind a thread count byte.
count=0
for seed in "$out"/decompress/dc-* "$out"/decompress/golden-*; do
  { printf "\\x0$((count++ % 3))"; cat "$seed"; } > "$out/parallel/$(basename "$seed")"
done
for target in decompress stream round_trip mutate dict parallel; do
  echo "$target: $(ls "$out/$target" | wc -l) seeds"
done
