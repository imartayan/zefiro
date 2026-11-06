//! Golden dictionaries: libzstd 1.5.7's verdicts on loading dictionaries (its own test
//! files, trained and raw ones, invalid ones) and on decoding frames with them, recorded
//! in `tests/data/golden-dictionary.txt` by `crates/compare/tests/golden_files.rs`. Needs
//! no oracle and no file access.

#[cfg(feature = "std")]
use zefiro::Reader;
use zefiro::{Decoder, Dictionary, Error};

const ARCHIVE: &[u8] = include_bytes!("data/golden-dictionary.bin");
const MANIFEST: &str = include_str!("data/golden-dictionary.txt");

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01B3)
    })
}

fn verdict(result: Result<usize, Error>, output: &[u8]) -> String {
    match result {
        Ok(len) => format!("ok {len} {:016x}", fnv1a(&output[..len])),
        Err(error) => format!("err {error:?}"),
    }
}

#[test]
fn golden_dictionaries_load_and_decode_as_libzstd_did() {
    let mut dictionaries = std::collections::HashMap::new();
    let (mut dict_rows, mut frame_rows) = (0, 0);
    for line in MANIFEST.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<_> = line.split(' ').collect();
        if fields[0] == "dict" {
            let (offset, length): (usize, usize) =
                (fields[2].parse().unwrap(), fields[3].parse().unwrap());
            let parsed = Dictionary::new(&ARCHIVE[offset..offset + length]);
            let expected = fields[4..].join(" ");
            let got = match &parsed {
                Ok(_) => "ok".to_string(),
                Err(error) => format!("err {error:?}"),
            };
            assert_eq!(got, expected, "{line}");
            dictionaries.insert(fields[1].to_string(), parsed.ok());
            dict_rows += 1;
            continue;
        }
        let dictionary = dictionaries[fields[1]].as_ref();
        let (offset, length): (usize, usize) =
            (fields[3].parse().unwrap(), fields[4].parse().unwrap());
        let capacity: usize = fields[5].parse().unwrap();
        let input = &ARCHIVE[offset..offset + length];
        let mut decoder = Decoder::new();
        // An empty dictionary is none.
        decoder.set_dictionary(dictionary.filter(|_| fields[1] != "none"));
        let mut output = vec![0; capacity];
        let expected = fields[6..].join(" ");
        assert_eq!(
            verdict(decoder.decompress_into(input, &mut output), &output),
            expected,
            "{line}"
        );
        // The same through a stream, when the frame decodes.
        if expected.starts_with("ok") {
            let mut output = vec![0; capacity];
            let (mut consumed, mut written) = (0, 0);
            while consumed < input.len() {
                let end = input.len().min(consumed + 1000);
                let progress = decoder
                    .decompress_stream(&input[consumed..end], &mut output[written..])
                    .unwrap();
                (consumed, written) =
                    (consumed + progress.consumed(), written + progress.written());
            }
            assert_eq!(verdict(Ok(written), &output), expected, "{line} (stream)");
            // And through a `Reader` that carries the dictionary.
            #[cfg(feature = "std")]
            {
                let mut fresh = Decoder::new();
                fresh.set_dictionary(dictionary.filter(|_| fields[1] != "none"));
                let mut reader =
                    Reader::with_decoder(fresh, std::io::BufReader::with_capacity(97, input));
                let mut output = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut output).unwrap();
                output.resize(capacity, 0);
                assert_eq!(verdict(Ok(written), &output), expected, "{line} (reader)");
            }
        }
        frame_rows += 1;
    }
    assert!(
        dict_rows > 8 && frame_rows > 300,
        "{dict_rows} {frame_rows}"
    );
}
