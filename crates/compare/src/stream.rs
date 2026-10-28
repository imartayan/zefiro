//! Streaming differential harness: one driver for both decoders, so that every call can
//! be compared (bytes consumed, bytes written, frame done), not just the final output.

#![allow(clippy::print_stderr)]

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use rust_zstd_decoder::Decoder;

use crate::{Code, Libzstd, Outcome, assert_agree};

/// What one `decompress_stream` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub consumed: usize,
    pub written: usize,
    pub done: bool,
}

/// Every call of a run, and its final outcome.
#[derive(Debug)]
pub struct Run {
    pub steps: Vec<Step>,
    pub outcome: Outcome,
}

/// Calls `step` with pieces of `input` and output buffers whose sizes cycle through
/// `in_sizes` and `out_sizes` (all non-zero), re-offering what was not consumed, until
/// the input is used and the output is drained. Input ending inside a frame is
/// `Err(Code::Truncated)`.
pub fn run_stream(
    mut step: impl FnMut(&[u8], &mut [u8]) -> Result<Step, Code>,
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
) -> Run {
    let mut buffer = vec![0; out_sizes.iter().copied().max().unwrap()];
    let (mut steps, mut output) = (Vec::new(), Vec::new());
    let (mut pos, mut still, mut last_done) = (0, 0, true);
    for call in 0.. {
        let out_len = out_sizes[call % out_sizes.len()];
        let end = input.len().min(pos + in_sizes[call % in_sizes.len()]);
        let result = match step(&input[pos..end], &mut buffer[..out_len]) {
            Ok(result) => result,
            Err(code) => {
                return Run {
                    steps,
                    outcome: Outcome::Err(code),
                };
            }
        };
        steps.push(result);
        output.extend_from_slice(&buffer[..result.written]);
        pos += result.consumed;
        if result.consumed > 0 || result.written > 0 {
            (still, last_done) = (0, result.done);
        } else {
            still += 1;
            assert!(still < 100 || pos == input.len(), "no progress");
        }
        if pos == input.len() && result.written < out_len {
            break;
        }
    }
    let outcome = if last_done {
        Outcome::Ok {
            consumed: input.len(),
            output,
        }
    } else {
        Outcome::Err(Code::Truncated)
    };
    Run { steps, outcome }
}

/// A run of our decoder (a fresh one) with the window limit `window_log` (0: default).
pub fn stream_ours(input: &[u8], in_sizes: &[usize], out_sizes: &[usize], window_log: u32) -> Run {
    let mut decoder = Decoder::new();
    decoder.set_window_log_max(window_log);
    let step = |input: &[u8], output: &mut [u8]| {
        let progress = decoder.decompress_stream(input, output)?;
        Ok(Step {
            consumed: progress.consumed,
            written: progress.written,
            done: progress.frame_done,
        })
    };
    run_stream(step, input, in_sizes, out_sizes)
}

/// The same run through a fresh libzstd context: its buffers only grow, which changes
/// the room left in the ring and so some verdicts.
pub fn stream_theirs(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    window_log: u32,
) -> Run {
    let mut oracle = Libzstd::new();
    oracle.set_window_log_max(window_log);
    run_stream(|i, o| oracle.stream_step(i, o), input, in_sizes, out_sizes)
}

/// Runs both decoders and panics unless the outcomes and every call agree. Returns
/// libzstd's run.
pub fn compare_stream(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    window_log: u32,
    what: &str,
) -> Run {
    let theirs = stream_theirs(input, in_sizes, out_sizes, window_log);
    let ours = stream_ours(input, in_sizes, out_sizes, window_log);
    let what = format!("{what}: in {in_sizes:?} out {out_sizes:?} window {window_log}");
    let check = || {
        assert_agree(&ours.outcome, &theirs.outcome, &what);
        assert_eq!(ours.steps, theirs.steps, "{what}: calls differ");
    };
    if let Err(panic) = catch_unwind(AssertUnwindSafe(check)) {
        let shown = &input[..input.len().min(400)];
        eprintln!("input ({} bytes): {shown:02x?}", input.len());
        resume_unwind(panic);
    }
    theirs
}
