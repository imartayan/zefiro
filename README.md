# zefiro

[![crates.io](https://img.shields.io/crates/v/zefiro)](https://crates.io/crates/zefiro)
[![docs](https://img.shields.io/docsrs/zefiro)](https://docs.rs/zefiro)

A fast and safe zstd decoder in pure Rust.

The Rust zstd crates I found were either slower than `libzstd`, too large to audit, or not tested against it as thoroughly as I wanted, so I wrote one that is none of these: zefiro matches the C speed in about 5K lines, checked against `libzstd` on every path.
It can also decompress large frames on several threads, about 2x faster than `libzstd` with 4 threads, with the same output as on one.
It only decodes; for compression, use [zstd](https://crates.io/crates/zstd).

Every decoding path is checked for exact parity with `libzstd`.
Unsafe blocks are validated with [Miri](https://github.com/rust-lang/miri) to prevent undefined behavior, the multi-threaded pipeline with [ThreadSanitizer](https://clang.llvm.org/docs/ThreadSanitizer.html) to prevent data races, streaming consistency is verified with [proptest](https://github.com/proptest-rs/proptest), and differential fuzzing against `libzstd` ensures stability on both valid and corrupted inputs.

Output is byte-identical to `libzstd` 1.5.7 on all valid frames.
Legacy formats (v0.1 to v0.7) and magicless frames are *not* supported.

## Usage

### One-shot decompression

Decompress into a `Vec` you own, sized from the frame headers and reusable across calls:

```rust ignore
use zefiro::Decoder;

let compressed: &[u8] = &[/* your .zst data */];

let mut output = Vec::new();
let mut decoder = Decoder::new();
decoder.set_threads(0); // decode with all available cores
let written = decoder.decompress_into_vec(compressed, &mut output)?;
```

### Streaming

Wrap any `BufRead` with `Reader`:

```rust ignore
use std::fs::File;
use std::io::{BufReader, Read};
use zefiro::{Decoder, Reader};

let file = BufReader::new(File::open("data.zst")?);
let mut decoder = Decoder::new();
decoder.set_threads(0); // decode with all available cores
let mut reader = Reader::with_decoder(decoder, file);

let mut decompressed = Vec::new();
reader.read_to_end(&mut decompressed)?;
```

To decompress large frames on several threads, call `decoder.set_threads(0)` (or a specific count like `4`) before `decompress_into`, `decompress_stream` or `Reader::with_decoder(decoder, file)` (output is identical).

More advanced options (custom dictionaries, window limits, manual stream stepping) are detailed in the documentation.

## Comparison with existing crates

There are several zstd crates in the Rust ecosystem, but most attempt to implement the full codec (both compression and decompression), leading to large codebases. zefiro focuses purely on decoding and doing it well: a small, auditable codebase with high performance, thoroughly tested against the C reference.

| Crate | Source LOC* | Safe | Fuzzed vs `libzstd` | Parallel Decompression |
| :--- | :---: | :---: | :---: | :---: |
| **zefiro** | **~5K** | Some unsafe (Miri checked) | ✅ | ✅ |
| [rust-zstd](https://github.com/physwkim/rust-zstd) (0.4.1) | ~19K | ❌ | ❌ | ✅ |
| [ruzstd](https://github.com/KillingSpark/zstd-rs) (0.9.1) | ~7K | ❌ | ⚠️ | ❌ |
| [zrip](https://github.com/paddor/zrip) (0.8.16) | ~13K | ✅ | ✅ | ❌ |
| [mzstd](https://github.com/M-Compressors/mzstd) (0.2.11) | ~30K | ✅ | ✅ | ❌ |

*Counted with [tokei](https://github.com/XAMPPRocky/tokei) excluding tests and `#[cfg(test)]` blocks.

⚠️ `ruzstd` fuzzes against crashes but not differentially against `libzstd`.

### Performance

Average throughput speedup against the reference C implementation (v1.5.7) across standard corpus files (levels 1 to 22, text, binaries, small frames, and dictionary datasets).

You can run the detailed benchmark with:
```sh
cargo r -r -p compare --bin bench -- --exhaustive
```

`--threads 4` adds the multi-threaded zefiro and rust-zstd with 4 threads, and `--large` adds larger inputs (fetched with `scripts/fetch-large-corpus.sh`).

#### ARM (Apple M1, 2026-11-06)

| Mode | zefiro | rust-zstd | ruzstd | zrip | mzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | **+5.5%** | +4.3% | -69.8% | -21.5% | -13.2% |
| Stream | **+5.9%** | +2.9% | -68.9% | -53.7% | -14.9% |
| One-shot, 4 threads | **+109.7%** | +87.6% | - | - | - |
| Stream, 4 threads | **+117.6%** | +80.1% | - | - | - |

#### x86 (AMD Zen 3, 2026-11-05)

| Mode | zefiro | rust-zstd | ruzstd | zrip | mzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | **+10.5%** | **+10.5%** | -67.1% | -16.2% | +2.5% |
| Stream | **+9.9%** | +8.8% | -64.1% | -44.3% | -1.8% |
| One-shot, 4 threads | +90.1% | **+93.6%** | - | - | - |
| Stream, 4 threads | **+80.3%** | +77.7% | - | - | - |

## Acknowledgements

Several optimization tricks and the design of the multi-threaded pipeline are adapted from [rust-zstd](https://github.com/physwkim/rust-zstd); see [OPTIMIZATIONS.md](OPTIMIZATIONS.md).

The testing and fuzzing setup is modeled after [sirocco](https://github.com/srijs/sirocco), a gzip decoder which also inspired the name "zefiro" (Italian for [zephyr](https://en.wikipedia.org/wiki/Zephyr)).
