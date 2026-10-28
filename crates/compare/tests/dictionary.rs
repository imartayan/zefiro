//! Dictionaries against libzstd: trained (ZDICT) and raw content ones, frames compressed
//! with them, wrong, missing and corrupted dictionaries, one-shot and streaming.

use std::sync::OnceLock;

use compare::Rng;
use compare::*;
use proptest::prelude::*;
use rust_zstd_decoder::{Decoder, Dictionary, Error};

/// Cases of the randomized tests: many in release builds.
const CASES: u32 = if cfg!(debug_assertions) { 300 } else { 10_000 };
const MAGIC_DICTIONARY: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];

struct Fixtures {
    /// Trained dictionaries (formatted, with IDs) and the documents they were trained for.
    trained: Vec<Vec<u8>>,
    /// Raw content dictionaries, including too short ones and a huge one.
    raw: Vec<Vec<u8>>,
    /// Payloads: documents of the trained family, of 40 B to 300 KB, and other data.
    payloads: Vec<Vec<u8>>,
    /// Frames of the payloads with every kind of dictionary: (dictionary, frame, payload).
    frames: Vec<Frame>,
}

struct Frame {
    name: String,
    dictionary: usize,
    bytes: Vec<u8>,
    payload: usize,
}

/// Small JSON-like records from a seeded vocabulary: what a dictionary is made for.
fn document(seed: u64, size: usize) -> Vec<u8> {
    let mut rng = Rng::with_seed(seed);
    let vocabulary: Vec<String> = (0..60)
        .map(|_| {
            (0..3 + rng.usize(..7))
                .map(|_| char::from(b'a' + rng.u8(..26)))
                .collect()
        })
        .collect();
    let mut out = Vec::new();
    while out.len() < size {
        let word = |rng: &mut Rng| vocabulary[rng.usize(..vocabulary.len())].clone();
        let sentence: Vec<String> = (0..2 + rng.usize(..9)).map(|_| word(&mut rng)).collect();
        out.extend_from_slice(
            format!(
                "{{\"id\":{},\"user\":\"{}\",\"tags\":[\"{}\",\"{}\"],\"text\":\"{}\"}}\n",
                rng.u32(..100_000),
                word(&mut rng),
                word(&mut rng),
                word(&mut rng),
                sentence.join(" "),
            )
            .as_bytes(),
        );
    }
    out.truncate(size);
    out
}

fn fixtures() -> &'static Fixtures {
    static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let mut rng = Rng::with_seed(0xD1C7);
        let source = document(1, 600_000);
        let samples: Vec<&[u8]> = (0..800)
            .map(|_| {
                let start = rng.usize(..source.len() - 1500);
                &source[start..start + 100 + rng.usize(..1400)]
            })
            .collect();
        let mut trained: Vec<Vec<u8>> = [1 << 11, 1 << 13, 1 << 16]
            .iter()
            .map(|&capacity| train_dictionary(&samples, capacity).expect("training failed"))
            .collect();
        // A dictionary of another family, and one with another ID.
        let other: Vec<u8> = document(2, 200_000);
        let other_samples: Vec<&[u8]> = other.chunks(700).collect();
        trained.push(train_dictionary(&other_samples, 1 << 12).expect("training failed"));
        let mut renamed = trained[1].clone();
        renamed[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        trained.push(renamed);
        let mut id_zero = trained[0].clone();
        id_zero[4..8].copy_from_slice(&0u32.to_le_bytes());
        trained.push(id_zero);

        let raw = vec![
            vec![],
            vec![b'a'],
            b"7 bytes".to_vec(),
            // The magic number alone: 4 bytes, then 8 bytes of content that is not a header.
            MAGIC_DICTIONARY.to_vec(),
            [&MAGIC_DICTIONARY[1..], b"12345678"].concat(),
            source[1000..3000].to_vec(),
            source[..(1 << 18) + 5000].to_vec(),
            (0..5000).map(|i| (i * 7 % 251) as u8).collect(),
        ];
        let mut payloads: Vec<Vec<u8>> = [40, 300, 1500, 9000, 70_000, 300_000]
            .iter()
            .map(|&size| document(1, size + 600_000)[600_000..].to_vec())
            .collect();
        // Within the training source (long matches into the dictionaries' content).
        payloads.push(source[2500..2500 + 20_000].to_vec());
        payloads.push(document(2, 5000));
        payloads.push((0..2000).map(|_| rng.u8(..)).collect());

        let mut frames = Vec::new();
        let dictionaries: Vec<&Vec<u8>> = trained.iter().chain(&raw).collect();
        for (dictionary, bytes) in dictionaries.iter().enumerate() {
            for (payload, data) in payloads.iter().enumerate() {
                // Few frames for the dictionaries that are not the main subjects.
                let main = dictionary == 1 || dictionary == 0 || dictionary == trained.len() + 5;
                if !main && payload % 3 != dictionary % 3 {
                    continue;
                }
                let variant = dictionary + payload;
                let params = Params {
                    checksum: variant % 2 == 0,
                    content_size: variant % 3 != 0,
                    window_log: [0, 0, 10, 12, 17][variant % 5],
                    ..Params::level([1, 3, 6, 12, 19, -3][variant % 6])
                };
                let write_id = variant % 4 != 3;
                frames.push(Frame {
                    name: format!("dictionary {dictionary} payload {payload} {params:?}"),
                    dictionary,
                    bytes: compress_with_dictionary(data, &params, bytes, write_id).unwrap(),
                    payload,
                });
            }
        }
        Fixtures {
            trained,
            raw,
            payloads,
            frames,
        }
    })
}

