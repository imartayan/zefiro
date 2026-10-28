use super::*;

const FORMATTED_PREFIX: [u8; 8] = [0x37, 0xA4, 0x30, 0xEC, 1, 2, 3, 4];

#[test]
fn input_without_the_magic_number_or_under_8_bytes_is_content() {
    for bytes in [
        &b""[..],
        b"a",
        &FORMATTED_PREFIX[..7],
        b"no magic number at all",
    ] {
        let dictionary = Dictionary::new(bytes).unwrap();
        assert_eq!(
            (dictionary.id(), dictionary.is_empty()),
            (0, bytes.is_empty())
        );
        assert!(dictionary.entropy().is_none());
        assert_eq!(&dictionary.padded()[..bytes.len()], bytes);
        assert_eq!(dictionary.padded().len(), bytes.len() + WILDCOPY_OVERLENGTH);
    }
}

#[test]
fn a_formatted_dictionary_needs_valid_tables_and_offsets() {
    // Magic, ID, then nothing or garbage where the Huffman table starts.
    assert_eq!(
        Dictionary::new(&FORMATTED_PREFIX).err(),
        Some(Error::DictionaryCorrupted)
    );
    let garbage = [&FORMATTED_PREFIX[..], &[0xFF; 40]].concat();
    assert_eq!(
        Dictionary::new(&garbage).err(),
        Some(Error::DictionaryCorrupted)
    );
    let weights_only = [&FORMATTED_PREFIX[..], &[0x80 + 1, 0x11]].concat();
    assert_eq!(
        Dictionary::new(&weights_only).err(),
        Some(Error::DictionaryCorrupted)
    );
}
