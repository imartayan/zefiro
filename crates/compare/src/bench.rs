//! Benchmark cases, timing and reports.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::IsTerminal as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, process};

use zefiro::{Decoder, Dictionary};

use crate::corpus::corpus_dir;
use crate::{Libzstd, Params, Rng, Step, compress, compress_with_dictionary, train_dictionary};

const CORPUS_FILES: &[&str] = &["elf_8m", "rust_src_8m", "silesia-small.tar", "words_1m"];
const LEVELS: &[i32] = &[1, 3, 9, 19, 22];
const SMALL_FRAME_SIZES: &[usize] = &[256, 1 << 10, 4 << 10];
const SMALL_LEVEL: i32 = 3;
const DICTIONARY_SIZE: usize = 64 << 10;
const DICTIONARY_SAMPLE: usize = 1 << 10;
const SMALL_SOURCE_CHUNK: usize = 1 << 20;
const GENERATED_SIZE: usize = 4 << 20;
const LONG_WINDOW_LOG: i32 = 27;
const LONG_LEVEL: i32 = 19;
// `long27` source: the corpus, then `LONG_COPIED` bytes of pieces of it copied from random places.
const LONG_COPIED: usize = 24 << 20;
const LONG_PIECE: Range<usize> = 256..4096;
// Pieces of input and output of the streaming impls (`ZSTD_DStreamInSize` and `OutSize`).
const STREAM_CHUNK: usize = 128 << 10;
// Output (and input) per call of the rust-zstd threaded streaming impl, which needs whole blocks.
pub const STREAM_MT_CHUNK: usize = 4 << 20;
const DEFAULT_THREADS: usize = 4;
const DEFAULT_ROUNDS: usize = 10;
const MIB: f64 = (1 << 20) as f64;
// Ratios to zstd within this band are printed without color.
const RATIO_NOISE: f64 = 0.02;

const DEFAULT_IMPLS: &[&str] = &[
    "zefiro",
    "zstd",
    "rust-zstd",
    "zefiro-stream",
    "zstd-stream",
    "rust-zstd-stream",
];

/// A one-shot decompressor under test, owning its context and output buffer.
pub trait Impl {
    fn name(&self) -> &'static str;
    /// Decompresses `input`, whose content is `size` bytes, and returns it.
    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8];
    /// Uses `dictionary` (empty: none) for the following frames.
    fn set_dictionary(&mut self, dictionary: &[u8]);
}

#[derive(Debug)]
pub struct Case {
    pub name: String,
    source: Arc<Vec<u8>>,
    frames: Vec<(Vec<u8>, Range<usize>)>,
    dictionary: Arc<Vec<u8>>,
}

#[derive(Debug)]
pub struct Row {
    pub case: String,
    pub imp: &'static str,
    pub content_bytes: usize,
    pub compressed_bytes: usize,
    pub frames: usize,
    pub best: Duration,
}

/// Benchmarks the decoders: a colored table on a terminal, TSV otherwise.
#[derive(Debug, clap::Parser)]
struct Args {
    /// Best of this many runs per case and impl.
    #[arg(short, long, default_value_t = DEFAULT_ROUNDS, value_parser = positive())]
    rounds: usize,
    /// Cases whose name contains one of these.
    #[arg(short, long = "case", value_delimiter = ',')]
    cases: Vec<String>,
    /// Impls to run, by name.
    #[arg(short, long = "impl", value_delimiter = ',')]
    impls: Vec<String>,
    /// Extra cases from .zst files.
    #[arg(short, long = "file", value_delimiter = ',')]
    files: Vec<PathBuf>,
    /// Write the TSV to this file.
    #[arg(short = 'o', long)]
    tsv: Option<PathBuf>,
    /// Print a Markdown table.
    #[arg(short, long = "md")]
    markdown: bool,
    /// Every impl and case.
    #[arg(short, long)]
    exhaustive: bool,
    /// Add the multi-threaded impls with this many threads.
    #[arg(short, long, value_parser = positive())]
    threads: Option<usize>,
    /// Also print the MiB/s of zefiro.
    #[arg(short, long)]
    speed: bool,
}

struct Ours {
    decoder: Decoder,
    buffer: Vec<u8>,
}

struct ZstdImpl {
    dctx: Libzstd,
    buffer: Vec<u8>,
}

