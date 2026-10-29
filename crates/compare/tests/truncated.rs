//! Every prefix of valid frames (libzstd's and decodecorpus') decodes with
//! libzstd's verdict, at the exact capacity and with room to spare.

use std::path::Path;

use compare::*;
use zefiro::Decoder;

struct Checker {
    oracle: Libzstd,
    decoder: Decoder,
    compared: usize,
}

impl Checker {
    /// All prefixes of `input` when it is small, about 2000 spread over it and every
    /// one of the last 40 otherwise.
    fn prefixes(&mut self, input: &[u8], data_len: usize, what: &str) {
        let step = (input.len() / 2000).max(1);
        let tail = input.len().saturating_sub(40);
        for end in (0..=input.len()).filter(|&end| end % step == 0 || end >= tail) {
            for capacity in [data_len, data_len + 100] {
                let theirs = self.oracle.decompress(&input[..end], capacity);
                let ours = decompress_ours(&mut self.decoder, &input[..end], capacity);
                assert_agree(
                    &ours,
                    &theirs,
                    &format!("{what}: prefix {end} capacity {capacity}"),
                );
                self.compared += 1;
            }
        }
    }
}

fn sample(name: &str) -> Vec<u8> {
    test_corpus()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap()
        .data
}

#[test]
fn prefixes_of_libzstd_frames() {
    let mut checker = Checker {
        oracle: Libzstd::new(),
        decoder: Decoder::new(),
        compared: 0,
    };
    for name in [
        "text-4096",
        "mixed-4096",
        "mixed-256",
        "random-100",
        "repeat-4096",
    ] {
        let data = if name == "mixed-256" {
            sample("mixed-4096")[..300].to_vec()
        } else {
            sample(name)
        };
        for level in [-3, 1, 3, 19] {
            for checksum in [false, true] {
                let params = Params {
                    checksum,
                    ..Params::level(level)
                };
                let frame = compress(&data, &params);
                checker.prefixes(&frame, data.len(), &format!("{name} level {level}"));
            }
        }
    }
    // Many small blocks, several frames with skippable frames in between.
    let data = sample("text-262144");
    let steps: Vec<(usize, bool)> = (0..100).map(|i| (50 + i * 7, i % 3 != 0)).collect();
    let flushed = compress_stream(
        &data[..10_000],
        &Params {
            checksum: true,
            ..Params::level(3)
        },
        &steps,
    );
    checker.prefixes(&flushed, 10_000, "flushed stream");
    let multi = [
        skippable_frame(b"skip", 2),
        compress(&data[..3000], &Params::level(5)),
        skippable_frame(&[], 9),
        flushed,
    ]
    .concat();
    checker.prefixes(&multi, 3000 + 10_000, "multi-frame");
    assert!(checker.compared > 20_000, "{} compared", checker.compared);
}

#[test]
#[allow(clippy::print_stderr)]
fn prefixes_of_decodecorpus_frames() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let Ok(entries) = std::fs::read_dir(dir.join("zst-small")) else {
        eprintln!("skipped: run scripts/decodecorpus.sh");
        return;
    };
    let mut names: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    names.sort();
    let mut checker = Checker {
        oracle: Libzstd::new(),
        decoder: Decoder::new(),
        compared: 0,
    };
    for path in names.iter().step_by(25) {
        let frame = std::fs::read(path).unwrap();
        let original =
            std::fs::read(dir.join("orig-small").join(path.file_stem().unwrap())).unwrap();
        checker.prefixes(&frame, original.len(), &path.display().to_string());
    }
    assert!(checker.compared > 10_000, "{} compared", checker.compared);
}
