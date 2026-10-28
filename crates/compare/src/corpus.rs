use crate::Rng;
use std::path::{Path, PathBuf};

const WORDS: &[&str] = &[
    "the", "of", "and", "zstd", "huffman", "literal", "match", "offset", "block", "stream",
    "window", "sequence", "code", "table", "fast", "rust", "frame", "buffer", "entropy", "state",
];

const SIZES: &[usize] = &[0, 1, 100, 4 << 10, 256 << 10, 4 << 20];

#[derive(Debug)]
pub struct Sample {
    pub name: String,
    pub data: Vec<u8>,
}

/// Deterministic samples: random, a repeated byte, word text, and literals
/// mixed with short-offset repeats, at 0 B to 4 MiB.
#[must_use]
pub fn synthetic_corpus() -> Vec<Sample> {
    let mut out = Vec::new();
    for &size in SIZES {
        let mut rng = Rng::with_seed(0x9E37_79B9_7F4A_7C15 ^ (size as u64 + 1));
        let mut push = |kind: &str, data: Vec<u8>| {
            out.push(Sample {
                name: format!("{kind}-{size}"),
                data,
            });
        };

        push("random", (0..size).map(|_| rng.u8(..)).collect());
        push("repeat", vec![b'a'; size]);

        let mut text = Vec::with_capacity(size + 16);
        while text.len() < size {
            text.extend_from_slice(WORDS[(rng.u64(..) % WORDS.len() as u64) as usize].as_bytes());
            text.push(if rng.u64(..).is_multiple_of(12) {
                b'\n'
            } else {
                b' '
            });
        }
        text.truncate(size);
        push("text", text);

        let mut mixed = Vec::with_capacity(size + 64);
        while mixed.len() < size {
            let r = rng.u64(..);
            if r.is_multiple_of(3) && mixed.len() > 8 {
                let offset = 1 + (r >> 8) as usize % mixed.len().min(64);
                for _ in 0..4 + (r >> 20) as usize % 40 {
                    mixed.push(mixed[mixed.len() - offset]);
                }
            } else {
                mixed.extend((0..=(r >> 8) % 12).map(|_| (rng.u64(..) % 40) as u8 + b'0'));
            }
        }
        mixed.truncate(size);
        push("mixed", mixed);
    }
    out
}

/// The synthetic corpus without the 4 MiB samples in debug builds.
#[must_use]
pub fn test_corpus() -> Vec<Sample> {
    let big = !cfg!(debug_assertions);
    synthetic_corpus()
        .into_iter()
        .filter(|s| big || s.data.len() <= 256 << 10)
        .collect()
}

/// Synthetic corpus plus every file under `$ZSTD_CORPUS_DIR` or `corpus/` (the
/// output of `scripts/fetch-corpus.sh`), named by their relative path.
/// `corpus/decodecorpus` is left out: it holds frames, not raw data.
#[must_use]
pub fn corpus() -> Vec<Sample> {
    let mut out = synthetic_corpus();
    let Some(dir) = corpus_dir() else {
        return out;
    };
    let mut paths = Vec::new();
    walk(&dir, &mut paths);
    paths.sort();
    for path in paths {
        let name = path
            .strip_prefix(&dir)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if !name.starts_with("decodecorpus/") {
            out.push(Sample {
                name,
                data: std::fs::read(&path).unwrap(),
            });
        }
    }
    out
}

pub(crate) fn corpus_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ZSTD_CORPUS_DIR") {
        return Some(dir.into());
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
    dir.is_dir().then_some(dir)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.file_name().unwrap().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}
