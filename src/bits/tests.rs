use alloc::vec::Vec;

use super::*;
use crate::testing::Rng;

// Bits in write order (first written first), then the end mark, packed LSB first.
fn encode(bits: &[bool]) -> Vec<u8> {
    let mut bytes = alloc::vec![0u8; (bits.len() + 1).div_ceil(8)];
    for (i, _) in bits.iter().enumerate().filter(|(_, bit)| **bit) {
        bytes[i / 8] |= 1 << (i % 8);
    }
    bytes[bits.len() / 8] |= 1 << (bits.len() % 8);
    bytes
}

fn random_bits(rng: &mut Rng, count: usize) -> Vec<bool> {
    (0..count).map(|_| rng.bool()).collect()
}

// Reads back `bits` in reverse in random chunks, checking each chunk, the
// statuses, and that the stream ends exactly.
fn check_reads(bits: &[bool], rng: &mut Rng) {
    let stream = encode(bits);
    let mut reader = BitReader::new(&stream).unwrap();
    let mut left = bits.len();
    while left > 0 {
        let status = reader.reload();
        assert_ne!(status, Status::Overflow);
        let chunk = (1 + rng.usize(..31)).min(left);
        let expected = bits[left - chunk..left]
            .iter()
            .rev()
            .fold(0u64, |acc, &bit| acc << 1 | u64::from(bit));
        assert_eq!(
            reader.read_bits(chunk as u32),
            expected,
            "{bits:?} chunk {chunk}"
        );
        left -= chunk;
    }
    assert!(reader.end_of_stream(), "len {} bits", bits.len());
    assert_eq!(reader.reload(), Status::Completed);
}

#[test]
fn streams_of_every_size_read_back() {
    let mut rng = Rng::with_seed(0x9E37_79B9_7F4A_7C15);
    for payload_bits in 0..300 {
        for _ in 0..4 {
            check_reads(&random_bits(&mut rng, payload_bits), &mut rng);
        }
    }
}

#[test]
fn one_to_eight_byte_streams_start_state() {
    // The end mark and the unused high bits of the container count as consumed.
    for len in 1..=8usize {
        for mark in 0..8u32 {
            let mut stream = alloc::vec![0xFF; len];
            stream[len - 1] = (1 << mark) | ((1u8 << mark) - 1);
            let reader = BitReader::new(&stream).unwrap();
            let payload_bits = (len as u32 - 1) * 8 + mark;
            assert_eq!(
                reader.bits_consumed,
                64 - payload_bits,
                "len {len} mark {mark}"
            );
            assert_eq!(reader.ptr, len.saturating_sub(8));
        }
    }
}

#[test]
fn rejects_empty_and_missing_end_mark() {
    assert!(BitReader::new(&[]).is_none());
    assert!(BitReader::new(&[0]).is_none());
    assert!(BitReader::new(&[1, 2, 3, 0]).is_none());
    assert!(BitReader::new(&[0xFF; 7]).is_some());
    let mut long = [0xAB; 20];
    long[19] = 0;
    assert!(BitReader::new(&long).is_none());
}

#[test]
fn one_byte_stream_hand_computed() {
    // 0b101: end mark at bit 2, payload bits "01": the top bit (0) is read first.
    let mut reader = BitReader::new(&[0b101]).unwrap();
    assert_eq!(reader.bits_consumed, 6 + 56);
    assert_eq!(reader.look_bits(2), 0b01);
    assert_eq!(reader.look_bits_fast(2), 0b01);
    assert_eq!(reader.read_bits(1), 0);
    assert_eq!(reader.read_bits_fast(1), 1);
    assert!(reader.end_of_stream());
    assert_eq!(reader.reload(), Status::Completed);
}

#[test]
fn reading_past_the_start_returns_what_c_returns() {
    // Payload: 7 zero bits, then 56 one bits. Container 0x80FF_FFFF_FFFF_FFFF.
    let mut stream = [0xFF; 8];
    stream[7] = 0x80;
    let mut reader = BitReader::new(&stream).unwrap();
    assert_eq!(reader.bits_consumed, 1);
    assert_eq!(reader.read_bits(31), 0x00FF_FFFF);
    assert_eq!(reader.read_bits(31), 0x7FFF_FFFF);
    reader.skip(1);
    assert!(reader.end_of_stream());
    // start = 64 - 64 - 4 wraps, masked to 60: the container's top nibble, 0x8.
    assert_eq!(reader.look_bits(4), 0x8);
    assert_eq!(reader.read_bits(4), 0x8);
    assert!(!reader.end_of_stream());
    // Overflow is sticky and leaves the container alone. Next start is -11, masked
    // to 53: container >> 53 is 0x407, low 7 bits 7.
    assert_eq!(reader.reload(), Status::Overflow);
    assert_eq!(reader.read_bits(7), 7);
    assert_eq!(reader.reload(), Status::Overflow);
}

