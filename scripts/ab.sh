#!/usr/bin/env bash
# A/B of the one-shot bench on tree copies (git is not needed).
#   scripts/ab.sh snapshot          copy the tree to target/ab/base
#   scripts/ab.sh run [bench args]  alternate base and current, print best-of-N deltas
# AB_ALTERNATIONS (default 5) runs of each tree, `--rounds 3 --impl ours` unless
# the bench args override. corpus/ and the frame cache are shared by both trees.
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD
base=target/ab/base
runs=target/ab/runs
export ZSTD_CORPUS_DIR=$root/corpus ZSTD_BENCH_CACHE=$root/target/bench-cache

case "${1:-}" in
snapshot)
  mkdir -p "$base"
  rsync -a --delete --exclude /target/ --exclude /corpus/ --exclude /ref/ \
    --exclude /fuzz/ --exclude /.git/ ./ "$base/"
  ;;
run)
  shift
  [ -d "$base" ] || { echo "no snapshot, run: scripts/ab.sh snapshot" >&2; exit 1; }
  if [ -n "${AB_INPLACE:-}" ]; then
    # Both binaries are built from this directory, swapping src/ in and out: the path is part
    # of the crate hash, and a build elsewhere can differ by 3 to 5% on small frames.
    stash=target/ab/cur-src
    mkdir -p target/ab/bin
    rsync -a --delete src/ "$stash/"
    trap 'rsync -a --delete "$root/$stash/" "$root/src/" && find "$root/src" -type f -exec touch {} +' EXIT
    # touch: rsync keeps the old mtimes, and cargo would not rebuild.
    rsync -a --delete "$base/src/" src/ && find src -type f -exec touch {} +
    cargo build --release -q -p compare --bin bench
    cp target/release/bench target/ab/bin/base
    rsync -a --delete "$stash/" src/ && find src -type f -exec touch {} +
    cargo build --release -q -p compare --bin bench
    cp target/release/bench target/ab/bin/cur
    bin_base=$root/target/ab/bin/base
    bin_cur=$root/target/ab/bin/cur
  else
    (cd "$base" && cargo build --release -q -p compare --bin bench)
    cargo build --release -q -p compare --bin bench
    bin_base=$root/$base/target/release/bench
    bin_cur=$root/target/release/bench
  fi
  rm -rf "$runs" && mkdir -p "$runs"
  for i in $(seq 1 "${AB_ALTERNATIONS:-5}"); do
    # Swap the order every alternation to cancel position bias.
    if (( i % 2 )); then order="base cur"; else order="cur base"; fi
    for tree in $order; do
      bin=bin_$tree
      "${!bin}" --rounds 3 --impl ours "$@" --tsv "$runs/$tree-$i.tsv" 2>"$runs/$tree-$i.log" \
        || { cat "$runs/$tree-$i.log" >&2; exit 1; }
    done
    echo "alternation $i done" >&2
  done
  awk -F'\t' '
    FNR == 1 { next }
    {
      n = split(FILENAME, path, "/"); split(path[n], name, "-"); tree = name[1]
      key = $1 SUBSEP $2
      if (!(key in seen)) { seen[key] = 1; order[++count] = key }
      if ($7 > best[tree, key]) best[tree, key] = $7
    }
    END {
      printf "%-24s %-8s %10s %10s %8s\n", "case", "impl", "base", "cur", "delta"
      for (i = 1; i <= count; i++) {
        key = order[i]; split(key, field, SUBSEP)
        base = best["base", key]; cur = best["cur", key]
        printf "%-24s %-8s %10.1f %10.1f %+7.1f%%\n", field[1], field[2], base, cur, (cur / base - 1) * 100
        logsum += log(cur / base)
      }
      printf "%-24s %-8s %10s %10s %+7.1f%%\n", "geomean", "", "", "", (exp(logsum / count) - 1) * 100
    }' "$runs"/*.tsv
  ;;
*)
  echo "usage: scripts/ab.sh snapshot | run [bench args]" >&2
  exit 2
  ;;
esac
