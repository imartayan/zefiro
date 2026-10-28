use alloc::vec;
use alloc::vec::Vec;

use super::*;

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const RAW: u32 = 0;
const RLE: u32 = 1;
const WINDOW_1K: u8 = 0;
const WINDOW_128K: u8 = 0x38;

fn block(last: bool, kind: u32, size: usize, payload: &[u8]) -> Vec<u8> {
    let header = u32::from(last) | kind << 1 | (size as u32) << 3;
    [&header.to_le_bytes()[..3], payload].concat()
}

// Frame without content size, with the window given by its descriptor byte.
fn frame(window_descriptor: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
    [&MAGIC[..], &[0, window_descriptor], &blocks.concat()].concat()
}

// Single-segment frame with a 1-byte content size.
fn sized_frame(content_size: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
    [&MAGIC[..], &[0x20, content_size], &blocks.concat()].concat()
}

// Feeds `input` in pieces and drains the output in pieces: the bytes written and the calls.
fn stream(
    decoder: &mut Decoder,
    input: &[u8],
    in_chunk: usize,
    out_chunk: usize,
) -> Result<(Vec<u8>, Vec<Progress>), Error> {
    let (mut output, mut calls, mut pos) = (Vec::new(), Vec::new(), 0);
    let mut buffer = vec![0; out_chunk];
    loop {
        let end = input.len().min(pos + in_chunk);
        let progress = decoder.decompress_stream(&input[pos..end], &mut buffer)?;
        output.extend_from_slice(&buffer[..progress.written]);
        pos += progress.consumed;
        calls.push(progress);
        if pos == input.len() && progress.written < out_chunk {
            return Ok((output, calls));
        }
    }
}

// 5000 bytes in raw blocks of 700, in a frame with a 1 KiB window: the ring (3136
// bytes) wraps.
fn wrapping_frame() -> (Vec<u8>, Vec<u8>) {
    let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + i / 300) as u8).collect();
    let chunks: Vec<&[u8]> = data.chunks(700).collect();
    let blocks: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| block(i + 1 == chunks.len(), RAW, chunk.len(), chunk))
        .collect();
    (frame(WINDOW_1K, &blocks), data)
}

#[test]
fn any_chunking_gives_the_data() {
    let (input, data) = wrapping_frame();
    for in_chunk in [1, 2, 5, 699, 5000] {
        for out_chunk in [1, 3, 700, 701, 5000] {
            let mut decoder = Decoder::new();
            let (output, calls) = stream(&mut decoder, &input, in_chunk, out_chunk).unwrap();
            assert_eq!(output, data, "in {in_chunk} out {out_chunk}");
            assert!(calls.iter().any(|c| c.frame_done));
            assert_eq!(calls.iter().map(|c| c.consumed).sum::<usize>(), input.len());
        }
    }
}

#[test]
fn the_last_input_byte_is_held_back_until_the_output_is_flushed() {
    let input = frame(WINDOW_1K, &[block(true, RAW, 6, b"abcdef")]);
    let mut decoder = Decoder::new();
    let mut output = [0; 4];
    let first = decoder.decompress_stream(&input, &mut output).unwrap();
    assert_eq!(
        first,
        Progress {
            consumed: input.len() - 1,
            written: 4,
            frame_done: false
        }
    );
    // The held byte is passed again, with room for the rest.
    let second = decoder
        .decompress_stream(&input[input.len() - 1..], &mut output)
        .unwrap();
    assert_eq!(
        second,
        Progress {
            consumed: 1,
            written: 2,
            frame_done: true
        }
    );
    assert_eq!(&output[..2], b"ef");
    // With no input the held byte stays pending.
    let mut decoder = Decoder::new();
    decoder.decompress_stream(&input, &mut output).unwrap();
    let probe = decoder.decompress_stream(&[], &mut output).unwrap();
    assert_eq!(
        probe,
        Progress {
            consumed: 0,
            written: 2,
            frame_done: false
        }
    );
    let probe = decoder.decompress_stream(&[], &mut output).unwrap();
    assert_eq!(
        probe,
        Progress {
            consumed: 0,
            written: 0,
            frame_done: false
        }
    );
}