/// `Ours` with `set_threads`, one-shot.
struct OursMt(Ours, &'static str);

/// `OursStream` with `set_threads`.
struct OursMtStream(OursStream, &'static str);

/// `Ours` and `ZstdImpl` through their streaming calls.
struct OursStream(Ours);
struct ZstdStream(ZstdImpl);

impl Case {
    fn new(name: String, source: Arc<Vec<u8>>, frames: Vec<(Vec<u8>, Range<usize>)>) -> Self {
        Self {
            name,
            source,
            frames,
            dictionary: Arc::default(),
        }
    }

    fn with_dictionary(mut self, dictionary: Arc<Vec<u8>>) -> Self {
        self.dictionary = dictionary;
        self
    }

    fn content_bytes(&self) -> usize {
        self.frames.iter().map(|(_, range)| range.len()).sum()
    }

    fn compressed_bytes(&self) -> usize {
        self.frames.iter().map(|(frame, _)| frame.len()).sum()
    }
}

impl Row {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn mib_per_s(&self) -> f64 {
        self.content_bytes as f64 / MIB / self.best.as_secs_f64()
    }
}

impl Ours {
    fn new() -> Self {
        Self {
            decoder: Decoder::new(),
            buffer: Vec::new(),
        }
    }
}

impl Impl for Ours {
    fn name(&self) -> &'static str {
        "zefiro"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let written = self
            .decoder
            .decompress_into(input, &mut self.buffer[..size])
            .expect("zefiro failed");
        &self.buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        let parsed = Dictionary::new(dictionary).expect("dictionary refused");
        self.decoder.set_dictionary(Some(&parsed));
    }
}

/// `"{base}{threads}{suffix}"` as a name that lives for the whole run.
fn mt_name(base: &str, threads: usize, suffix: &str) -> &'static str {
    Box::leak(format!("{base}{threads}{suffix}").into_boxed_str())
}

impl OursMt {
    fn new(threads: usize) -> Self {
        let mut ours = Ours::new();
        ours.decoder.set_threads(threads);
        Self(ours, mt_name("zefiro-mt", threads, ""))
    }
}

impl Impl for OursMt {
    fn name(&self) -> &'static str {
        self.1
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        self.0.decompress(input, size)
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.0.set_dictionary(dictionary);
    }
}

impl OursMtStream {
    fn new(threads: usize) -> Self {
        let mut ours = Ours::new();
        ours.decoder.set_threads(threads);
        Self(OursStream(ours), mt_name("zefiro-mt", threads, "-stream"))
    }
}

impl Impl for OursMtStream {
    fn name(&self) -> &'static str {
        self.1
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        self.0.decompress(input, size)
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.0.set_dictionary(dictionary);
    }
}

impl ZstdImpl {
    fn new() -> Self {
        Self {
            dctx: Libzstd::new(),
            buffer: Vec::new(),
        }
    }
}

impl Impl for ZstdImpl {
    fn name(&self) -> &'static str {
        "zstd"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let written = self
            .dctx
            .decompress_into(input, &mut self.buffer[..size])
            .expect("zstd failed");
        &self.buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dctx
            .load_dictionary(dictionary)
            .expect("dictionary refused");
    }
}

impl Impl for OursStream {
    fn name(&self) -> &'static str {
        "zefiro-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        let Ours { decoder, buffer } = &mut self.0;
        grow(buffer, size);
        let written = stream_frame(input, &mut buffer[..size], |input, output| {
            let progress = decoder
                .decompress_stream(input, output)
                .expect("zefiro failed");
            Step {
                consumed: progress.consumed,
                written: progress.written,
                done: progress.frame_done,
            }
        });
        &buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.0.set_dictionary(dictionary);
    }
}

impl Impl for ZstdStream {
    fn name(&self) -> &'static str {
        "zstd-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        let ZstdImpl { dctx, buffer } = &mut self.0;
        grow(buffer, size);
        let written = stream_frame(input, &mut buffer[..size], |input, output| {
            dctx.stream_step(input, output).expect("zstd failed")
        });
        &buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.0.set_dictionary(dictionary);
    }
}

/// Decodes one frame the way `zstd -d` reads a file: `STREAM_CHUNK` bytes of input and
/// room for `STREAM_CHUNK` bytes of output per call, the output going straight into place.
fn stream_frame(
    input: &[u8],
    output: &mut [u8],
    mut step: impl FnMut(&[u8], &mut [u8]) -> Step,
) -> usize {
    let (mut consumed, mut written) = (0, 0);
    loop {
        let input_end = input.len().min(consumed + STREAM_CHUNK);
        let output_end = output.len().min(written + STREAM_CHUNK);
        let result = step(
            &input[consumed..input_end],
            &mut output[written..output_end],
        );
        consumed += result.consumed;
        written += result.written;
        if result.done {
            return written;
        }
        assert!(result.consumed + result.written > 0, "no progress");
    }
}

struct RustZstdOneShot {
    decompressor: rust_zstd::Decompressor,
    buffer: Vec<u8>,
}

impl RustZstdOneShot {
    const fn new(decompressor: rust_zstd::Decompressor) -> Self {
        Self {
            decompressor,
            buffer: Vec::new(),
        }
    }
}

impl Impl for RustZstdOneShot {
    fn name(&self) -> &'static str {
        "rust-zstd"
    }

    fn decompress(&mut self, input: &[u8], _size: usize) -> &[u8] {
        self.decompressor
            .decompress_into(input, &mut self.buffer)
            .expect("rust-zstd failed");
        &self.buffer
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        rust_zstd_set_dictionary(&mut self.decompressor, dictionary);
    }
}

struct RustZstdStream {
    decompressor: rust_zstd::Decompressor,
    buffer: Vec<u8>,
    chunk: usize,
}

impl RustZstdStream {
    const fn new(decompressor: rust_zstd::Decompressor, chunk: usize) -> Self {
        Self {
            decompressor,
            buffer: Vec::new(),
            chunk,
        }
    }
}

