//! The oracle itself: it must round-trip what libzstd compresses.

use compare::*;

fn same_output(outcome: Outcome, data: &[u8], what: &str) {
    match outcome {
        Outcome::Ok { output, .. } => assert!(output == data, "{what}: output differs"),
        Outcome::Err(code) => panic!("{what}: {code:?}"),
    }
}

#[test]
fn build_is_the_expected_oracle() {
    check_oracle_build();
}

#[test]
fn round_trips_corpus() {
    let mut oracle = Libzstd::new();
    for sample in test_corpus() {
        for level in [-3, 1, 3, 9, 19] {
            let params = Params {
                checksum: level % 2 == 1,
                ..Params::level(level)
            };
            let frame = compress(&sample.data, &params);
            let what = format!("{} level {level}", sample.name);
            same_output(
                oracle.decompress(&frame, sample.data.len()),
                &sample.data,
                &what,
            );
            same_output(
                oracle.decompress_stream(&frame, 1 << 20, 1 << 17),
                &sample.data,
                &what,
            );
        }
    }
}

#[test]
fn round_trips_parameter_variants() {
    let mut oracle = Libzstd::new();
    let text = &test_corpus()
        .into_iter()
        .find(|s| s.name == "text-262144")
        .unwrap()
        .data;
    let variants = [
        Params {
            content_size: false,
            ..Params::default()
        },
        Params {
            window_log: 10,
            ..Params::level(5)
        },
        Params {
            long_distance: true,
            ..Params::level(7)
        },
        Params {
            target_cblock_size: 1500,
            ..Params::level(3)
        },
        Params {
            literal_mode: LiteralMode::Raw,
            ..Params::level(3)
        },
        Params {
            literal_mode: LiteralMode::Huffman,
            ..Params::level(1)
        },
    ];
    for (i, params) in variants.iter().enumerate() {
        let frame = compress(text, params);
        same_output(
            oracle.decompress(&frame, text.len()),
            text,
            &format!("variant {i}"),
        );
        same_output(
            oracle.decompress_stream(&frame, 100, 100),
            text,
            &format!("variant {i}"),
        );
    }
}

#[test]
fn streamed_compression_makes_many_blocks() {
    let mut oracle = Libzstd::new();
    let text = &test_corpus()
        .into_iter()
        .find(|s| s.name == "text-262144")
        .unwrap()
        .data;
    let steps: Vec<(usize, bool)> = (0..200).map(|i| (1 + i * 13, i % 3 != 0)).collect();
    let frame = compress_stream(text, &Params::default(), &steps);
    assert!(frame.len() > compress(text, &Params::default()).len());
    same_output(
        oracle.decompress(&frame, text.len()),
        text,
        "flushed stream",
    );
    same_output(
        oracle.decompress_stream(&frame, 7, 1000),
        text,
        "flushed stream",
    );
}

#[test]
fn concatenated_frames_and_skippable_frames() {
    let mut oracle = Libzstd::new();
    let mut input = skippable_frame(b"header", 0);
    let mut data = Vec::new();
    for sample in test_corpus().iter().filter(|s| s.data.len() == 100) {
        input.extend(compress(&sample.data, &Params::default()));
        input.extend(skippable_frame(&sample.data, 15));
        data.extend_from_slice(&sample.data);
    }
    same_output(oracle.decompress(&input, data.len()), &data, "multi-frame");
    same_output(
        oracle.decompress_stream(&input, 5, 64),
        &data,
        "multi-frame",
    );
}

#[test]
fn verdicts() {
    let mut oracle = Libzstd::new();
    let data = vec![b'x'; 1000];
    let frame = compress(
        &data,
        &Params {
            checksum: true,
            ..Params::default()
        },
    );

    let small = oracle.decompress(&frame, 999);
    assert!(
        matches!(small, Outcome::Err(Code::DstSizeTooSmall)),
        "{small:?}"
    );
    let truncated = oracle.decompress(&frame[..8], 1000);
    assert!(
        matches!(truncated, Outcome::Err(Code::SrcSizeWrong)),
        "{truncated:?}"
    );
    let truncated = oracle.decompress_stream(&frame[..8], 3, 3);
    assert!(
        matches!(truncated, Outcome::Err(Code::Truncated)),
        "{truncated:?}"
    );
    let no_checksum = oracle.decompress(&frame[..frame.len() - 1], 1000);
    assert!(
        matches!(no_checksum, Outcome::Err(Code::ChecksumWrong)),
        "{no_checksum:?}"
    );
    let mut bad_checksum = frame.clone();
    *bad_checksum.last_mut().unwrap() ^= 1;
    let bad = oracle.decompress(&bad_checksum, 1000);
    assert!(matches!(bad, Outcome::Err(Code::ChecksumWrong)), "{bad:?}");
    let bad = oracle.decompress_stream(&bad_checksum, 1 << 20, 1 << 20);
    assert!(matches!(bad, Outcome::Err(Code::ChecksumWrong)), "{bad:?}");
    let garbage = oracle.decompress(b"garbage garbage!", 16);
    assert!(
        matches!(garbage, Outcome::Err(Code::PrefixUnknown)),
        "{garbage:?}"
    );

    let empty = oracle.decompress(&[], 0);
    assert!(
        matches!(&empty, Outcome::Ok { output, .. } if output.is_empty()),
        "{empty:?}"
    );
    assert_agree(&empty, &oracle.decompress_stream(&[], 1, 1), "empty input");
}

#[test]
#[allow(clippy::print_stderr)]
fn decodecorpus_frames_decode_to_their_originals() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let Ok(entries) = std::fs::read_dir(dir.join("zst")) else {
        eprintln!("skipped: run scripts/decodecorpus.sh");
        return;
    };
    let mut oracle = Libzstd::new();
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let original = std::fs::read(dir.join("orig").join(path.file_stem().unwrap())).unwrap();
        let frame = std::fs::read(&path).unwrap();
        same_output(oracle.decompress(&frame, original.len()), &original, &name);
        same_output(
            oracle.decompress_stream(&frame, 4096, 1 << 14),
            &original,
            &name,
        );
    }
}