fn all_dictionaries() -> Vec<&'static [u8]> {
    let fixtures = fixtures();
    fixtures
        .trained
        .iter()
        .chain(&fixtures.raw)
        .map(Vec::as_slice)
        .collect()
}

#[test]
fn trained_dictionaries_are_formatted_and_used() {
    let fixtures = fixtures();
    for dictionary in &fixtures.trained {
        assert_eq!(dictionary[..4], MAGIC_DICTIONARY);
        assert!(Dictionary::new(dictionary).is_ok());
    }
    // The first payloads are smaller with the dictionary that fits: it is being used.
    let params = Params::level(3);
    for payload in &fixtures.payloads[..4] {
        let with = compress_with_dictionary(payload, &params, &fixtures.trained[1], true).unwrap();
        assert!(with.len() < compress(payload, &params).len());
    }
}

#[test]
fn frames_decode_to_their_payload_with_their_dictionary() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    for frame in &fixtures.frames {
        let payload = &fixtures.payloads[frame.payload];
        for capacity in [
            payload.len(),
            payload.len() + 100,
            payload.len().saturating_sub(1),
        ] {
            let outcome = compare_dictionary(
                dictionaries[frame.dictionary],
                &frame.bytes,
                capacity,
                &frame.name,
            )
            .unwrap();
            if capacity >= payload.len() {
                match outcome {
                    Outcome::Ok { output, .. } => {
                        assert!(output == *payload, "{}: wrong", frame.name);
                    }
                    Outcome::Err(code) => panic!("{}: {code:?}", frame.name),
                }
            }
        }
    }
}

#[test]
fn frames_with_other_dictionaries_agree() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    for (index, frame) in fixtures.frames.iter().enumerate() {
        let capacity = fixtures.payloads[frame.payload].len() + 50;
        // No dictionary, then a few others.
        for other in std::iter::once(None)
            .chain((0..4).map(|k| Some((index * 5 + k * 3) % dictionaries.len())))
        {
            let dictionary: &[u8] = other.map_or(&[], |other| dictionaries[other]);
            let what = format!("{} decoded with dictionary {other:?}", frame.name);
            compare_dictionary(dictionary, &frame.bytes, capacity, &what);
        }
    }
}

fn id_of(dictionary: &[u8]) -> u32 {
    if dictionary.len() >= 8 && dictionary[..4] == MAGIC_DICTIONARY {
        u32::from_le_bytes(dictionary[4..8].try_into().unwrap())
    } else {
        0
    }
}