impl Impl for RustZstdStream {
    fn name(&self) -> &'static str {
        "rust-zstd-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let (mut consumed, mut written) = (0, 0);
        loop {
            let input_end = input.len().min(consumed + self.chunk);
            let output_end = size.min(written + self.chunk);
            let (mut src_pos, mut dst_pos) = (0, 0);
            let hint = self
                .decompressor
                .decompress_stream(
                    &input[consumed..input_end],
                    &mut src_pos,
                    &mut self.buffer[written..output_end],
                    &mut dst_pos,
                )
                .expect("rust-zstd stream failed");
            consumed += src_pos;
            written += dst_pos;
            assert!(src_pos + dst_pos > 0, "no progress");
            if hint == 0 {
                return &self.buffer[..written];
            }
        }
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        rust_zstd_set_dictionary(&mut self.decompressor, dictionary);
    }
}

/// An impl run inside its own rayon pool (rust-zstd's pipeline, gate of 2 blocks and 32 KiB).
struct RustZstdMt<I> {
    pool: rayon::ThreadPool,
    inner: I,
    name: &'static str,
}

impl<I: Impl + Send> RustZstdMt<I> {
    fn new(threads: usize, inner: I, suffix: &str) -> Self {
        Self {
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("rayon pool"),
            inner,
            name: mt_name("rust-zstd-mt", threads, suffix),
        }
    }
}

impl<I: Impl + Send> Impl for RustZstdMt<I> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        let inner = &mut self.inner;
        self.pool.install(|| inner.decompress(input, size))
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.inner.set_dictionary(dictionary);
    }
}

// Serial whatever the input: the default options use the rayon pool of the caller.
fn rust_zstd_serial() -> rust_zstd::Decompressor {
    rust_zstd::Decompressor::with_options(&rust_zstd::decode::DecodeOptions {
        min_parallel_blocks: usize::MAX,
        ..Default::default()
    })
}

fn rust_zstd_set_dictionary(decompressor: &mut rust_zstd::Decompressor, dictionary: &[u8]) {
    let parsed = (!dictionary.is_empty())
        .then(|| rust_zstd::decode::DecodeDict::new(dictionary).expect("dictionary refused"));
    decompressor.set_dict(parsed.as_ref());
}

struct RuzstdImpl {
    decoder: ruzstd::decoding::FrameDecoder,
    buffer: Vec<u8>,
    dictionary: Vec<u8>,
}

impl RuzstdImpl {
    fn new() -> Self {
        Self {
            decoder: ruzstd::decoding::FrameDecoder::new(),
            buffer: Vec::new(),
            dictionary: Vec::new(),
        }
    }
}

impl Impl for RuzstdImpl {
    fn name(&self) -> &'static str {
        "ruzstd"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let mut empty = &[][..];
        let _ = self.decoder.reset(&mut empty);
        if !self.dictionary.is_empty()
            && let Ok(d) = ruzstd::decoding::Dictionary::decode_dict(&self.dictionary)
        {
            let _ = self.decoder.add_dict(d);
        }
        self.decoder
            .decode_all(input, &mut self.buffer[..size])
            .expect("ruzstd failed");
        &self.buffer[..size]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary.clear();
        self.dictionary.extend_from_slice(dictionary);
        self.decoder = ruzstd::decoding::FrameDecoder::new();
        if !dictionary.is_empty()
            && let Ok(d) = ruzstd::decoding::Dictionary::decode_dict(dictionary)
        {
            let _ = self.decoder.add_dict(d);
        }
    }
}

struct RuzstdStream {
    decoder: ruzstd::decoding::FrameDecoder,
    buffer: Vec<u8>,
    dictionary: Vec<u8>,
}

impl RuzstdStream {
    fn new() -> Self {
        Self {
            decoder: ruzstd::decoding::FrameDecoder::new(),
            buffer: Vec::new(),
            dictionary: Vec::new(),
        }
    }
}

impl Impl for RuzstdStream {
    fn name(&self) -> &'static str {
        "ruzstd-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        use std::io::Read;
        grow(&mut self.buffer, size);
        let mut empty = &[][..];
        let _ = self.decoder.reset(&mut empty);
        if !self.dictionary.is_empty()
            && let Ok(d) = ruzstd::decoding::Dictionary::decode_dict(&self.dictionary)
        {
            let _ = self.decoder.add_dict(d);
        }
        let mut streaming =
            ruzstd::decoding::StreamingDecoder::new_with_decoder(input, &mut self.decoder)
                .expect("ruzstd stream failed");
        let mut written = 0;
        while written < size {
            let chunk = (size - written).min(STREAM_CHUNK);
            let n = streaming
                .read(&mut self.buffer[written..written + chunk])
                .expect("ruzstd stream failed");
            if n == 0 {
                break;
            }
            written += n;
        }
        &self.buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary.clear();
        self.dictionary.extend_from_slice(dictionary);
        self.decoder = ruzstd::decoding::FrameDecoder::new();
        if !dictionary.is_empty()
            && let Ok(d) = ruzstd::decoding::Dictionary::decode_dict(dictionary)
        {
            let _ = self.decoder.add_dict(d);
        }
    }
}

