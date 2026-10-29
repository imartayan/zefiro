//! Valid frames with a few flipped or replaced bytes: same verdict and error
//! code as libzstd at capacities around the exact size.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use compare::*;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestRunner;
use zefiro::Decoder;

/// Cases per test: many in release builds.
const CASES: u32 = if cfg!(debug_assertions) {
    6000
} else {
    100_000
};

struct Base {
    frame: Vec<u8>,
    len: usize,
}

fn libzstd_bases() -> Vec<Base> {
    let mut bases = Vec::new();
    for sample in test_corpus() {
        if !(100..=4096).contains(&sample.data.len()) || sample.name.starts_with("repeat") {
            continue;
        }
        for level in [-5, 1, 3, 9, 19] {
            for (checksum, literal_mode) in
                [(false, LiteralMode::Auto), (true, LiteralMode::Huffman)]
            {
                let params = Params {
                    checksum,
                    literal_mode,
                    ..Params::level(level)
                };
                bases.push(Base {
                    frame: compress(&sample.data, &params),
                    len: sample.data.len(),
                });
            }
        }
    }
    let data = &test_corpus()
        .into_iter()
        .find(|s| s.name == "mixed-262144")
        .unwrap()
        .data[..20_000];
    let steps: Vec<(usize, bool)> = (0..60).map(|i| (100 + i * 9, i % 4 != 0)).collect();
    bases.push(Base {
        frame: compress_stream(data, &Params::level(3), &steps),
        len: data.len(),
    });
    bases
}

fn decodecorpus_bases() -> Vec<Base> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let Ok(entries) = std::fs::read_dir(dir.join("zst-small")) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    paths
        .iter()
        .step_by(4)
        .map(|path| Base {
            frame: std::fs::read(path).unwrap(),
            len: std::fs::metadata(dir.join("orig-small").join(path.file_stem().unwrap()))
                .unwrap()
                .len() as usize,
        })
        .collect()
}

/// Mostly in the headers and the ends of blocks, where the interesting checks are.
fn mutate(input: &mut Vec<u8>, (limit_class, at, operation, operand): (u8, Index, u8, u8)) {
    let limit = match limit_class {
        0 => input.len().min(30),
        1 => input.len().min(300),
        _ => input.len(),
    };
    let at = at.index(limit);
    match operation {
        0 | 1 => input[at] ^= 1 << (operand % 8),
        2 => input[at] = operand,
        3 => input[at] = input[at].wrapping_add(1 + operand % 3),
        _ => {
            input.remove(at);
        }
    }
}

/// Each of `required` must be the verdict of more than 50 comparisons.
fn run(bases: &[Base], required: &[&str]) {
    let state = RefCell::new((Libzstd::new(), Decoder::new(), BTreeMap::new()));
    let mutation = (0u8..4, any::<Index>(), 0u8..5, any::<u8>());
    let case = (
        0..bases.len(),
        prop::collection::vec(mutation, 1..4),
        0usize..64,
        any::<Index>(),
    );
    TestRunner::new(ProptestConfig::with_cases(CASES))
        .run(&case, |(base, mutations, around, random)| {
            let base = &bases[base];
            let mut input = base.frame.clone();
            for mutation in mutations {
                mutate(&mut input, mutation);
            }
            let capacities = [
                base.len,
                base.len + 1000,
                (base.len + around).saturating_sub(32),
                random.index(2 * base.len + 2),
            ];
            let (oracle, decoder, verdicts) = &mut *state.borrow_mut();
            for capacity in capacities {
                let theirs = oracle.decompress(&input, capacity);
                let ours = decompress_ours(decoder, &input, capacity);
                assert_agree(&ours, &theirs, &format!("capacity {capacity}"));
                let verdict = match theirs {
                    Outcome::Ok { .. } => "Ok".to_owned(),
                    Outcome::Err(code) => format!("{code:?}"),
                };
                *verdicts.entry(verdict).or_insert(0) += 1;
            }
            Ok(())
        })
        .unwrap();
    let verdicts = state.into_inner().2;
    for code in required {
        assert!(
            verdicts.get(*code).is_some_and(|&n| n > 50),
            "{code}: {verdicts:?}"
        );
    }
}

#[test]
fn corrupted_libzstd_frames() {
    let required = [
        "Ok",
        "CorruptionDetected",
        "DstSizeTooSmall",
        "SrcSizeWrong",
        "ChecksumWrong",
    ];
    run(&libzstd_bases(), &required);
}

#[test]
#[allow(clippy::print_stderr)]
fn corrupted_decodecorpus_frames() {
    let bases = decodecorpus_bases();
    if bases.is_empty() {
        eprintln!("skipped: run scripts/decodecorpus.sh");
        return;
    }
    run(&bases, &["Ok", "CorruptionDetected", "DstSizeTooSmall"]);
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// Random bytes, alone or after the magic number so that they reach frame parsing.
    #[test]
    fn random_bytes_match_libzstd(
        bytes in prop::collection::vec(any::<u8>(), 0..2000),
        with_magic in any::<bool>(),
        capacity in 0usize..70_000,
    ) {
        let input = if with_magic { [&MAGIC[..], &bytes].concat() } else { bytes };
        let theirs = Libzstd::new().decompress(&input, capacity);
        let ours = decompress_ours(&mut Decoder::new(), &input, capacity);
        assert_agree(&ours, &theirs, &format!("capacity {capacity}"));
    }
}
