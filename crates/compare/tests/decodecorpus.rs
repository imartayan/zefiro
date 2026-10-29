//! Every frame under `corpus/decodecorpus/zst*` (scripts/decodecorpus.sh: valid
//! frames using every format feature) decodes to its original, with the same
//! verdicts as libzstd at smaller capacities.

use std::path::Path;

use compare::*;
use zefiro::Decoder;

#[test]
#[allow(clippy::print_stderr)]
fn decodecorpus_frames_decode_to_their_originals() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let mut sets: Vec<_> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|name| name.strip_prefix("zst").map(str::to_owned))
        .collect();
    sets.sort();
    if sets.is_empty() {
        eprintln!("skipped: run scripts/decodecorpus.sh");
        return;
    }
    let (mut oracle, mut decoder) = (Libzstd::new(), Decoder::new());
    let mut rng = Rng::with_seed(0x00DE_C0DE);
    let mut frames = 0;
    for set in sets {
        let mut names: Vec<_> = std::fs::read_dir(root.join(format!("zst{set}")))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        // Every frame in release builds, a quarter in debug builds.
        let step = if cfg!(debug_assertions) { 4 } else { 1 };
        for name in names.into_iter().step_by(step) {
            let frame = std::fs::read(root.join(format!("zst{set}")).join(&name)).unwrap();
            let stem = Path::new(&name).file_stem().unwrap();
            let original = std::fs::read(root.join(format!("orig{set}")).join(stem)).unwrap();
            let what = format!("{set}/{}", name.to_string_lossy());
            let len = original.len();
            let random = rng.u64(..) as usize % (len + 1);
            for capacity in [len, len + 100, len.saturating_sub(1), random] {
                let ours = decompress_ours(&mut decoder, &frame, capacity);
                assert_agree(&ours, &oracle.decompress(&frame, capacity), &what);
                if capacity >= len {
                    match ours {
                        Outcome::Ok { output, .. } => assert!(output == original, "{what}: wrong"),
                        Outcome::Err(code) => panic!("{what}: {code:?}"),
                    }
                }
            }
            frames += 1;
        }
    }
    eprintln!("{frames} frames");
}

/// The same frames through `decompress_stream`, several chunkings, every call compared.
#[test]
#[allow(clippy::print_stderr)]
fn decodecorpus_frames_stream_like_libzstd() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let mut frames = 0;
    for entry in std::fs::read_dir(&root).into_iter().flatten() {
        let set = entry.unwrap().file_name().to_string_lossy().into_owned();
        let Some(suffix) = set.strip_prefix("zst") else {
            continue;
        };
        let mut names: Vec<_> = std::fs::read_dir(root.join(&set))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        let step = if cfg!(debug_assertions) { 4 } else { 1 };
        for (index, name) in names.into_iter().enumerate().step_by(step) {
            let frame = std::fs::read(root.join(&set).join(&name)).unwrap();
            let stem = Path::new(&name).file_stem().unwrap();
            let original = std::fs::read(root.join(format!("orig{suffix}")).join(stem)).unwrap();
            let what = format!("{set}/{}", name.to_string_lossy());
            let plans: [(&[usize], &[usize]); 4] = [
                (&[1 << 20], &[1 << 20]),
                (&[1 << 17], &[1 << 17]),
                (&[1000, 7], &[3000, 1 << 16]),
                (&[1 + index % 97], &[1 + index % 211]),
            ];
            for (in_sizes, out_sizes) in plans {
                let run = compare_stream(&frame, in_sizes, out_sizes, 0, &what);
                match run.outcome {
                    Outcome::Ok { output, .. } => assert!(output == original, "{what}: wrong"),
                    Outcome::Err(code) => panic!("{what}: {code:?}"),
                }
            }
            frames += 1;
        }
    }
    if frames == 0 {
        eprintln!("skipped: run scripts/decodecorpus.sh");
    }
}

/// The sets `corpus/decodecorpus/dict*` (`scripts/decodecorpus.sh ... --use-dict=SIZE`):
/// frames using a generated dictionary, with offsets reaching into it past the window.
/// They decode to their originals through both APIs, with the dictionary ID checked
/// (no dictionary: `DictionaryWrong`), and agree with libzstd at other capacities,
/// chunkings and window limits.
#[test]
#[allow(clippy::print_stderr)]
fn decodecorpus_frames_with_a_dictionary_decode_like_libzstd() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/decodecorpus");
    let mut sets: Vec<_> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("dict"))
        .collect();
    sets.sort();
    if sets.is_empty() {
        eprintln!("skipped: run SET=-10k scripts/decodecorpus.sh 300 1 --use-dict=10240");
    }
    let mut rng = Rng::with_seed(0xD1C7_C0DE);
    let (mut frames, mut differing) = (0, 0);
    for set in sets {
        let dictionary = std::fs::read(root.join(&set).join("zst/dictionary")).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(root.join(&set).join("zst"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".zst"))
            .collect();
        names.sort();
        let step = if cfg!(debug_assertions) { 3 } else { 1 };
        for (index, name) in names.into_iter().enumerate().step_by(step) {
            let frame = std::fs::read(root.join(&set).join("zst").join(&name)).unwrap();
            let stem = Path::new(&name).file_stem().unwrap();
            let original = std::fs::read(root.join(&set).join("orig").join(stem)).unwrap();
            let what = format!("{set}/{}", name.to_string_lossy());
            let len = original.len();
            for capacity in [len, len + 100, len.saturating_sub(1), rng.usize(..=len)] {
                let outcome = compare_dictionary(&dictionary, &frame, capacity, &what).unwrap();
                if capacity >= len {
                    match outcome {
                        Outcome::Ok { output, .. } => assert!(output == original, "{what}: wrong"),
                        Outcome::Err(code) => panic!("{what}: {code:?}"),
                    }
                }
            }
            // Another content: the dictionary is being read (outputs differ), and both agree.
            let mut other = dictionary.clone();
            other[dictionary.len() * 3 / 8..]
                .iter_mut()
                .for_each(|byte| *byte ^= 0x55);
            differing += match compare_dictionary(&other, &frame, len + 100, &what) {
                Some(Outcome::Ok { output, .. }) => usize::from(output != original),
                Some(Outcome::Err(Code::ChecksumWrong)) => 1,
                _ => 0,
            };
            let outcome = compare_dictionary(&[], &frame, len + 100, &what).unwrap();
            assert!(
                matches!(outcome, Outcome::Err(Code::DictionaryWrong)),
                "{what}"
            );
            let plans: [(&[usize], &[usize]); 4] = [
                (&[1 << 20], &[1 << 20]),
                (&[1 << 17], &[1 << 17]),
                (&[1000, 7], &[3000, 1 << 16]),
                (&[1 + index % 97], &[1 + index % 211]),
            ];
            for (window_log, (in_sizes, out_sizes)) in [0, 10, 0, 0].into_iter().zip(plans) {
                let run = compare_stream_dictionary(
                    &dictionary,
                    &frame,
                    in_sizes,
                    out_sizes,
                    window_log,
                    &what,
                );
                if window_log == 0 {
                    match run.outcome {
                        Outcome::Ok { output, .. } => assert!(output == original, "{what}: wrong"),
                        Outcome::Err(code) => panic!("{what}: {code:?}"),
                    }
                }
            }
            frames += 1;
        }
    }
    eprintln!("{frames} frames with a dictionary, {differing} read it");
    assert!(differing * 4 > frames, "the dictionary is hardly used");
}