struct ZripImpl {
    buffer: Vec<u8>,
    dictionary: Option<zrip::Dictionary>,
}

impl ZripImpl {
    const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            dictionary: None,
        }
    }
}

impl Impl for ZripImpl {
    fn name(&self) -> &'static str {
        "zrip"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        if let Some(ref dict) = self.dictionary {
            let res = zrip::decompress_with_dict(input, dict).expect("zrip failed");
            grow(&mut self.buffer, res.len());
            self.buffer[..res.len()].copy_from_slice(&res);
            &self.buffer[..res.len()]
        } else {
            self.buffer.clear();
            self.buffer.reserve(size);
            let written = zrip::decompress_into(input, &mut self.buffer).expect("zrip failed");
            &self.buffer[..written]
        }
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary = if dictionary.is_empty() {
            None
        } else {
            zrip::Dictionary::from_bytes(dictionary).ok()
        };
    }
}

struct ZripStream {
    buffer: Vec<u8>,
    dictionary: Option<zrip::Dictionary>,
}

impl ZripStream {
    const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            dictionary: None,
        }
    }
}

impl Impl for ZripStream {
    fn name(&self) -> &'static str {
        "zrip-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        use std::io::Read;
        grow(&mut self.buffer, size);
        if let Some(ref dict) = self.dictionary {
            let mut decoder = zrip::FrameDecoder::with_dict(input, dict.clone());
            let mut written = 0;
            let mut failed = false;
            while written < size {
                let chunk = (size - written).min(STREAM_CHUNK);
                match decoder.read(&mut self.buffer[written..written + chunk]) {
                    Ok(0) => break,
                    Ok(n) => written += n,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            if !failed && written == size {
                return &self.buffer[..written];
            }
            // zrip 0.8.13 has a bug in FrameDecoder::with_dict when window_size < dict.len():
            // it truncates decode_history to window_size instead of preserving dict history.
            let res = zrip::decompress_with_dict(input, dict).expect("zrip failed");
            grow(&mut self.buffer, res.len());
            self.buffer[..res.len()].copy_from_slice(&res);
            &self.buffer[..res.len()]
        } else {
            let mut decoder = zrip::FrameDecoder::new(input);
            let mut written = 0;
            while written < size {
                let chunk = (size - written).min(STREAM_CHUNK);
                let n = decoder
                    .read(&mut self.buffer[written..written + chunk])
                    .expect("zrip stream failed");
                if n == 0 {
                    break;
                }
                written += n;
            }
            &self.buffer[..written]
        }
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary = if dictionary.is_empty() {
            None
        } else {
            zrip::Dictionary::from_bytes(dictionary).ok()
        };
    }
}

struct MzstdImpl {
    decompressor: mzstd::Decompressor,
    buffer: Vec<u8>,
    dictionary: Option<mzstd::dictionary::DecodeDictionary>,
}

impl MzstdImpl {
    fn new() -> Self {
        Self {
            decompressor: mzstd::Decompressor::default(),
            buffer: Vec::new(),
            dictionary: None,
        }
    }
}

impl Impl for MzstdImpl {
    fn name(&self) -> &'static str {
        "mzstd"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        if let Some(ref dict) = self.dictionary {
            let written = self
                .decompressor
                .decompress_with_dictionary_to_slice(dict, input, &mut self.buffer[..size])
                .expect("mzstd failed");
            &self.buffer[..written]
        } else {
            let written = self
                .decompressor
                .decompress_to_slice(input, &mut self.buffer[..size])
                .expect("mzstd failed");
            &self.buffer[..written]
        }
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary = if dictionary.is_empty() {
            None
        } else {
            mzstd::dictionary::DecodeDictionary::new(
                dictionary,
                mzstd::dictionary::DictionaryContent::Auto,
            )
            .ok()
        };
    }
}

struct MzstdStream {
    decompressor: mzstd::Decompressor,
    buffer: Vec<u8>,
    dictionary: Option<mzstd::dictionary::DecodeDictionary>,
}

impl MzstdStream {
    fn new() -> Self {
        Self {
            decompressor: mzstd::Decompressor::default(),
            buffer: Vec::new(),
            dictionary: None,
        }
    }
}

impl Impl for MzstdStream {
    fn name(&self) -> &'static str {
        "mzstd-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        use std::io::Read;
        grow(&mut self.buffer, size);
        let mut reader = if let Some(ref dict) = self.dictionary {
            self.decompressor.reader_with_dictionary(input, dict)
        } else {
            self.decompressor.reader(input)
        };
        let mut written = 0;
        while written < size {
            let chunk = (size - written).min(STREAM_CHUNK);
            let n = reader
                .read(&mut self.buffer[written..written + chunk])
                .expect("mzstd stream failed");
            if n == 0 {
                break;
            }
            written += n;
        }
        &self.buffer[..written]
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dictionary = if dictionary.is_empty() {
            None
        } else {
            mzstd::dictionary::DecodeDictionary::new(
                dictionary,
                mzstd::dictionary::DictionaryContent::Auto,
            )
            .ok()
        };
    }
}