#[test]
fn missing_and_wrong_dictionaries_are_dictionary_wrong() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    let mut checked = 0;
    for frame in fixtures
        .frames
        .iter()
        .filter(|f| f.dictionary < fixtures.trained.len())
    {
        // Only frames written with a dictionary ID.
        if frame.bytes[4] % 4 == 0 {
            continue;
        }
        let capacity = fixtures.payloads[frame.payload].len();
        for other in [None, Some(fixtures.trained.len() + 1), Some(3)] {
            let dictionary: &[u8] = other.map_or(&[], |other| dictionaries[other]);
            let outcome =
                compare_dictionary(dictionary, &frame.bytes, capacity, &frame.name).unwrap();
            if id_of(dictionary) != id_of(dictionaries[frame.dictionary]) {
                assert!(
                    matches!(outcome, Outcome::Err(Code::DictionaryWrong)),
                    "{}: {outcome:?}",
                    frame.name
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 20, "{checked}");
}

#[test]
fn dictionary_validation_matches_libzstd() {
    let fixtures = fixtures();
    let (mut accepted, mut refused) = (0, 0);
    // A frame to decode, so the dictionary is used when it is accepted.
    let frame = compress_with_dictionary(
        &fixtures.payloads[1],
        &Params::level(3),
        &fixtures.trained[0],
        false,
    )
    .unwrap();
    let mut check = |bytes: &[u8], what: &str| {
        if compare_dictionary(bytes, &frame, 300, what).is_some() {
            accepted += 1;
        } else {
            refused += 1;
        }
    };
    let dictionary = &fixtures.trained[0];
    for end in 0..=dictionary.len() {
        check(&dictionary[..end], &format!("prefix {end}"));
    }
    let mut rng = Rng::with_seed(7);
    for at in 0..dictionary.len().min(600) {
        for value in [0u8, 0xFF, rng.u8(..), dictionary[at] ^ 1] {
            let mut mutated = dictionary.clone();
            mutated[at] = value;
            check(&mutated, &format!("byte {at} = {value:#x}"));
        }
    }
    for _ in 0..2000 {
        let mut mutated = fixtures.trained[1 + rng.usize(..2)].clone();
        for _ in 0..=rng.usize(..4) {
            let at = rng.usize(..mutated.len());
            mutated[at] = rng.u8(..);
        }
        check(&mutated, "random mutation");
    }
    for bytes in [&MAGIC_DICTIONARY[..], &[0x37, 0xA4, 0x30, 0xEC, 1, 2, 3, 4]] {
        let expect_error = bytes.len() >= 8;
        assert_eq!(Dictionary::new(bytes).is_err(), expect_error);
        check(bytes, "magic only");
    }
    // Both outcomes happen often (content mutations are accepted, header ones refused).
    assert!(
        accepted > 100 && refused > 100,
        "{accepted} accepted, {refused} refused"
    );
    assert_eq!(Dictionary::new(&[0; 7]).map(|_| ()), Ok(()));
    assert_eq!(
        Dictionary::new(&[0x37, 0xA4, 0x30, 0xEC, 0, 0, 0, 0]).map(|_| ()),
        Err(Error::DictionaryCorrupted)
    );
}

#[test]
fn streams_with_dictionaries_agree_for_fixed_chunkings() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    let plans: [(&[usize], &[usize]); 6] = [
        (&[1 << 20], &[1 << 20]),
        (&[4096], &[100]),
        (&[64, 1, 7], &[5000, 1 << 17, 3]),
        (&[1 << 17], &[61]),
        (&[1], &[1000]),
        (&[300], &[1 << 18]),
    ];
    for frame in &fixtures.frames {
        let small = frame.bytes.len() < 5000;
        for (index, (in_sizes, out_sizes)) in plans.iter().enumerate() {
            if !small && in_sizes[0] == 1 {
                continue;
            }
            let run = compare_stream_dictionary(
                dictionaries[frame.dictionary],
                &frame.bytes,
                in_sizes,
                out_sizes,
                0,
                &frame.name,
            );
            let Outcome::Ok { output, .. } = run.outcome else {
                panic!("{} plan {index}: {:?}", frame.name, run.outcome);
            };
            assert!(
                output == fixtures.payloads[frame.payload],
                "{}: wrong",
                frame.name
            );
        }
    }
}

#[test]
fn streams_with_wrong_dictionaries_and_window_limits_agree() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    for (index, frame) in fixtures.frames.iter().enumerate() {
        for k in 0..3 {
            let other = (index + k * 4) % dictionaries.len();
            for window_log in [0, 10, 17] {
                let what = format!("{} with dictionary {other}", frame.name);
                compare_stream_dictionary(
                    dictionaries[other],
                    &frame.bytes,
                    &[3000],
                    &[4000],
                    window_log,
                    &what,
                );
            }
        }
    }
}