#[test]
fn frames_and_skippable_frames_follow_each_other() {
    let skippable = [&[0x5F, 0x2A, 0x4D, 0x18, 3, 0, 0, 0][..], b"xyz"].concat();
    let one = frame(WINDOW_1K, &[block(true, RAW, 3, b"abc")]);
    let two = sized_frame(2, &[block(true, RLE, 2, b"z")]);
    let input = [&skippable[..], &one, &skippable, &two, &skippable].concat();
    for chunk in [1, 4, 1000] {
        let mut decoder = Decoder::new();
        let (output, calls) = stream(&mut decoder, &input, chunk, 2).unwrap();
        assert_eq!(output, b"abczz", "chunk {chunk}");
        assert_eq!(
            calls.iter().filter(|c| c.frame_done).count(),
            5,
            "chunk {chunk}"
        );
    }
}

#[test]
fn a_frame_whole_in_the_input_and_fitting_the_output_is_decoded_in_one_pass() {
    let input = sized_frame(5, &[block(true, RAW, 5, b"hello")]);
    let mut output = [0; 5];
    let progress = Decoder::new()
        .decompress_stream(&input, &mut output)
        .unwrap();
    assert_eq!(
        progress,
        Progress {
            consumed: input.len(),
            written: 5,
            frame_done: true
        }
    );
    assert_eq!(&output, b"hello");
    // One byte of room less: the ring path, which gives the same data in two calls.
    let mut decoder = Decoder::new();
    let (output, calls) = stream(&mut decoder, &input, input.len(), 4).unwrap();
    assert_eq!(output, b"hello");
    assert_eq!(
        calls[0],
        Progress {
            consumed: input.len() - 1,
            written: 4,
            frame_done: false
        }
    );
}

#[test]
fn the_window_limit_applies_to_the_ring_path_only() {
    let input = frame(WINDOW_128K, &[block(true, RAW, 3, b"abc")]);
    let mut output = [0; 8];
    for (log, expected) in [
        (0, Ok(())),
        (16, Err(Error::WindowTooLarge)),
        (17, Ok(())),
        (31, Ok(())),
    ] {
        let mut decoder = Decoder::new();
        decoder.set_window_log_max(log);
        let result = decoder.decompress_stream(&input, &mut output).map(|_| ());
        assert_eq!(result, expected, "limit {log}");
    }
    // Single segment of 200 bytes (window 200 < 1 KiB minimum) and room for it.
    let sized = sized_frame(200, &[block(true, RLE, 200, b"q")]);
    let mut decoder = Decoder::new();
    decoder.set_window_log_max(10);
    assert!(
        decoder
            .decompress_stream(&sized, &mut [0; 200])
            .unwrap()
            .frame_done
    );
    // The default limit is 2^27.
    let huge = [&MAGIC[..], &[0, 0x90], &block(true, RAW, 0, b"")].concat();
    assert_eq!(
        Decoder::new().decompress_stream(&huge, &mut output),
        Err(Error::WindowTooLarge)
    );
    let mut decoder = Decoder::new();
    decoder.set_window_log_max(28);
    assert!(decoder.decompress_stream(&huge, &mut output).is_ok());
}

#[test]
fn window_log_max_is_clamped() {
    let mut decoder = Decoder::new();
    for (log, window) in [
        (0, 1 << 27),
        (3, 1 << 10),
        (10, 1 << 10),
        (31, 1 << 31),
        (99, 1 << 31),
    ] {
        decoder.set_window_log_max(log);
        assert_eq!(decoder.stream.max_window_size, window, "log {log}");
    }
}

#[test]
fn an_error_restarts_at_the_next_frame() {
    let good = frame(WINDOW_1K, &[block(true, RAW, 3, b"abc")]);
    let mut decoder = Decoder::new();
    let mut output = [0; 8];
    assert_eq!(
        decoder.decompress_stream(&[1, 2, 3, 4, 5, 6], &mut output),
        Err(Error::PrefixUnknown)
    );
    let progress = decoder.decompress_stream(&good, &mut output).unwrap();
    assert_eq!((progress.written, progress.frame_done), (3, true));
}