/// Runs the benchmark described by the command line; `extra_impls` come after
/// built-in implementations.
pub fn main(extra_impls: Vec<Box<dyn Impl>>) {
    let mut args = <Args as clap::Parser>::parse();
    let threads = args.threads.unwrap_or(DEFAULT_THREADS);
    let mut impls: Vec<Box<dyn Impl>> = vec![
        Box::new(Ours::new()),
        Box::new(ZstdImpl::new()),
        Box::new(RustZstdOneShot::new(rust_zstd_serial())),
        Box::new(RuzstdImpl::new()),
        Box::new(ZripImpl::new()),
        Box::new(MzstdImpl::new()),
        Box::new(OursStream(Ours::new())),
        Box::new(ZstdStream(ZstdImpl::new())),
        Box::new(RustZstdStream::new(rust_zstd_serial(), STREAM_CHUNK)),
        Box::new(RuzstdStream::new()),
        Box::new(ZripStream::new()),
        Box::new(MzstdStream::new()),
        Box::new(OursMt::new(threads)),
        Box::new(OursMtStream::new(threads)),
        Box::new(RustZstdMt::new(
            threads,
            RustZstdOneShot::new(rust_zstd::Decompressor::new()),
            "",
        )),
        Box::new(RustZstdMt::new(
            threads,
            RustZstdStream::new(rust_zstd::Decompressor::new(), STREAM_MT_CHUNK),
            "-stream",
        )),
    ];

    // Deduplicate in case extra_impls provides replacement implementations.
    impls.retain(|imp| !extra_impls.iter().any(|extra| extra.name() == imp.name()));
    impls.extend(extra_impls);

    // Support alias "ours" for "zefiro" and "libzstd" for "zstd".
    for name in &mut args.impls {
        if name == "ours" {
            *name = "zefiro".into();
        } else if name == "ours-stream" {
            *name = "zefiro-stream".into();
        } else if name == "libzstd" {
            *name = "zstd".into();
        } else if name == "libzstd-stream" {
            *name = "zstd-stream".into();
        }
    }

    for name in &args.impls {
        if !impls.iter().any(|imp| imp.name() == name) {
            let known: Vec<_> = impls.iter().map(|imp| imp.name()).collect();
            usage(&format!("unknown impl {name}, available: {known:?}"));
        }
    }

    if !args.impls.is_empty() {
        impls.retain(|imp| args.impls.iter().any(|n| n == imp.name()));
    } else if !args.exhaustive {
        impls.retain(|imp| {
            DEFAULT_IMPLS.contains(&imp.name())
                || (args.threads.is_some() && imp.name().contains("-mt"))
        });
    }

    // With `--file` only, the built-in cases are skipped.
    let mut cases = if args.files.is_empty() || !args.cases.is_empty() {
        cases(&args.cases)
    } else {
        Vec::new()
    };
    cases.extend(args.files.iter().map(|path| file_case(path)));
    let rows = run(&cases, &mut impls, args.rounds);
    let tsv = tsv(&rows);
    if let Some(path) = &args.tsv {
        fs::write(path, &tsv).unwrap_or_else(|e| usage(&format!("{}: {e}", path.display())));
    } else if !args.markdown && std::io::stdout().is_terminal() {
        print!(
            "{}",
            pretty(&rows, std::env::var_os("NO_COLOR").is_none(), args.speed)
        );
    } else if !args.markdown {
        print!("{tsv}");
    }
    if args.markdown {
        print!("{}", markdown(&rows));
    }
}

