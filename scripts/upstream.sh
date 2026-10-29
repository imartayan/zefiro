# Sourced by the corpus scripts. Downloads into target/upstream (gitignored), once.

ZSTD_VERSION=1.5.7
# Pinned upstream commit that last changed the benchmark fixtures.
FIXTURES_URL=https://raw.githubusercontent.com/physwkim/rust-zstd/541daeac9a473f61c8758bab7bc7b710279c2746/tests/fixtures
SILESIA_URL=https://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip

upstream=target/upstream
mkdir -p "$upstream"

# zstd sources (lib/, programs/, tests/): $ZSTD_SRC if set, else the v$ZSTD_VERSION release.
zstd_src() {
  if [ -n "${ZSTD_SRC:-}" ]; then echo "$ZSTD_SRC"; return; fi
  local dir=$upstream/zstd-$ZSTD_VERSION
  if [ ! -d "$dir" ]; then
    curl -fsSL "https://github.com/facebook/zstd/releases/download/v$ZSTD_VERSION/zstd-$ZSTD_VERSION.tar.gz" |
      tar -xz -C "$upstream"
  fi
  echo "$dir"
}