#[test]
fn look_bits_fast_masks_shift_like_c() {
    let mut stream = [0x11; 8];
    stream[7] = 0x80;
    let mut reader = BitReader::new(&stream).unwrap();
    reader.skip(63);
    assert_eq!(reader.look_bits_fast(4), 0x8);
}

#[test]
fn reload_statuses_on_a_long_stream() {
    let stream: Vec<u8> = (1..=20).collect();
    let mut reader = BitReader::new(&stream).unwrap();
    assert_eq!((reader.ptr, reader.bits_consumed), (12, 4));
    assert_eq!(reader.reload(), Status::Unfinished);
    assert_eq!((reader.ptr, reader.bits_consumed), (12, 4));
    reader.skip(60);
    // SAFETY: 64 bits consumed.
    assert_eq!(unsafe { reader.reload_fast() }, Status::Unfinished);
    assert_eq!((reader.ptr, reader.bits_consumed), (4, 0));
    // Under 8 bytes from the start: the fast reload refuses, the cautious one moves.
    reader.skip(12);
    // SAFETY: 12 bits consumed.
    assert_eq!(unsafe { reader.reload_fast() }, Status::Overflow);
    assert_eq!(reader.reload(), Status::Unfinished);
    assert_eq!((reader.ptr, reader.bits_consumed), (3, 4));
    // Wants 5 bytes back, only 3 remain.
    reader.skip(40);
    assert_eq!(reader.reload(), Status::EndOfBuffer);
    assert_eq!((reader.ptr, reader.bits_consumed), (0, 20));
    assert_eq!(reader.reload(), Status::EndOfBuffer);
    reader.skip(44);
    assert_eq!(reader.reload(), Status::Completed);
    assert!(reader.end_of_stream());
    reader.skip(1);
    assert_eq!(reader.reload(), Status::Overflow);
    assert!(!reader.end_of_stream());
}

#[test]
fn resume_continues_a_stream_where_new_would_be() {
    let mut rng = Rng::with_seed(0xAB12);
    let bits = random_bits(&mut rng, 150);
    let stream = encode(&bits);
    let mut fresh = BitReader::new(&stream).unwrap();
    fresh.read_bits(13);
    fresh.reload();
    let mut resumed = BitReader::resume(&stream, fresh.ptr, fresh.bits_consumed);
    for chunk in [7, 20, 31, 5] {
        assert_eq!(resumed.reload(), fresh.reload());
        assert_eq!(resumed.read_bits(chunk), fresh.read_bits(chunk));
    }
}

#[test]
fn resume_may_start_below_its_stream() {
    // The stream's own bytes are the last 10; the reader is given the whole input
    // and walks down into the bytes before them, as the Huffman fast path allows.
    let input: Vec<u8> = (0..30).map(|i| (i * 37 + 5) as u8).collect();
    let mut reader = BitReader::resume(&input, 12, 0);
    assert_eq!(
        reader.read_bits(16),
        u64::from(u16::from_le_bytes([input[18], input[19]]))
    );
    reader.skip(48);
    assert_eq!(reader.reload(), Status::Unfinished);
    assert_eq!(reader.ptr, 4);
    // Down to the first byte of the input and no further.
    reader.skip(60);
    assert_eq!(reader.reload(), Status::EndOfBuffer);
    assert_eq!(reader.ptr, 0);
}

#[test]
fn skip_clamped_stops_at_the_container_size() {
    let mut reader = BitReader::resume(&[0; 16], 8, 10);
    reader.skip_clamped(5);
    assert_eq!(reader.bits_consumed, 15);
    reader.skip_clamped(60);
    assert_eq!(reader.bits_consumed, 64);
    reader.skip_clamped(3);
    assert_eq!(reader.bits_consumed, 64);
}
