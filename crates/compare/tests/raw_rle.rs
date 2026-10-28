//! Frames with only raw and RLE blocks: header, block, multi-frame, checksum
//! and capacity handling, compared with libzstd including error codes.

use compare::*;
use rust_zstd_decoder::Decoder;

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const BLOCK_SIZES: [usize; 12] = [
    0, 1, 2, 17, 100, 300, 1000, 4096, 65536, 131_072, 131_073, 200_000,
];

struct Checker {
    oracle: Libzstd,
    decoder: Decoder,
    compared: usize,
}

struct GeneratedInput {
    bytes: Vec<u8>,
    /// Total content size if every frame is valid.
    content_len: usize,
}

impl Checker {
    fn new() -> Self {
        Self {
            oracle: Libzstd::new(),
            decoder: Decoder::new(),
            compared: 0,
        }
    }

    fn check(&mut self, input: &[u8], capacity: usize, what: &str) {
        let theirs = self.oracle.decompress(input, capacity);
        let ours = decompress_ours(&mut self.decoder, input, capacity);
        assert_agree(
            &ours,
            &theirs,
            &format!("{what} (len {}, capacity {capacity})", input.len()),
        );
        self.compared += 1;
    }

    fn check_capacities(&mut self, input: &[u8], expected_len: usize, rng: &mut Rng, what: &str) {
        let random = rng.u64(..) as usize % (expected_len + 10);
        let mut capacities = vec![0, expected_len, expected_len + 1000, random];
        if expected_len > 0 {
            capacities.push(expected_len - 1);
        }
        for capacity in capacities {
            self.check(input, capacity, what);
        }
    }
}

fn one_in(rng: &mut Rng, n: u64) -> bool {
    rng.u64(..).is_multiple_of(n)
}

/// Last 4 bytes of libzstd's checksummed frame: the XXH64 of `content`.
fn checksum_of(content: &[u8]) -> [u8; 4] {
    let frame = compress(
        content,
        &Params {
            checksum: true,
            ..Params::level(1)
        },
    );
    frame[frame.len() - 4..].try_into().unwrap()
}

fn block(last: bool, rle: bool, size: usize, payload: &[u8]) -> Vec<u8> {
    let header = u32::from(last) | u32::from(rle) << 1 | (size as u32) << 3;
    [&header.to_le_bytes()[..3], payload].concat()
}

/// A frame of raw and RLE blocks with a random (sometimes wrong) header.
fn random_frame(rng: &mut Rng) -> (Vec<u8>, usize) {
    let mut content = Vec::new();
    let mut blocks = Vec::new();
    let block_count = 1 + rng.u64(..) % 4;
    for index in 0..block_count {
        let size = if one_in(rng, 4) {
            *rng.choice(&BLOCK_SIZES).unwrap()
        } else {
            *rng.choice(&BLOCK_SIZES[..7]).unwrap()
        };
        let last = index + 1 == block_count && !one_in(rng, 20);
        if one_in(rng, 2) {
            let payload: Vec<u8> = (0..size).map(|_| rng.u8(..)).collect();
            blocks.push(block(last, false, size, &payload));
            content.extend_from_slice(&payload);
        } else {
            let byte = rng.u8(..);
            blocks.push(block(last, true, size, &[byte]));
            content.resize(content.len() + size, byte);
        }
    }

    let single_segment = one_in(rng, 2);
    let mut content_size_code = rng.u64(..) % 4;
    if single_segment && content.len() > 255 {
        content_size_code = 1 + rng.u64(..) % 3;
    }
    let dict_id_code = if one_in(rng, 6) {
        1 + rng.u64(..) % 3
    } else {
        0
    };
    let checksum = one_in(rng, 2);
    let mut descriptor = (content_size_code << 6
        | u64::from(single_segment) << 5
        | u64::from(checksum) << 2
        | dict_id_code) as u8;
    if one_in(rng, 30) {
        descriptor |= 0x08;
    }
    let mut header = [&MAGIC[..], &[descriptor]].concat();
    if !single_segment {
        header.push(if one_in(rng, 12) {
            0xA0 + rng.u64(..) as u8 % 0x60
        } else {
            rng.u64(..) as u8 % 0x60
        });
    }
    let dict_id = if one_in(rng, 3) { 0 } else { rng.u64(..) };
    header.extend_from_slice(&dict_id.to_le_bytes()[..[0, 1, 2, 4][dict_id_code as usize]]);
    let mut size_field = content.len() as u64;
    if one_in(rng, 8) {
        let noise = rng.u64(..);
        size_field = size_field.wrapping_add(*rng.choice(&[1, u64::MAX, noise]).unwrap());
    }
    match content_size_code {
        0 if single_segment => header.push(size_field as u8),
        0 => {}
        1 => header.extend_from_slice(&(size_field.wrapping_sub(256) as u16).to_le_bytes()),
        2 => header.extend_from_slice(&(size_field as u32).to_le_bytes()),
        _ => header.extend_from_slice(&size_field.to_le_bytes()),
    }

    let mut frame = [header, blocks.concat()].concat();
    if checksum {
        let mut digest = checksum_of(&content).to_vec();
        match rng.u64(..) % 12 {
            0 => digest[0] ^= 1 << (rng.u64(..) % 8),
            1 => digest.truncate(rng.usize(..4)),
            _ => {}
        }
        frame.extend_from_slice(&digest);
    }
    (frame, content.len())
}

