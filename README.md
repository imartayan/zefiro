# zefiro

A zstd decompressor in Rust. It decodes only (no compression), and gives the same
output and the same error codes as libzstd 1.5.7, including on invalid input. Not
supported: legacy formats (v0.1 to v0.7) and magicless frames. A few invalid inputs
(offsets beyond the window, for instance) can give a different error or garbage.

## Features

- One-shot decompression into a `Vec` (`decompress`) or into your buffer
  (`Decoder::decompress_into`).
- Streaming with `Decoder::decompress_stream`, which follows `ZSTD_decompressStream`
  call by call, and `Reader`, an `io::Read` over any `BufRead`.
- Dictionaries, formatted or raw content (`Dictionary`, `Decoder::set_dictionary`).
- A window limit for untrusted input (`Decoder::set_window_log_max`, 128 MiB by default).
- Concatenated and skippable frames, checksums, content sizes, long windows.
- `no_std` with `alloc`: disable the default `std` feature. `Reader` needs `std`.

Minimum supported Rust version: 1.89.

## Usage

```rust
use zefiro::decompress;

// A frame with one raw block: "hello".
let compressed = [
    0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
];
// The second argument bounds the size of the output.
let data = decompress(&compressed, 1 << 20).unwrap();
assert_eq!(data, b"hello");
```

Into a buffer you own, reusing the decoder (and its tables) between calls. The buffer
needs the content size of the frames, which `frame_content_size` reads from a header:

```rust
use zefiro::{Decoder, frame_content_size};

let compressed = [
    0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
];
let size = frame_content_size(&compressed).unwrap().unwrap();
let mut output = vec![0; size as usize];
let mut decoder = Decoder::new();
let written = decoder.decompress_into(&compressed, &mut output).unwrap();
assert_eq!(&output[..written], b"hello");
```

Streaming from any `BufRead`. Input ending inside a frame is `ErrorKind::UnexpectedEof`,
invalid input is `ErrorKind::InvalidData` with the `Error` as its source:

```rust
use std::io::Read;
use zefiro::Reader;

let compressed = [
    0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
];
let mut reader = Reader::new(compressed.as_slice());
let mut text = String::new();
reader.read_to_string(&mut text).unwrap();
assert_eq!(text, "hello");
```

A dictionary and a window limit, for frames read from a file:

```rust,no_run
use std::fs::File;
use std::io::{BufReader, Read};
use zefiro::{Decoder, Dictionary, Reader};

let dictionary = Dictionary::new(&std::fs::read("data.dict").unwrap()).unwrap();
let mut decoder = Decoder::new();
decoder.set_dictionary(Some(&dictionary));
decoder.set_window_log_max(24); // refuse frames with a window above 16 MiB
let file = BufReader::new(File::open("data.zst").unwrap());
let mut data = Vec::new();
Reader::with_decoder(decoder, file).read_to_end(&mut data).unwrap();
```

Without `Reader`, feed `decompress_stream` with pieces of input and drain its output.
It stops when the input is used up and the output buffer is not full:

```rust
use zefiro::Decoder;

let compressed = [
    0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
];
let mut decoder = Decoder::new();
let (mut data, mut buffer) = (Vec::new(), [0; 4]);
let mut input = &compressed[..];
loop {
    let progress = decoder.decompress_stream(input, &mut buffer).unwrap();
    data.extend_from_slice(&buffer[..progress.written]);
    input = &input[progress.consumed..];
    if input.is_empty() && progress.written < buffer.len() {
        break;
    }
}
assert_eq!(data, b"hello");
```

## How exactness is checked

libzstd is the oracle. The tests in `crates/compare` decode the same inputs with both
decoders and require the same output or the same error code, and for streaming the same
bytes consumed and written on every call, for random chunkings, window limits and
dictionaries. The inputs are valid, truncated, mutated and random frames, frames from
`decodecorpus` (every format feature), and golden files recorded from libzstd.
The fuzz targets in `fuzz/` (`decompress`, `stream`, `dict`, `round_trip`, `mutate`) run
the same comparisons under libFuzzer.

## Performance

On an Apple M1, one-shot decompression runs at 0.99x to 1.11x of libzstd on the
benchmark files (levels 1 to 22, geomean 1.02x). On an AMD Zen 3 (x86-64, libzstd with its BMI2
assembly Huffman loop) it runs at 0.96x to 1.12x on the benchmark files, with the same
verdicts.
