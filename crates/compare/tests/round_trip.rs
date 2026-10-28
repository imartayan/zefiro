//! libzstd-compressed corpora decode to their originals with the same
//! verdicts as libzstd, at exact, larger and too small capacities.

use compare::*;
use proptest::prelude::*;
use rust_zstd_decoder::Decoder;

const LEVELS: [i32; 6] = [-7, 1, 3, 9, 19, 22];

struct Checker {
    oracle: Libzstd,
    decoder: Decoder,
    frames: usize,
}

impl Checker {
    fn new() -> Self {
        Self {
            oracle: Libzstd::new(),
            decoder: Decoder::new(),
            frames: 0,
        }
    }

    fn agree(&mut self, input: &[u8], capacity: usize, what: &str) -> Outcome {
        let theirs = self.oracle.decompress(input, capacity);
        let ours = decompress_ours(&mut self.decoder, input, capacity);
        assert_agree(&ours, &theirs, &format!("{what} (capacity {capacity})"));
        ours
    }

    /// `input` decodes to `data` at the exact size and with room to spare, and
    /// one byte less is `DstSizeTooSmall` on both sides.
    fn round_trip(&mut self, input: &[u8], data: &[u8], what: &str) {
        self.frames += 1;
        for capacity in [data.len(), data.len() + 1000] {
            match self.agree(input, capacity, what) {
                Outcome::Ok { output, .. } => assert!(output == data, "{what}: wrong output"),
                Outcome::Err(code) => panic!("{what}: {code:?}"),
            }
        }
        if !data.is_empty() {
            let outcome = self.agree(input, data.len() - 1, what);
            assert!(
                matches!(outcome, Outcome::Err(Code::DstSizeTooSmall)),
                "{what}: {outcome:?}"
            );
        }
    }
}

fn parameter_sets() -> Vec<(&'static str, Params)> {
    let base = Params::level(3);
    vec![
        (
            "checksum",
            Params {
                checksum: true,
                ..base
            },
        ),
        (
            "no content size",
            Params {
                content_size: false,
                ..base
            },
        ),
        (
            "window log 10",
            Params {
                window_log: 10,
                ..Params::level(5)
            },
        ),
        (
            "long distance",
            Params {
                long_distance: true,
                ..Params::level(7)
            },
        ),
        (
            "target block size",
            Params {
                target_cblock_size: 1500,
                ..base
            },
        ),
        (
            "raw literals",
            Params {
                literal_mode: LiteralMode::Raw,
                ..base
            },
        ),
        (
            "huffman literals",
            Params {
                literal_mode: LiteralMode::Huffman,
                ..Params::level(1)
            },
        ),
        (
            "min match 3",
            Params {
                min_match: 3,
                ..Params::level(9)
            },
        ),
    ]
}

#[test]
fn levels() {
    let mut checker = Checker::new();
    for sample in test_corpus() {
        for level in LEVELS {
            let params = Params {
                checksum: level % 2 != 0,
                content_size: level != 3,
                ..Params::level(level)
            };
            let frame = compress(&sample.data, &params);
            checker.round_trip(
                &frame,
                &sample.data,
                &format!("{} level {level}", sample.name),
            );
        }
    }
}

#[test]
fn parameter_variants() {
    let mut checker = Checker::new();
    for sample in test_corpus() {
        if !(sample.name.starts_with("text") || sample.name.starts_with("mixed"))
            || sample.data.len() < 4096
        {
            continue;
        }
        for (name, params) in parameter_sets() {
            let frame = compress(&sample.data, &params);
            checker.round_trip(&frame, &sample.data, &format!("{} {name}", sample.name));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4))]

    #[test]
    fn streamed_compression_with_flushes(
        steps in prop::collection::vec((1usize..3000, prop::bool::weighted(2.0 / 3.0)), 300),
    ) {
        let mut checker = Checker::new();
        for sample in test_corpus().iter().filter(|s| s.data.len() >= 4096) {
            for level in [1, 9] {
                let params = Params {
                    checksum: level == 9,
                    ..Params::level(level)
                };
                let frame = compress_stream(&sample.data, &params, &steps);
                checker.round_trip(
                    &frame,
                    &sample.data,
                    &format!("{} stream level {level}", sample.name),
                );
            }
        }
    }
}

#[test]
fn several_frames_and_skippable_frames() {
    let mut checker = Checker::new();
    let samples: Vec<_> = test_corpus()
        .into_iter()
        .filter(|s| s.data.len() == 100 || s.data.len() == 4096)
        .collect();
    let (mut input, mut data) = (skippable_frame(b"head", 0), Vec::new());
    for (i, sample) in samples.iter().enumerate() {
        let params = Params {
            checksum: i % 2 == 0,
            ..Params::level(1 + i as i32 % 5)
        };
        input.extend(compress(&sample.data, &params));
        input.extend(skippable_frame(
            &sample.data[..sample.data.len() / 2],
            (i % 16) as u8,
        ));
        data.extend_from_slice(&sample.data);
    }
    checker.round_trip(&input, &data, "multi-frame");
}
