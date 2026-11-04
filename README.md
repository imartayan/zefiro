# zefiro

A fast and safe zstd decoder in pure Rust.

The main goal of zefiro is to have a fast decoder, on par with or faster than the original C implementation, thoroughly tested and fuzzed against `libzstd`.
We focus solely on decoding and do not implement encoding.

Every decoding path is checked for exact parity with `libzstd`.
Unsafe blocks are validated with [Miri](https://github.com/rust-lang/miri) to prevent undefined behavior, streaming consistency is verified with [proptest](https://github.com/proptest-rs/proptest), and differential fuzzing against `libzstd` ensures stability on both valid and corrupted inputs.

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
let written = decoder.decompress_into_vec(compressed, &mut output)?;
```

### Streaming

Wrap any `BufRead` with `Reader`:

```rust ignore
use std::fs::File;
use std::io::{BufReader, Read};
use zefiro::Reader;

let file = BufReader::new(File::open("data.zst")?);
let mut reader = Reader::new(file);

let mut decompressed = Vec::new();
reader.read_to_end(&mut decompressed)?;
```

To decompress large frames on several threads, call `decoder.set_threads(4)` before decompressing, or pass that decoder to `Reader::with_decoder(decoder, file)` (output is identical).

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

`--mt 4` adds the multi-threaded zefiro and rust-zstd with 4 threads.

#### ARM (Apple M1, 2026-11-04)

| Mode | zefiro | rust-zstd | ruzstd | zrip | mzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | **+4.2%** | **+4.2%** | -74.7% | -29.7% | -12.0% |
| Stream | **+5.0%** | +1.2% | -72.6% | -60.2% | -16.0% |
| One-shot, 4 threads | **+82.0%** | +37.8% | - | - | - |
| Stream, 4 threads | **+52.0%** | +38.4% | - | - | - |

#### x86 (AMD Zen 3, 2026-10-30)

| Mode | zefiro | rust-zstd | ruzstd | zrip | mzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | +3.3% | **+6.9%** | -72.7% | -28.8% | -0.6% |
| Stream | +3.2% | **+6.6%** | -67.9% | -55.2% | -3.0% |

## Acknowledgements

[libzstd](https://github.com/facebook/zstd) is the reference implementation we test and fuzz against.

We reused several optimization tricks from [rust-zstd](https://github.com/physwkim/rust-zstd).

Our testing and fuzzing architecture is modeled after [sirocco](https://github.com/srijs/sirocco), which also inspired the name "zefiro".
