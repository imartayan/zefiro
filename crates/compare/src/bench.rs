//! Benchmark cases, timing and reports (PLAN.md section 7). `bin/bench.rs`
//! and `bench-ref` both call [`main`], the latter with extra implementations.

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
const DEFAULT_ROUNDS: usize = 10;
const MIB: f64 = (1 << 20) as f64;
// Ratios to libzstd within this band are printed without color.
const RATIO_NOISE: f64 = 0.02;
const USAGE: &str = "usage: bench [--rounds N] [--case SUBSTRING[,..]] [--impl NAME[,..]] \
                     [--file PATH.zst[,..]] [--tsv FILE] [--md] (a colored table on a terminal, TSV otherwise)";

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

#[derive(Debug)]
struct Args {
    rounds: usize,
    cases: Vec<String>,
    impls: Vec<String>,
    files: Vec<PathBuf>,
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

/// `Ours` and `LibzstdImpl` through their streaming calls.
struct OursStream(Ours);
struct LibzstdStream(LibzstdImpl);

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

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            rounds: DEFAULT_ROUNDS,
            cases: Vec::new(),
            impls: Vec::new(),
            files: Vec::new(),
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
                "--file" => args.files.extend(list().map(PathBuf::from)),
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

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        let parsed = Dictionary::new(dictionary).expect("dictionary refused");
        self.decoder.set_dictionary(Some(&parsed));
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

    fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.dctx
            .load_dictionary(dictionary)
            .expect("dictionary refused");
    }
}

impl Impl for OursStream {
    fn name(&self) -> &'static str {
        "ours-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        let Ours { decoder, buffer } = &mut self.0;
        grow(buffer, size);
        let written = stream_frame(input, &mut buffer[..size], |input, output| {
            let progress = decoder
                .decompress_stream(input, output)
                .expect("ours failed");
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

impl Impl for LibzstdStream {
    fn name(&self) -> &'static str {
        "libzstd-stream"
    }

    fn decompress(&mut self, input: &[u8], size: usize) -> &[u8] {
        let LibzstdImpl { dctx, buffer } = &mut self.0;
        grow(buffer, size);
        let written = stream_frame(input, &mut buffer[..size], |input, output| {
            dctx.stream_step(input, output).expect("libzstd failed")
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
        assert!(result.consumed + result.written > 0, "no progress");
        if result.done {
            return written;
        }
    }
}

/// Runs the benchmark described by the command line; `extra_impls` come after
/// `ours`, `libzstd` and their streaming versions.
pub fn main(extra_impls: Vec<Box<dyn Impl>>) {
    let args = Args::parse();
    let mut impls: Vec<Box<dyn Impl>> = vec![
        Box::new(Ours::new()),
        Box::new(LibzstdImpl::new()),
        Box::new(OursStream(Ours::new())),
        Box::new(LibzstdStream(LibzstdImpl::new())),
    ];
    impls.extend(extra_impls);
    for name in &args.impls {
        if !impls.iter().any(|imp| imp.name() == name) {
            let known: Vec<_> = impls.iter().map(|imp| imp.name()).collect();
            usage(&format!("unknown impl {name}, available: {known:?}"));
        }
    }
    impls.retain(|imp| args.impls.is_empty() || args.impls.iter().any(|n| n == imp.name()));

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
        print!("{}", pretty(&rows, std::env::var_os("NO_COLOR").is_none()));
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

/// One line per case (rows of a case are consecutive): MiB/s of each impl,
/// then each impl over `libzstd`.
#[must_use]
pub fn markdown(rows: &[Row]) -> String {
    let names = impl_names(rows);
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

/// Aligned table for a terminal, one group per mode (one-shot, then `-stream`):
/// the MiB/s of `ours`, then every other impl of the mode over its `libzstd`
/// (green or red outside the noise band), geomeans last.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn pretty(rows: &[Row], color: bool) -> String {
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
        let baseline = index(&format!("libzstd{suffix}"));
        let in_mode = names.iter().enumerate().filter(|&(i, name)| {
            name.ends_with("-stream") != suffix.is_empty() && Some(i) != baseline
        });
        for (i, name) in in_mode {
            let short = name.strip_suffix(suffix).unwrap_or(name);
            if short == "ours" || baseline.is_none() {
                columns.push((mode, format!("{short} MiB/s"), i, None));
            }
            if baseline.is_some() {
                columns.push((mode, format!("{short}/libzstd"), i, baseline));
            }
        }
    }
    let case_width = rows.iter().map(|r| r.case.len()).max().unwrap_or(0).max(7);
    let widths: Vec<_> = columns.iter().map(|c| c.1.len()).collect();
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
                    push_cell(&mut out, k, format!("{ratio:.2}x"), Some(ratio));
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
                push_cell(&mut out, k, format!("{ratio:.2}x"), Some(ratio));
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
    let content = zefiro::decompress(&input, usize::MAX).unwrap_or_else(|e| fail(&e));
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

fn usage(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
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