/// A frame of known size that is whole in the input, with output room: the single-pass
/// shortcut, which loads the dictionary through the one-shot path.
#[test]
fn stream_shortcut_with_a_dictionary() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    for frame in fixtures
        .frames
        .iter()
        .filter(|f| f.dictionary < 3 || f.dictionary >= fixtures.trained.len() + 5)
    {
        let room = fixtures.payloads[frame.payload].len() + 10;
        compare_stream_dictionary(
            dictionaries[frame.dictionary],
            &frame.bytes,
            &[1 << 20],
            &[room],
            0,
            &frame.name,
        );
        compare_stream_dictionary(
            dictionaries[frame.dictionary],
            &frame.bytes,
            &[1 << 20],
            &[room],
            10,
            &frame.name,
        );
    }
}

/// One decoder and one context reused for dictionaries and frames in a random order: no
/// table, repeat offset or history leaks from one frame or dictionary to the next.
#[test]
fn reused_decoders_agree_across_dictionaries() {
    let fixtures = fixtures();
    let dictionaries = all_dictionaries();
    let parsed: Vec<Option<Dictionary>> = dictionaries
        .iter()
        .map(|d| Dictionary::new(d).ok())
        .collect();
    let mut rng = Rng::with_seed(99);
    let mut decoder = Decoder::new();
    for step in 0..400 {
        let frame = &fixtures.frames[rng.usize(..fixtures.frames.len())];
        let which = if rng.u8(..) < 150 {
            frame.dictionary
        } else {
            rng.usize(..dictionaries.len())
        };
        let cleared = rng.u8(..) < 30;
        let dictionary: &[u8] = if cleared { &[] } else { dictionaries[which] };
        decoder.set_dictionary(if cleared {
            None
        } else {
            parsed[which].as_ref()
        });
        // A new context each time: whether libzstd takes the prefetching loop for the first
        // block depends on its memory (`ddictIsCold`), which a new context gives a fixed state.
        let mut oracle = Libzstd::new();
        oracle.load_dictionary(dictionary).unwrap();
        let capacity = fixtures.payloads[frame.payload].len() + rng.usize(..50);
        let what = format!(
            "step {step}: {} with dictionary {which} (cleared: {cleared})",
            frame.name
        );
        if rng.u8(..) < 128 {
            assert_agree(
                &decompress_ours(&mut decoder, &frame.bytes, capacity),
                &oracle.decompress(&frame.bytes, capacity),
                &what,
            );
        } else {
            let (in_len, out_len) = (1 + rng.usize(..5000), 1 + rng.usize(..5000));
            let ours = run_stream(
                |i, o| {
                    let progress = decoder.decompress_stream(i, o)?;
                    Ok(Step {
                        consumed: progress.consumed,
                        written: progress.written,
                        done: progress.frame_done,
                    })
                },
                &frame.bytes,
                &[in_len],
                &[out_len],
            );
            let theirs = run_stream(
                |i, o| oracle.stream_step(i, o),
                &frame.bytes,
                &[in_len],
                &[out_len],
            );
            assert_agree(&ours.outcome, &theirs.outcome, &what);
            assert_eq!(ours.steps, theirs.steps, "{what}: calls differ");
        }
    }
}