fn random_input(rng: &mut Rng) -> GeneratedInput {
    let mut bytes = Vec::new();
    let mut content_len = 0;
    for _ in 0..=rng.u64(..) % 3 {
        if one_in(rng, 3) {
            let payload = vec![rng.u8(..); rng.usize(..20)];
            bytes.extend(skippable_frame(&payload, (rng.u64(..) % 16) as u8));
        }
        let (frame, len) = random_frame(rng);
        bytes.extend(frame);
        content_len += len;
    }
    if one_in(rng, 8) {
        bytes.extend((0..rng.u64(..) % 12).map(|_| rng.u8(..)));
    }
    GeneratedInput { bytes, content_len }
}

#[test]
fn generated_frames_agree_with_libzstd() {
    let mut checker = Checker::new();
    let mut rng = Rng::with_seed(0x0123_4567_89AB_CDEF);
    for round in 0..4000 {
        let input = random_input(&mut rng);
        checker.check_capacities(
            &input.bytes,
            input.content_len,
            &mut rng,
            &format!("round {round}"),
        );
    }
    assert!(checker.compared > 15_000, "{} compared", checker.compared);
}

#[test]
fn every_prefix_of_a_frame_agrees() {
    let mut checker = Checker::new();
    let mut rng = Rng::with_seed(0xFEED_FACE_CAFE_BEEF);
    let mut frames = 0;
    while frames < 40 {
        let (frame, content_len) = random_frame(&mut rng);
        if frame.len() > 5000 {
            continue;
        }
        frames += 1;
        let input = [
            skippable_frame(b"skip", 3),
            frame.clone(),
            skippable_frame(&[], 15),
            frame,
        ]
        .concat();
        for end in 0..=input.len() {
            checker.check(
                &input[..end],
                content_len * 2 + 10,
                &format!("prefix {end}"),
            );
            checker.check(&input[..end], content_len, &format!("prefix {end}"));
        }
    }
    assert!(checker.compared > 10_000, "{} compared", checker.compared);
}

#[test]
fn mutated_frames_agree() {
    let mut checker = Checker::new();
    let mut rng = Rng::with_seed(0xDEAD_BEEF_0BAD_F00D);
    for round in 0..6000 {
        let (frame, content_len) = random_frame(&mut rng);
        let mut input = [frame.clone(), skippable_frame(&[1, 2, 3], 7), frame].concat();
        for _ in 0..=rng.u64(..) % 3 {
            // Mostly in the headers, where the interesting checks are.
            let limit = if one_in(&mut rng, 4) {
                input.len()
            } else {
                input.len().min(20)
            };
            let at = rng.usize(..limit);
            input[at] = if one_in(&mut rng, 2) {
                input[at] ^ 1 << (rng.u64(..) % 8)
            } else {
                rng.u8(..)
            };
        }
        checker.check_capacities(&input, content_len * 2, &mut rng, &format!("round {round}"));
    }
    assert!(checker.compared > 10_000, "{} compared", checker.compared);
}

#[test]
fn libzstd_frames_with_raw_blocks_decode() {
    let mut checker = Checker::new();
    let mut rng = Rng::with_seed(7);
    for sample in test_corpus() {
        for level in [-5, 1, 3, 12] {
            for (checksum, content_size) in
                [(false, true), (true, false), (true, true), (false, false)]
            {
                let params = Params {
                    checksum,
                    content_size,
                    ..Params::level(level)
                };
                let frame = compress(&sample.data, &params);
                let what = format!("{} level {level}", sample.name);
                checker.check_capacities(&frame, sample.data.len(), &mut rng, &what);
                let doubled = [frame.clone(), skippable_frame(&[0; 9], 1), frame].concat();
                checker.check_capacities(&doubled, 2 * sample.data.len(), &mut rng, &what);
            }
        }
    }
    // Random data and empty data are raw blocks at every level.
    assert!(checker.compared > 400, "{} compared", checker.compared);
}
