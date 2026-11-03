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

Decompress directly into a buffer you own:

```rust ignore
use zefiro::{Decoder, frame_content_size};

let compressed: &[u8] = &[/* your .zst data */];
let size = frame_content_size(compressed)?.unwrap_or(1 << 20);

let mut output = vec![0; size as usize];
let mut decoder = Decoder::new();
let written = decoder.decompress_into(compressed, &mut output)?;
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

More advanced options (custom dictionaries, window limits, manual stream stepping) are detailed in the documentation.

## Comparison with existing crates

There are several zstd crates in the Rust ecosystem, but most attempt to implement the full codec (both compression and decompression), leading to large codebases. zefiro focuses purely on decoding and doing it well: a small, auditable codebase with high performance, thoroughly tested against the C reference.

| Crate | Source LOC* | Safe | Fuzzed vs `libzstd` | Parallel Decompression |
| :--- | :---: | :---: | :---: | :---: |
| **zefiro** | **~4K** | Some unsafe (Miri checked) | ✅ | ❌ |
| [rust-zstd](https://github.com/physwkim/rust-zstd) (0.4.1) | ~19K | ❌ | ❌ | ✅ |
| [ruzstd](https://github.com/KillingSpark/zstd-rs) (0.9.1) | ~9K | ❌ | ⚠️ | ❌ |
| [zrip](https://github.com/paddor/zrip) (0.8.13) | ~13K | ✅ | ✅ | ❌ |
| [mzstd](https://github.com/M-Compressors/mzstd) (0.2.8) | ~30K | ✅ | ✅ | ❌ |

*Counted with [tokei](https://github.com/XAMPPRocky/tokei) excluding tests and `#[cfg(test)]` blocks.

⚠️ `ruzstd` fuzzes against crashes but not differentially against `libzstd`.

### Performance

Average throughput speedup against the reference C implementation (v1.5.7) across standard corpus files (levels 1 to 22, text, binaries, small frames, and dictionary datasets).

You can run the detailed benchmark with:
```sh
cargo r -r -p compare --bin bench -- --exhaustive
```

#### ARM (Apple M1, 2026-11-03)

| Mode | zefiro | rust-zstd | mzstd | zrip | ruzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | **+4.4%** | **+4.4%** | -27.3% | -29.8% | -74.8% |
| Stream | **+5.0%** | +1.6% | -29.5% | -66.6% | -72.6% |

#### x86 (AMD Zen 3, 2026-10-30)

| Mode | zefiro | rust-zstd | mzstd | zrip | ruzstd |
| :--- | :---: | :---: | :---: | :---: | :---: |
| One-shot | +3.3% | **+6.9%** | -0.6% | -28.8% | -72.7% |
| Stream | +3.2% | **+6.6%** | -3.0% | -55.2% | -67.9% |

## Acknowledgements

[libzstd](https://github.com/facebook/zstd) is the reference implementation we test and fuzz against.

We reused several optimization tricks from [rust-zstd](https://github.com/physwkim/rust-zstd).

Our testing and fuzzing architecture is modeled after [sirocco](https://github.com/srijs/sirocco), which also inspired the name "zefiro".