/// The cases whose name contains one of `filters` (all if empty). Frames of the
/// large cases are compressed once with libzstd and cached under `target/`.
#[must_use]
pub fn cases(filters: &[String]) -> Vec<Case> {
    let keep = |name: &str| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str()));
    let mut out = Vec::new();

    for file in CORPUS_FILES {
        let levels: Vec<_> = LEVELS
            .iter()
            .filter(|level| keep(&format!("{file}-l{level}")))
            .collect();
        if levels.is_empty() {
            continue;
        }
        let source = Arc::new(read_corpus_file(file));
        let hash = content_hash(&source);
        for &&level in &levels {
            let name = format!("{file}-l{level}");
            let frame = compress_cached(&name, &source, hash, &Params::level(level));
            let range = 0..source.len();
            out.push(Case::new(name, source.clone(), vec![(frame, range)]));
        }
    }

    if keep("long27") {
        let source = Arc::new(long_source());
        let hash = content_hash(&source);
        let params = Params {
            window_log: LONG_WINDOW_LOG,
            long_distance: true,
            ..Params::level(LONG_LEVEL)
        };
        let frame = compress_cached(&format!("long27-l{LONG_LEVEL}"), &source, hash, &params);
        let range = 0..source.len();
        out.push(Case::new("long27".into(), source, vec![(frame, range)]));
    }

    let small_names: Vec<_> = SMALL_FRAME_SIZES
        .iter()
        .map(|&size| (size, small_name(size)))
        .filter(|(_, name)| keep(name))
        .collect();
    if !small_names.is_empty() {
        let source = Arc::new(small_source());
        for (size, name) in small_names {
            let frames = (0..source.len() / size)
                .map(|i| {
                    let range = i * size..(i + 1) * size;
                    (
                        compress(&source[range.clone()], &Params::level(SMALL_LEVEL)),
                        range,
                    )
                })
                .collect();
            out.push(Case::new(name, source.clone(), frames));
        }
    }

    let dict_names: Vec<_> = SMALL_FRAME_SIZES
        .iter()
        .map(|&size| (size, small_name(size).replace("small", "dict")))
        .filter(|(_, name)| keep(name))
        .collect();
    if !dict_names.is_empty() {
        let source = Arc::new(small_source());
        let dictionary = Arc::new(trained_dictionary(&source));
        for (size, name) in dict_names {
            let frames = (0..source.len() / size)
                .map(|i| {
                    let range = i * size..(i + 1) * size;
                    let frame = compress_with_dictionary(
                        &source[range.clone()],
                        &Params::level(SMALL_LEVEL),
                        &dictionary,
                        true,
                    );
                    (frame.expect("dictionary refused"), range)
                })
                .collect();
            let case = Case::new(name, source.clone(), frames);
            out.push(case.with_dictionary(dictionary.clone()));
        }
    }

    let mut rng = Rng::with_seed(0x9E37_79B9_7F4A_7C15);
    let generated = [
        ("raw-4m", (0..GENERATED_SIZE).map(|_| rng.u8(..)).collect()),
        ("rle-4m", vec![b'a'; GENERATED_SIZE]),
    ];
    for (name, data) in generated {
        if keep(name) {
            let frame = compress(&data, &Params::level(SMALL_LEVEL));
            let range = 0..data.len();
            out.push(Case::new(name.into(), Arc::new(data), vec![(frame, range)]));
        }
    }
    out
}

/// Checks every impl against the content of every case, then times `rounds`
/// passes, rotating the impl order each round. Keeps the best pass.
pub fn run(cases: &[Case], impls: &mut [Box<dyn Impl>], rounds: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for case in cases {
        eprintln!("{}", case.name);
        for imp in impls.iter_mut() {
            imp.set_dictionary(&case.dictionary);
            for (frame, range) in &case.frames {
                let output = imp.decompress(frame, range.len());
                assert!(
                    output == &case.source[range.clone()],
                    "{}: {} output differs from the input",
                    case.name,
                    imp.name()
                );
            }
        }
        let mut best = vec![Duration::MAX; impls.len()];
        for round in 0..rounds {
            for k in 0..impls.len() {
                let i = (k + round) % impls.len();
                best[i] = best[i].min(time_pass(impls[i].as_mut(), case));
            }
        }
        for (imp, best) in impls.iter().zip(best) {
            rows.push(Row {
                case: case.name.clone(),
                imp: imp.name(),
                content_bytes: case.content_bytes(),
                compressed_bytes: case.compressed_bytes(),
                frames: case.frames.len(),
                best,
            });
        }
    }
    rows
}

#[must_use]
pub fn tsv(rows: &[Row]) -> String {
    let mut out =
        String::from("case\timpl\tcontent_bytes\tcompressed_bytes\tframes\tbest_ns\tmib_s\n");
    for row in rows {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{:.1}",
            row.case,
            row.imp,
            row.content_bytes,
            row.compressed_bytes,
            row.frames,
            row.best.as_nanos(),
            row.mib_per_s()
        );
    }
    out
}

/// Formats rows as a Markdown table.
///
/// One line per case: MiB/s of each impl, then each impl over its mode's `zstd`
/// baseline.
#[must_use]
pub fn markdown(rows: &[Row]) -> String {
    let names = impl_names(rows);
    let baseline_oneshot = names
        .iter()
        .position(|&name| name == "zstd")
        .or_else(|| names.iter().position(|&name| name == "libzstd"));
    let baseline_stream = names
        .iter()
        .position(|&name| name == "zstd-stream")
        .or_else(|| names.iter().position(|&name| name == "libzstd-stream"));

    let ratio_columns: Vec<(usize, String, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, &name)| {
            let is_stream = name.ends_with("-stream");
            let baseline = if is_stream {
                baseline_stream
            } else {
                baseline_oneshot
            }?;
            if i == baseline {
                return None;
            }
            let base_name = names[baseline];
            Some((i, format!("{name}/{base_name}"), baseline))
        })
        .collect();

    let mut out = format!("| case | {} |", names.join(" | "));
    let mut separator = format!("| --- |{}", " ---: |".repeat(names.len()));
    for (_, header, _) in &ratio_columns {
        let _ = write!(out, " {header} |");
        separator += " ---: |";
    }
    let _ = write!(out, "\n{separator}\n");
    for case in rows.chunk_by(|a, b| a.case == b.case) {
        let _ = write!(out, "| {} |", case[0].case);
        for row in case {
            let _ = write!(out, " {:.0} |", row.mib_per_s());
        }
        for &(i, _, baseline) in &ratio_columns {
            let ratio = case[i].mib_per_s() / case[baseline].mib_per_s();
            let _ = write!(out, " {} |", format_delta(ratio));
        }
        out.push('\n');
    }
    out
}

