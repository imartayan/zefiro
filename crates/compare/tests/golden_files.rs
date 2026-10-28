//! Writes the golden files of the root `tests/golden.rs`: `tests/data/golden.bin`
//! (the inputs, concatenated) and `tests/data/golden.txt` with libzstd's verdict
//! for each (input, capacity). Run on demand:
//! `UPDATE_GOLDEN=1 cargo t -p compare --test golden_files`.

use std::fmt::Write as _;
use std::path::Path;

use compare::*;

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01B3)
    })
}

fn decodecorpus_frames(count: usize) -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let mut frames = Vec::new();
    for set in ["zst-small", "zst-bs8", "zst-bs12", "zst"] {
        let Ok(entries) = std::fs::read_dir(dir.join(set)) else {
            continue;
        };
        let mut paths: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
        paths.sort();
        let step = (paths.len() / count).max(1);
        for path in paths.iter().step_by(step).take(count) {
            let frame = std::fs::read(path).unwrap();
            if frame.len() <= 6000 {
                frames.push((
                    format!(
                        "decodecorpus-{set}-{}",
                        path.file_stem().unwrap().to_string_lossy()
                    ),
                    frame,
                ));
            }
        }
    }
    frames
}

#[test]
#[allow(clippy::print_stderr)]
fn regenerate_golden_files() {
    if std::env::var_os("UPDATE_GOLDEN").is_none() {
        eprintln!("skipped: set UPDATE_GOLDEN=1 to rewrite tests/data/golden*");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data");
    let samples = test_corpus();
    let sample = |name: &str| {
        samples
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .data
            .clone()
    };
    let mut inputs: Vec<(String, Vec<u8>)> = Vec::new();
    for name in ["text-4096", "mixed-4096", "random-100", "repeat-4096"] {
        for level in [-7, 1, 3, 19] {
            let params = Params {
                checksum: level % 2 != 0,
                ..Params::level(level)
            };
            inputs.push((
                format!("{name}-level{level}"),
                compress(&sample(name), &params),
            ));
        }
    }
    let mixed = sample("mixed-262144");
    let steps: Vec<(usize, bool)> = (0..80).map(|i| (50 + i * 11, i % 3 != 0)).collect();
    let flushed = compress_stream(
        &mixed[..12_000],
        &Params {
            checksum: true,
            ..Params::level(3)
        },
        &steps,
    );
    inputs.push(("mixed-flushed-blocks".into(), flushed.clone()));
    inputs.push((
        "multi-frame".into(),
        [
            skippable_frame(b"head", 0),
            compress(&sample("text-4096"), &Params::level(1)),
            skippable_frame(&[], 9),
            flushed,
        ]
        .concat(),
    ));
    inputs.extend(decodecorpus_frames(6));

    // Truncated and corrupted variants of some of them.
    let mut rng = Rng::with_seed(0x601D);
    let originals = inputs.clone();
    for (name, input) in originals
        .iter()
        .filter(|(n, _)| n.contains("level3") || n.contains("level19") || n.starts_with("decode"))
    {
        let cut = input.len() * 2 / 3;
        inputs.push((format!("{name}-truncated"), input[..cut].to_vec()));
        for variant in 0..3 {
            let mut mutated = input.clone();
            let at = rng.usize(..mutated.len());
            mutated[at] ^= 1 << (rng.u64(..) % 8);
            inputs.push((format!("{name}-flip{variant}"), mutated));
        }
    }

    let mut oracle = Libzstd::new();
    let mut manifest = String::from(
        "# name offset length capacity verdict (`ok <size> <fnv1a of output>` or `err <Error>`), libzstd 1.5.7\n",
    );
    let mut archive = Vec::new();
    for (name, input) in &inputs {
        let (offset, length) = (archive.len(), input.len());
        archive.extend_from_slice(input);
        let exact = match oracle.decompress(input, 1 << 20) {
            Outcome::Ok { output, .. } => output.len(),
            Outcome::Err(_) => 4096,
        };
        for capacity in [exact, exact + 100, exact.saturating_sub(1)] {
            let verdict = match oracle.decompress(input, capacity) {
                Outcome::Ok { output, .. } => {
                    format!("ok {} {:016x}", output.len(), fnv1a(&output))
                }
                Outcome::Err(code) => format!("err {code:?}"),
            };
            writeln!(manifest, "{name} {offset} {length} {capacity} {verdict}").unwrap();
        }
    }
    std::fs::write(root.join("golden.bin"), archive).unwrap();
    std::fs::write(root.join("golden.txt"), manifest).unwrap();
}
