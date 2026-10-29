//! Differential checks of dictionaries: loading, then one-shot decoding through every
//! libzstd entry point.

use zefiro::{Decoder, Dictionary};

use crate::{Code, Libzstd, Outcome, Params, assert_agree, compress, decompress_ours};

const MAGIC: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];

/// Compares ours with libzstd on `dictionary` (empty: none) and the frames in `input`:
/// - loading: both accept it, or both refuse it (`DictionaryCorrupted`; libzstd's
///   `ZSTD_DCtx_loadDictionary` reports `memory_allocation`, and `ZSTD_decompress_usingDict`
///   `dictionary_corrupted` once a frame starts);
/// - decoding at `capacity`: ours, `ZSTD_decompressDCtx` with the dictionary loaded and
///   `ZSTD_decompress_usingDict` all agree.
///
/// Returns libzstd's outcome, or `None` if the dictionary is refused.
pub fn compare_dictionary(
    dictionary: &[u8],
    input: &[u8],
    capacity: usize,
    what: &str,
) -> Option<Outcome> {
    let mut oracle = Libzstd::new();
    let loaded = oracle.load_dictionary(dictionary);
    let Ok(parsed) = Dictionary::new(dictionary) else {
        assert!(
            loaded.is_err(),
            "{what}: libzstd accepts a dictionary we refuse"
        );
        let frame = compress(b"x", &Params::level(1));
        let verdict = oracle.decompress_using_dict(&frame, 16, dictionary);
        assert!(
            matches!(verdict, Outcome::Err(Code::DictionaryCorrupted)),
            "{what}: usingDict says {verdict:?} for a refused dictionary"
        );
        return None;
    };
    assert!(
        loaded.is_ok(),
        "{what}: libzstd refuses a dictionary we accept: {loaded:?}"
    );
    let mut decoder = Decoder::new();
    decoder.set_dictionary(Some(&parsed));
    let ours = decompress_ours(&mut decoder, input, capacity);
    let theirs = oracle.decompress(input, capacity);
    assert_agree(&ours, &theirs, what);
    // `ZSTD_decompress_usingDict` is not the digested dictionary's path: it takes the
    // regular sequence loop where a cold digested dictionary takes the prefetching one
    // (other error codes on invalid data), and starts the history of a formatted dictionary
    // after its header, which the digested one keeps (an offset reaching the header is
    // `corruption_detected` there only).
    let using = oracle.decompress_using_dict(input, capacity, dictionary);
    let formatted = dictionary.len() >= 8 && dictionary[..4] == MAGIC;
    match (&ours, &using) {
        (Outcome::Err(_), Outcome::Err(_)) => {}
        (Outcome::Ok { .. }, Outcome::Err(Code::CorruptionDetected)) if formatted => {}
        _ => assert_agree(&ours, &using, &format!("{what} (usingDict)")),
    }
    Some(theirs)
}
