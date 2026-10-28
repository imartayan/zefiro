//! Benchmark cases, timing and reports (PLAN.md section 7). `bin/bench.rs`
//! and `bench-ref` both call [`main`], the latter with extra implementations.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, process};

use rust_zstd_decoder::Decoder;

use crate::corpus::corpus_dir;
use crate::{Libzstd, Params, Rng, compress};

const CORPUS_FILES: &[&str] = &["elf_8m", "rust_src_8m", "silesia-small.tar", "words_1m"];
const LEVELS: &[i32] = &[1, 3, 9, 19, 22];
const SMALL_FRAME_SIZES: &[usize] = &[256, 1 << 10, 4 << 10];
const SMALL_LEVEL: i32 = 3;
const SMALL_SOURCE_CHUNK: usize = 1 << 20;
const GENERATED_SIZE: usize = 4 << 20;
const LONG_WINDOW_LOG: i32 = 27;
const LONG_LEVEL: i32 = 19;
// `long27` source: the corpus, then `LONG_COPIED` bytes of pieces of it copied from random places.
const LONG_COPIED: usize = 24 << 20;
const LONG_PIECE: Range<usize> = 256..4096;
const DEFAULT_ROUNDS: usize = 10;
const MIB: f64 = (1 << 20) as f64;
const USAGE: &str = "usage: bench [--rounds N] [--case SUBSTRING[,..]] [--impl NAME[,..]] \
                     [--tsv FILE] [--md]";

/// A one-shot decompressor under test, owning its context and output buffer.
pub trait Impl {
    fn name(&self) -> &'static str;
    /// Decompresses `input`, whose content is `size` bytes, and returns it.
    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8];
}

#[derive(Debug)]
pub struct Case {
    pub name: String,
    source: Arc<Vec<u8>>,
    frames: Vec<(Vec<u8>, Range<usize>)>,
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

#[derive(Debug)]
struct Args {
    rounds: usize,
    cases: Vec<String>,
    impls: Vec<String>,
    tsv: Option<PathBuf>,
    markdown: bool,
}

struct Ours {
    decoder: Decoder,
    buffer: Vec<u8>,
}

struct LibzstdImpl {
    dctx: Libzstd,
    buffer: Vec<u8>,
}

impl Case {
    fn new(name: String, source: Arc<Vec<u8>>, frames: Vec<(Vec<u8>, Range<usize>)>) -> Self {
        Self {
            name,
            source,
            frames,
        }
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

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            rounds: DEFAULT_ROUNDS,
            cases: Vec::new(),
            impls: Vec::new(),
            tsv: None,
            markdown: false,
        };
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            if flag == "--md" {
                args.markdown = true;
                continue;
            }
            let Some(value) = argv.next() else {
                usage(&format!("{flag} needs a value"));
            };
            let list = || value.split(',').map(String::from);
            match flag.as_str() {
                "--rounds" => match value.parse() {
                    Ok(rounds) if rounds > 0 => args.rounds = rounds,
                    _ => usage("--rounds needs a positive number"),
                },
                "--case" => args.cases.extend(list()),
                "--impl" => args.impls.extend(list()),
                "--tsv" => args.tsv = Some(value.into()),
                _ => usage(&format!("unknown argument {flag}")),
            }
        }
        args
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
        "ours"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let written = self
            .decoder
            .decompress_into(input, &mut self.buffer[..size])
            .expect("ours failed");
        &self.buffer[..written]
    }
}

impl LibzstdImpl {
    fn new() -> Self {
        Self {
            dctx: Libzstd::new(),
            buffer: Vec::new(),
        }
    }
}

impl Impl for LibzstdImpl {
    fn name(&self) -> &'static str {
        "libzstd"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        grow(&mut self.buffer, size);
        let written = self
            .dctx
            .decompress_into(input, &mut self.buffer[..size])
            .expect("libzstd failed");
        &self.buffer[..written]
    }
}

/// Runs the benchmark described by the command line; `extra_impls` come after
/// `ours` and `libzstd`.
pub fn main(extra_impls: Vec<Box<dyn Impl>>) {
    let args = Args::parse();
    let mut impls: Vec<Box<dyn Impl>> = vec![Box::new(Ours::new()), Box::new(LibzstdImpl::new())];
    impls.extend(extra_impls);
    for name in &args.impls {
        if !impls.iter().any(|imp| imp.name() == name) {
            let known: Vec<_> = impls.iter().map(|imp| imp.name()).collect();
            usage(&format!("unknown impl {name}, available: {known:?}"));
        }
    }
    impls.retain(|imp| args.impls.is_empty() || args.impls.iter().any(|n| n == imp.name()));

    let rows = run(&cases(&args.cases), &mut impls, args.rounds);
    let tsv = tsv(&rows);
    if let Some(path) = &args.tsv {
        fs::write(path, &tsv).unwrap_or_else(|e| usage(&format!("{}: {e}", path.display())));
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

/// One line per case (rows of a case are consecutive): MiB/s of each impl,
/// then each impl over `libzstd`.
#[must_use]
pub fn markdown(rows: &[Row]) -> String {
    let names: Vec<_> = rows
        .iter()
        .take_while(|r| r.case == rows[0].case)
        .map(|r| r.imp)
        .collect();
    let baseline = names.iter().position(|&name| name == "libzstd");
    let ratio_names: Vec<_> = names
        .iter()
        .enumerate()
        .filter(|&(i, _)| Some(i) != baseline)
        .collect();

    let mut out = format!("| case | {} |", names.join(" | "));
    let mut separator = format!("| --- |{}", " ---: |".repeat(names.len()));
    if baseline.is_some() {
        for (_, name) in &ratio_names {
            let _ = write!(out, " {name}/libzstd |");
            separator += " ---: |";
        }
    }
    let _ = write!(out, "\n{separator}\n");
    for case in rows.chunk_by(|a, b| a.case == b.case) {
        let _ = write!(out, "| {} |", case[0].case);
        for row in case {
            let _ = write!(out, " {:.0} |", row.mib_per_s());
        }
        if let Some(baseline) = baseline {
            for &(i, _) in &ratio_names {
                let _ = write!(
                    out,
                    " {:.2} |",
                    case[i].mib_per_s() / case[baseline].mib_per_s()
                );
            }
        }
        out.push('\n');
    }
    out
}

fn time_pass(imp: &mut dyn Impl, case: &Case) -> Duration {
    let start = Instant::now();
    for (frame, range) in &case.frames {
        black_box(imp.decompress(black_box(frame), range.len()));
    }
    start.elapsed()
}

fn usage(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    process::exit(2);
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