fn format_delta(ratio: f64) -> String {
    let pct = (ratio - 1.0) * 100.0;
    let pct = if pct.abs() < 0.05 { 0.0 } else { pct };
    format!("{pct:+.1}%")
}

/// Aligned table for a terminal.
///
/// One group per mode (one-shot, then `-stream`): each impl of the mode over
/// its `zstd` baseline (green or red outside the noise band), optionally preceded
/// by the MiB/s of `zefiro` if `speed` is true. Geomeans last.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn pretty(rows: &[Row], color: bool, speed: bool) -> String {
    let paint = |text: &str, code: &str| {
        if color && !code.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let names = impl_names(rows);
    let index = |name: &str| names.iter().position(|&n| n == name);
    // (mode, header, impl, baseline): a speed column without baseline, else a ratio.
    let mut columns = Vec::new();
    for (mode, suffix) in [("one-shot", ""), ("stream", "-stream")] {
        let baseline =
            index(&format!("zstd{suffix}")).or_else(|| index(&format!("libzstd{suffix}")));
        let in_mode = names.iter().enumerate().filter(|&(i, name)| {
            name.ends_with("-stream") != suffix.is_empty() && Some(i) != baseline
        });
        for (i, name) in in_mode {
            let short = name.strip_suffix(suffix).unwrap_or(name);
            if (short == "zefiro" || short == "ours" || baseline.is_none())
                && (speed || baseline.is_none())
            {
                columns.push((mode, format!("{short} MiB/s"), i, None));
            }
            if let Some(b) = baseline {
                let base_short = names[b].strip_suffix(suffix).unwrap_or(names[b]);
                columns.push((mode, format!("{short}/{base_short}"), i, Some(b)));
            }
        }
    }
    let case_width = rows.iter().map(|r| r.case.len()).max().unwrap_or(0).max(7);
    let widths: Vec<_> = columns.iter().map(|c| c.1.len().max(7)).collect();
    let starts_group = |k: usize| k == 0 || columns[k - 1].0 != columns[k].0;
    let gap = |k: usize| if k > 0 && starts_group(k) { 4 } else { 2 };

    let mut groups = " ".repeat(case_width);
    let mut header = paint(&format!("{:<case_width$}", "case"), "1");
    for (k, (mode, name, _, _)) in columns.iter().enumerate() {
        let gap = " ".repeat(gap(k));
        if starts_group(k) {
            let span = columns[k..].iter().take_while(|c| c.0 == *mode);
            let span_width = span.map(|c| c.1.len() + 2).sum::<usize>() - 2;
            groups.push_str(&gap);
            groups.push_str(&paint(
                &format!("{:-^span_width$}", format!(" {mode} ")),
                "1;2",
            ));
        }
        let _ = write!(header, "{gap}{}", paint(name, "1"));
    }
    let mut out = format!("{groups}\n{header}\n");

    let mut log_ratio_sums = vec![0.0; columns.len()];
    let mut case_count = 0;
    let push_cell = |out: &mut String, k: usize, text: String, ratio: Option<f64>| {
        out.push_str(&" ".repeat(gap(k) + widths[k].saturating_sub(text.len())));
        let code = match ratio {
            Some(ratio) if ratio > 1.0 + RATIO_NOISE => "32",
            Some(ratio) if ratio < 1.0 - RATIO_NOISE => "31",
            _ => "",
        };
        out.push_str(&paint(&text, code));
    };
    for case in rows.chunk_by(|a, b| a.case == b.case) {
        let speeds: Vec<_> = case.iter().map(Row::mib_per_s).collect();
        let _ = write!(out, "{:<case_width$}", case[0].case);
        for (k, &(_, _, i, baseline)) in columns.iter().enumerate() {
            match baseline {
                None => push_cell(&mut out, k, format!("{:.0}", speeds[i]), None),
                Some(b) => {
                    let ratio = speeds[i] / speeds[b];
                    log_ratio_sums[k] += ratio.ln();
                    push_cell(&mut out, k, format_delta(ratio), Some(ratio));
                }
            }
        }
        out.push('\n');
        case_count += 1;
    }
    if case_count > 1 {
        out.push_str(&paint(&format!("{:<case_width$}", "geomean"), "1;2"));
        for (k, column) in columns.iter().enumerate() {
            if column.3.is_some() {
                let ratio = (log_ratio_sums[k] / case_count as f64).exp();
                push_cell(&mut out, k, format_delta(ratio), Some(ratio));
            } else {
                push_cell(&mut out, k, String::new(), None);
            }
        }
        out.push('\n');
    }
    out
}

// A `.zst` file as one input, its content decoded once by our decoder.
fn file_case(path: &Path) -> Case {
    let fail = |e: &dyn std::fmt::Debug| usage(&format!("{}: {e:?}", path.display()));
    let input = fs::read(path).unwrap_or_else(|e| fail(&e));
    let content = zefiro::decompress(&input, usize::MAX, 1).unwrap_or_else(|e| fail(&e));
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let range = 0..content.len();
    Case::new(name, Arc::new(content), vec![(input, range)])
}