/// Changing the dictionary in the middle of a stream abandons the frame (libzstd refuses
/// with `stage_wrong`); the next frame decodes with the new dictionary.
#[test]
fn set_dictionary_in_the_middle_of_a_stream_restarts() {
    let fixtures = fixtures();
    let frame = fixtures
        .frames
        .iter()
        .find(|f| f.dictionary == 0 && f.bytes[4] % 4 != 0 && f.bytes.len() > 3000)
        .unwrap();
    let payload = &fixtures.payloads[frame.payload];
    let dictionary = Dictionary::new(&fixtures.trained[0]).unwrap();
    let mut decoder = Decoder::new();
    decoder.set_dictionary(Some(&dictionary));
    let mut out = vec![0; 100];
    decoder
        .decompress_stream(&frame.bytes[..frame.bytes.len() / 2], &mut out)
        .unwrap();
    decoder.set_dictionary(Some(&dictionary));
    let mut rest = vec![0; payload.len() + 10];
    let progress = decoder.decompress_stream(&frame.bytes, &mut rest).unwrap();
    assert!(progress.frame_done && rest[..progress.written] == payload[..]);
    decoder.set_dictionary(None);
    assert_eq!(
        decoder.decompress_stream(&frame.bytes, &mut rest),
        Err(Error::DictionaryWrong)
    );
}

#[derive(Debug, Clone)]
struct Case {
    frame: usize,
    dictionary: usize,
    mutations: Vec<(usize, u8)>,
    truncate: Option<usize>,
    twice: bool,
    in_size: usize,
    out_size: usize,
    window_log: u32,
}

fn cases() -> impl Strategy<Value = Case> {
    let frames = fixtures().frames.len();
    let dictionaries = all_dictionaries().len();
    (
        0..frames,
        0..dictionaries,
        prop::collection::vec((any::<usize>(), any::<u8>()), 0..4),
        prop::option::weighted(0.2, any::<usize>()),
        any::<bool>(),
        (0u32..13).prop_map(|bits| 1usize << bits),
        (0u32..13).prop_map(|bits| 1usize << bits),
        prop_oneof![Just(0), 10u32..20],
    )
        .prop_map(
            |(frame, dictionary, mutations, truncate, twice, in_size, out_size, window_log)| {
                // Mostly the dictionary the frame was made with.
                let dictionary = if dictionary % 4 == 0 {
                    dictionary
                } else {
                    fixtures().frames[frame].dictionary
                };
                Case {
                    frame,
                    dictionary,
                    mutations,
                    truncate,
                    twice,
                    in_size,
                    out_size,
                    window_log,
                }
            },
        )
}

fn mutated_input(case: &Case) -> Vec<u8> {
    let mut input = fixtures().frames[case.frame].bytes.clone();
    for &(at, value) in &case.mutations {
        let at = at % input.len();
        input[at] = value;
    }
    if let Some(end) = case.truncate {
        input.truncate(end % (input.len() + 1));
    }
    // A second frame: the dictionary is no longer cold.
    if case.twice {
        input = input.repeat(2);
    }
    input
}

/// Valid, truncated, corrupted frames with their own or another dictionary, one-shot and
/// streaming, random chunkings and window limits.
#[test]
fn random_frames_dictionaries_and_chunkings_agree() {
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: CASES,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let dictionaries = all_dictionaries();
    runner
        .run(&cases(), |case| {
            let input = mutated_input(&case);
            let dictionary = dictionaries[case.dictionary];
            let capacity = fixtures().payloads[fixtures().frames[case.frame].payload].len()
                * (1 + usize::from(case.twice))
                + case.out_size % 7;
            let what = format!("{case:?}");
            compare_dictionary(dictionary, &input, capacity, &what);
            compare_dictionary(dictionary, &input, capacity / 2, &what);
            compare_stream_dictionary(
                dictionary,
                &input,
                &[case.in_size],
                &[case.out_size],
                case.window_log,
                &what,
            );
            Ok(())
        })
        .unwrap();
}
