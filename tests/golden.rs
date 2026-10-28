//! Golden files: inputs in `tests/data/golden.bin` with the verdicts libzstd
//! 1.5.7 gave for them (`tests/data/golden.txt`, written by
//! `crates/compare/tests/golden_files.rs`). Needs no oracle and no file access.

use rust_zstd_decoder::Decoder;

const INPUTS: &[u8] = include_bytes!("data/golden.bin");
const MANIFEST: &str = include_str!("data/golden.txt");

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01B3)
    })
}

#[test]
fn golden_files_decode_as_libzstd_did() {
    let mut decoder = Decoder::new();
    let mut rows = 0;
    for line in MANIFEST.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<_> = line.split(' ').collect();
        let (offset, length): (usize, usize) =
            (fields[1].parse().unwrap(), fields[2].parse().unwrap());
        let capacity: usize = fields[3].parse().unwrap();
        let mut output = vec![0; capacity];
        let verdict = match decoder.decompress_into(&INPUTS[offset..offset + length], &mut output) {
            Ok(len) => format!("ok {len} {:016x}", fnv1a(&output[..len])),
            Err(error) => format!("err {error:?}"),
        };
        assert_eq!(verdict, fields[4..].join(" "), "{line}");
        rows += 1;
    }
    assert!(rows > 300, "{rows} rows");
}