fn time_pass(imp: &mut dyn Impl, case: &Case) -> Duration {
    let start = Instant::now();
    for (frame, range) in &case.frames {
        black_box(imp.decompress(black_box(frame), range.len()));
    }
    start.elapsed()
}

fn positive() -> clap::builder::RangedU64ValueParser<usize> {
    clap::builder::RangedU64ValueParser::new().range(1..)
}

fn usage(message: &str) -> ! {
    eprintln!("{message}");
    process::exit(2);
}

// Impls in row order, from the rows of the first case.
fn impl_names(rows: &[Row]) -> Vec<&'static str> {
    rows.iter()
        .take_while(|r| r.case == rows[0].case)
        .map(|r| r.imp)
        .collect()
}

fn grow(buffer: &mut Vec<u8>, size: usize) {
    if buffer.len() < size {
        buffer.resize(size, 0);
    }
}

fn small_name(size: usize) -> String {
    match size {
        s if s >= 1 << 10 => format!("small-{}k", s >> 10),
        s => format!("small-{s}"),
    }
}

fn read_corpus_file(name: &str) -> Vec<u8> {
    let dir = corpus_dir().expect("no corpus dir, run scripts/fetch-corpus.sh");
    fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}, run scripts/fetch-corpus.sh"))
}

/// The middle megabyte of each corpus file, concatenated.
fn small_source() -> Vec<u8> {
    let mut out = Vec::new();
    for file in CORPUS_FILES {
        let data = read_corpus_file(file);
        let start = data.len().saturating_sub(SMALL_SOURCE_CHUNK) / 2;
        out.extend_from_slice(&data[start..data.len().min(start + SMALL_SOURCE_CHUNK)]);
    }
    out
}

fn long_source() -> Vec<u8> {
    let mut out: Vec<u8> = CORPUS_FILES
        .iter()
        .flat_map(|file| read_corpus_file(file))
        .collect();
    let corpus_len = out.len();
    let mut rng = Rng::with_seed(0x1234_5678_9ABC_DEF0);
    while out.len() < corpus_len + LONG_COPIED {
        let len = rng.usize(LONG_PIECE);
        let start = rng.usize(..corpus_len - len);
        out.extend_from_within(start..start + len);
    }
    out
}

/// A dictionary trained on the 1 KiB pieces of `source` (cached like the frames).
fn trained_dictionary(source: &[u8]) -> Vec<u8> {
    let dir = std::env::var_os("ZSTD_BENCH_CACHE").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-cache"),
        PathBuf::from,
    );
    let path = dir.join(format!("dictionary-{:016x}.bin", content_hash(source)));
    if let Ok(dictionary) = fs::read(&path) {
        return dictionary;
    }
    let samples: Vec<&[u8]> = source.chunks(DICTIONARY_SAMPLE).collect();
    let dictionary = train_dictionary(&samples, DICTIONARY_SIZE).expect("training failed");
    if fs::create_dir_all(&dir)
        .and_then(|()| fs::write(&path, &dictionary))
        .is_err()
    {
        eprintln!("warning: cannot write {}", path.display());
    }
    dictionary
}

fn content_hash(data: &[u8]) -> u64 {
    data.chunks(8).fold(0xCBF2_9CE4_8422_2325, |hash, chunk| {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        (hash ^ u64::from_le_bytes(word)).wrapping_mul(0x0100_0000_01B3)
    })
}

fn compress_cached(name: &str, data: &[u8], hash: u64, params: &Params) -> Vec<u8> {
    let dir = std::env::var_os("ZSTD_BENCH_CACHE").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-cache"),
        PathBuf::from,
    );
    let path = dir.join(format!("{name}-{hash:016x}.zst"));
    if let Ok(frame) = fs::read(&path) {
        return frame;
    }
    let frame = compress(data, params);
    if fs::create_dir_all(&dir)
        .and_then(|()| fs::write(&path, &frame))
        .is_err()
    {
        eprintln!("warning: cannot write {}", path.display());
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn format_delta_examples() {
        assert_eq!(format_delta(1.02), "+2.0%");
        assert_eq!(format_delta(0.95), "-5.0%");
        assert_eq!(format_delta(1.0), "+0.0%");
        assert_eq!(format_delta(1.0002), "+0.0%");
        assert_eq!(format_delta(0.9998), "+0.0%");
        assert_eq!(format_delta(1.105), "+10.5%");
    }

    #[test]
    fn pretty_table_format() {
        let rows = vec![
            Row {
                case: "test-case".into(),
                imp: "zefiro",
                content_bytes: 1_000_000,
                compressed_bytes: 500_000,
                frames: 1,
                best: Duration::from_millis(1),
            },
            Row {
                case: "test-case".into(),
                imp: "zstd",
                content_bytes: 1_000_000,
                compressed_bytes: 500_000,
                frames: 1,
                best: Duration::from_millis(1),
            },
        ];
        let table = pretty(&rows, false, false);
        assert!(table.contains("+0.0%"));
    }
}
