# Changelog

<!-- next-header -->
## [Unreleased]

First release: a zstd decoder in pure Rust, byte-identical to `libzstd` 1.5.7 on valid frames and differentially fuzzed against it.

- One-shot (`Decoder::decompress_into`, `decompress_into_vec`, `decompress_into_uninit`) and streaming (`Decoder::decompress_stream`, `Reader`) decompression.
- Dictionaries (`Dictionary`), window limits, checksum verification on or off.
- Multi-threaded decompression within a frame with `Decoder::set_threads`, for one-shot, `decompress_stream` and `Reader`, with the same output as serial.
- `Decoder::decompressed_bound` to check the output size of untrusted input before decoding.
- `no_std` support (with `alloc`); threads and `Reader` need the `std` feature.
