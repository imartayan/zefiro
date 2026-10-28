//! Every frame under `corpus/decodecorpus/zst*` (scripts/decodecorpus.sh: valid
//! frames using every format feature) decodes to its original, with the same
//! verdicts as libzstd at smaller capacities.

use std::path::Path;

use compare::*;
use rust_zstd_decoder::Decoder;

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
