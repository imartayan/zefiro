//! The in-frame pipeline of NOTES/PARALLEL.md. A frame's compressed blocks go through three
//! stages: 1 reads the headers and Huffman and FSE tables in stream order, ahead of the
//! execution, on the calling thread; 2 decodes literals and sequences from the job's copy of
//! the block, on the workers (or on the caller when it would wait); 3 resolves the repeat
//! offsets and executes in block order, on the caller, the only thread to write the output.
//!
//! One-shot decoding borrows the blocks from the input and scopes its threads to the call; any
//! error redoes the whole frame with the serial code, so the verdict is the serial one.
//! `decompress_stream` (`Streaming`, kept in the `Decoder`, so is the `Reader`'s) keeps its
//! threads, and jobs own the bytes copied from the input, which the serial stream machine is
//! fed, job by job. The only thing it takes from the pipeline is the work on a compressed
//! block: a block that stage 1, 2 or 3 fails on is decoded by the serial code from the states
//! before it, which gives the serial error, and the serial code does the rest of the frame. An
//! item (block header and block, or checksum) cut by the end of a call's input waits in its job.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hash::Hasher;
use core::mem;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use twox_hash::XxHash64;

use crate::decoder::{
    BlockDecoder, Decoder, History, SerialBlocks, block_placement, decompress_compressed_block,
    hash_in_place,
};
use crate::dict::Dictionary;
use crate::error::Error;
use crate::frame::{
    BLOCK_HEADER_SIZE, BlockType, FrameStart, find_frame_size, parse_block_header,
    parse_frame_start, scan_frame,
};
use crate::huf::{HufDef, HufTable};
use crate::literals::{LiteralsPlan, LiteralsState};
use crate::sequences::{
    DecodedSequences, Output, SequencesState, TableDefs, Tables, execute_decoded,
};
use crate::stream::FRAME_HEADER_SIZE_MAX;
use crate::stream::Progress;

// A frame takes the pipeline from this many compressed blocks and compressed bytes (one-shot),
// or content bytes (`Reader`, where the compressed size is not known). Zero in tests (which
// set `Decoder::gate_bytes`) and fuzzing, so that small frames use the threads.
#[cfg(not(test))]
const GATE_BLOCKS: usize = if cfg!(fuzzing) { 0 } else { 4 };
#[cfg(not(test))]
const GATE_BYTES: usize = if cfg!(fuzzing) { 0 } else { 256 << 10 };
// Blocks planned and not yet executed, per thread, counting at most this many threads (more
// jobs in flight than 24 make stage 3 read colder data: 8 threads gain 1 to 7% with the cap).
const WINDOW_PER_THREAD: usize = 6;
const WINDOW_MAX_THREADS: usize = 4;
const CHECKSUM_SIZE: usize = 4;
// From this many threads the workers hash the output; with fewer they have no time to spare
// (chr2 and the FASTQ samples, `Reader`: 2 to 3% slower at 3 threads). Two in tests and fuzzing.
const HASH_OFFLOAD_MIN_THREADS: usize = if cfg!(any(test, fuzzing)) { 2 } else { 4 };

// What the sections and blocks so far left in the literals and sequences states, besides
// the tables: `LiteralsState::flags` and `SequencesState::flags`.
type Flags = ([bool; 2], [bool; 5]);

// A block with what the stages learn about it.
#[derive(Debug, Default)]
struct Job<'a> {
    index: usize,
    // One-shot: the block, borrowed from the input. Streaming: the bytes copied from the input
    // for this item (block header and block, or the checksum), not all of them when the input
    // ends.
    bytes: Cow<'a, [u8]>,
    // Where the block starts in `bytes`.
    body: usize,
    plan: LiteralsPlan,
    nb_seq: usize,
    stream_start: usize,
    cold: bool,
    // The tables the block decodes with, as stage 1 knows them, and as stage 2 built them
    // (in buffers that stay with the job).
    huf_def: HufDef,
    table_defs: TableDefs,
    dictionary: Option<Dictionary>,
    huf: Option<Box<HufTable>>,
    tables: Option<Box<Tables>>,
    // The states once stage 1 is done with the block.
    flags: Flags,
    // Stages 1 and 2 succeeded: the block can be executed.
    usable: bool,
    literals: Vec<u8>,
    sequences: DecodedSequences,
}

// A finished range of the output, for the hash. Nothing writes it until the threads are joined.
#[derive(Debug)]
struct Region {
    start: *const u8,
    len: usize,
}

// SAFETY: the bytes are initialized and only read, by one thread at a time.
unsafe impl Send for Region {}

// What the threads share: jobs move from `pending` to `decoded`.
#[derive(Debug, Default)]
struct Queue<'a> {
    // Planned, by index.
    pending: VecDeque<Job<'a>>,
    decoded: Vec<Job<'a>>,
    // The frame's XXH64 state, `None` while a thread hashes `regions` (in order) into it.
    hasher: Option<XxHash64>,
    regions: VecDeque<Region>,
    // The frame is over, or the reader is dropped: nobody waits any more.
    stop: bool,
}

#[derive(Debug, Default)]
struct Shared<'a> {
    queue: Mutex<Queue<'a>>,
    // A job is pending, or `stop`.
    pending_or_stop: Condvar,
    // A job is decoded, the hash caught up, or `stop`.
    decoded_or_stop: Condvar,
}

// Stops the threads when dropped: on the way out of the frame, or when a worker panics.
struct Stopper<'s, 'a>(&'s Shared<'a>);

// The calling thread's part: stage 1 state kept across the blocks of a frame (the current
// table snapshots, shared by the jobs that did not change them), stage 3 state, and the
// order of the jobs.
#[derive(Debug, Default)]
struct Pipeline<'a> {
    shared: Arc<Shared<'a>>,
    // Executed jobs, for reuse (workers never see them).
    free: Vec<Job<'a>>,
    window: usize,
    // Blocks planned, and taken for execution; more to plan.
    planned: usize,
    executed: usize,
    planning: bool,
    // The dictionary the tables may refer to.
    dictionary: Option<Dictionary>,
    // Stage 3 state.
    repeat_offsets: [u32; 3],
    block_size_max: usize,
    // The workers hash the output, and the frame's hash state is in the queue.
    offload_hash: bool,
    hashing: bool,
    // Streaming: the states after the last block executed, to go back to when the serial code
    // takes over, which it has done when `serial` is set.
    boundary: Snapshot,
    serial: bool,
    // Stage 1 still reads the headers of the blocks (until one fails, or the serial code takes over).
    staging: bool,
    // Tests: blocks the serial code took over, and a block to take over although it is usable.
    #[cfg(test)]
    fallbacks: usize,
    #[cfg(test)]
    handovers: usize,
    #[cfg(test)]
    fallback_at: Option<usize>,
}

#[derive(Debug, Default)]
struct Snapshot {
    huf: HufDef,
    tables: TableDefs,
    flags: Flags,
}

// One-shot stage 1 over the input.
struct OneShot<'a> {
    pipeline: Pipeline<'a>,
    input: &'a [u8],
    // Next block header to plan.
    plan_pos: usize,
}

// The state of the `Decoder`'s streaming with threads: the workers, which outlive the frames,
// and the jobs of the frame in progress.
#[derive(Debug, Default)]
pub(crate) struct Streaming {
    pipeline: Pipeline<'static>,
    workers: Vec<JoinHandle<()>>,
    started: bool,
    // The frame in progress goes through the pipeline: its header is consumed.
    engaged: bool,
    // The frame has a checksum: it follows the last block.
    has_checksum: bool,
    checksum_next: bool,
    // The item being copied is the last of the frame.
    frame_ends: bool,
    // The last item of the frame is copied: its last byte is not reported as consumed until the
    // frame is done, and is not fed to the machine in a call that does not offer it again.
    closing: bool,
    // The item being copied from the input (`want` bytes in all, more than `bytes` holds), and
    // what it is.
    building: Option<Job<'static>>,
    item: Item,
    want: usize,
    // The job fed to the stream machine, and how much of it was.
    current: Option<(Job<'static>, usize)>,
    // Bytes of a raw or RLE item (header included) that go from the input to the machine
    // without a job, when no job is in flight.
    direct: usize,
    // An error met after output was written in the call: reported by the next call.
    deferred: Option<Error>,
    #[cfg(test)]
    pub(crate) frames: usize,
    // Errors of the serial code on frames that do not go through the pipeline, which loses the
    // bytes written in the call that fails.
    #[cfg(test)]
    pub(crate) serial_errors: usize,
}

// What the job being copied from the input holds.
#[derive(Debug, Clone, Copy, Default)]
enum Item {
    #[default]
    BlockHeader,
    Block(BlockType),
    Checksum,
}

// Stage 3 of a block of a streamed frame, called by the stream machine.
struct StreamBlocks<'p, 'j> {
    pipeline: &'p mut Pipeline<'static>,
    // The job being fed, none while the machine only flushes.
    job: Option<&'j Job<'static>>,
}

impl Job<'_> {
    fn block(&self) -> &[u8] {
        &self.bytes[self.body..]
    }

    // Stage 2: literals into the job's buffer (raw ones stay in the block), sequences raw.
    fn decode_streams(&mut self) -> Result<(), Error> {
        let block = &self.bytes[self.body..];
        let huf = if !self.plan.is_compressed() {
            None
        } else if self.huf_def.from_dictionary {
            let entropy = self.dictionary.as_ref().and_then(Dictionary::entropy);
            Some(&*entropy.ok_or(Error::CorruptionDetected)?.huffman)
        } else {
            let table = self.huf.get_or_insert_with(HufTable::boxed);
            table.build(&self.huf_def)?;
            Some(&**table)
        };
        self.plan.fill(huf, block, &mut self.literals)?;
        if self.nb_seq == 0 {
            self.sequences.clear();
            return Ok(());
        }
        let tables = self.tables.get_or_insert_with(Tables::boxed);
        tables.build_all(&self.table_defs, self.dictionary.as_ref())?;
        self.sequences
            .decode(tables, &block[self.stream_start..], self.nb_seq)
    }
}

impl Queue<'_> {
    // The hash state, if there is something to hash and nobody does.
    fn take_hasher(&mut self) -> Option<XxHash64> {
        if self.regions.is_empty() {
            return None;
        }
        self.hasher.take()
    }
}

impl<'a> Shared<'a> {
    fn lock(&self) -> MutexGuard<'_, Queue<'a>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'g>(condvar: &Condvar, queue: MutexGuard<'g, Queue<'a>>) -> MutexGuard<'g, Queue<'a>> {
        condvar.wait(queue).unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self) {
        self.lock().stop = true;
        self.pending_or_stop.notify_all();
        self.decoded_or_stop.notify_all();
    }

    // A worker: the hash of the finished output, else stage 2 on the pending jobs, until `stop`.
    fn work(&self) {
        let _stopper = Stopper(self);
        loop {
            let mut queue = self.lock();
            if queue.stop {
                return;
            }
            if let Some(hasher) = queue.take_hasher() {
                drop(queue);
                self.hash(hasher);
            } else if let Some(mut job) = queue.pending.pop_front() {
                drop(queue);
                job.usable = job.decode_streams().is_ok();
                self.lock().decoded.push(job);
                self.decoded_or_stop.notify_one();
            } else {
                drop(Self::wait(&self.pending_or_stop, queue));
            }
        }
    }

    // Hashes the queued regions, then gives the state back.
    fn hash(&self, mut hasher: XxHash64) {
        loop {
            let mut queue = self.lock();
            let Some(region) = queue.regions.pop_front() else {
                queue.hasher = Some(hasher);
                drop(queue);
                self.decoded_or_stop.notify_one();
                return;
            };
            drop(queue);
            // SAFETY: `Region`.
            hasher.write(unsafe { core::slice::from_raw_parts(region.start, region.len) });
        }
    }
}

impl Drop for Stopper<'_, '_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

impl<'a> Pipeline<'a> {
    // The serial code takes over: stage 1 must not touch the states any more.
    const fn go_serial(&mut self) {
        self.serial = true;
        self.staging = false;
    }

    // The serial code takes over from the states after the last block executed, which stage 1
    // no longer keeps (it only knows the definitions of the tables).
    fn hand_over(&mut self, literals: &mut LiteralsState, sequences: &mut SequencesState) {
        #[cfg(test)]
        {
            self.handovers += 1;
        }
        let Snapshot { huf, tables, flags } = &self.boundary;
        literals.restore(flags.0, huf);
        sequences.restore(flags.1, self.repeat_offsets, tables);
        self.go_serial();
    }

    // The frame's jobs not executed, and the ones the workers return late, are dropped.
    fn discard_jobs(&mut self) {
        self.executed = self.planned;
        let (pending, decoded) = {
            let mut queue = self.shared.lock();
            (mem::take(&mut queue.pending), mem::take(&mut queue.decoded))
        };
        self.free.extend(pending.into_iter().chain(decoded));
    }

    const fn has_room(&self) -> bool {
        self.planning && self.planned - self.executed < self.window
    }

    // Stage 1 of a compressed block: its headers and tables, in stream order. Checks that
    // depend on the output room are stage 3's.
    fn read_headers(
        &self,
        job: &mut Job<'a>,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
    ) -> Result<(), Error> {
        let block_size_max = self.block_size_max;
        let block = &job.bytes[job.body..];
        if block.len() > block_size_max {
            return Err(Error::SrcSizeWrong);
        }
        job.plan = literals.plan::<false>(block, block_size_max, block_size_max)?;
        job.huf_def = *literals.def();
        let (nb_seq, stream) = sequences.read_header::<false>(&block[job.plan.section_size..])?;
        job.nb_seq = nb_seq;
        job.stream_start = block.len() - stream.len();
        job.cold = sequences.begin_block(nb_seq);
        job.table_defs = *sequences.defs();
        job.dictionary.clone_from(&self.dictionary);
        job.flags = (literals.flags(), sequences.flags());
        Ok(())
    }

    // Queues a planned block for stage 2, or as done if it needs none.
    fn submit(&mut self, mut job: Job<'a>, needs_stage_2: bool) {
        job.index = self.planned;
        job.usable = false;
        self.planned += 1;
        let mut queue = self.shared.lock();
        if needs_stage_2 {
            queue.pending.push_back(job);
            drop(queue);
            self.shared.pending_or_stop.notify_one();
        } else {
            queue.decoded.push(job);
        }
    }

    // The job of the next block to execute, once its stage 2 is done. With `wait`, takes a
    // pending job to decode itself rather than wait; `None` if no block is planned, or a worker
    // died. Without, `None` also if the job is not decoded yet.
    fn next_decoded(&mut self, wait: bool) -> Option<Job<'a>> {
        if self.executed == self.planned {
            return None;
        }
        let job = loop {
            let mut queue = self.shared.lock();
            if let Some(at) = queue.decoded.iter().position(|j| j.index == self.executed) {
                break queue.decoded.swap_remove(at);
            }
            if queue.stop || !wait {
                return None;
            }
            let Some(mut job) = queue.pending.pop_front() else {
                if let Some(hasher) = queue.take_hasher() {
                    drop(queue);
                    self.shared.hash(hasher);
                } else {
                    drop(Shared::wait(&self.shared.decoded_or_stop, queue));
                }
                continue;
            };
            drop(queue);
            job.usable = job.decode_streams().is_ok();
            if job.index == self.executed {
                break job;
            }
            self.shared.lock().decoded.push(job);
        };
        self.executed += 1;
        Some(job)
    }

    // Stage 3, with the room checks of the serial `decode` and `read_header` callers.
    fn execute(
        &mut self,
        job: &Job<'_>,
        output: Output,
        start: usize,
        history: History<'_>,
    ) -> Result<usize, Error> {
        let capacity = output.len - start;
        if job.plan.len > history.block_size_max.min(capacity) || job.nb_seq > 0 && capacity == 0 {
            return Err(Error::DstSizeTooSmall);
        }
        let literals = job.plan.literals(job.block(), &job.literals);
        let place = block_placement(&literals, start, capacity, history);
        // The offsets of a block that fails are not kept: the serial code redoes it from the old ones.
        let mut repeat_offsets = self.repeat_offsets;
        let end = execute_decoded(
            &job.sequences,
            job.tables.as_deref(),
            job.cold,
            &mut repeat_offsets,
            &literals,
            output,
            &place,
        )?;
        self.repeat_offsets = repeat_offsets;
        Ok(end - start)
    }

    // Queues the finished `output[start..start + len]` for the hash, which starts from `checksum`.
    fn hash_block(&mut self, checksum: &mut XxHash64, output: Output, start: usize, len: usize) {
        if !self.offload_hash {
            return hash_in_place(checksum, output, start, len);
        }
        let mut queue = self.shared.lock();
        if !self.hashing {
            self.hashing = true;
            queue.hasher = Some(mem::take(checksum));
        }
        queue.regions.push_back(Region {
            // SAFETY: the block ends within `output`.
            start: unsafe { output.base.add(start) },
            len,
        });
        drop(queue);
    }

    // Waits until all that was queued is hashed, helping with it, and gives the state back.
    fn finish_checksum(&mut self, checksum: &mut XxHash64) {
        if !mem::take(&mut self.hashing) {
            return;
        }
        loop {
            let mut queue = self.shared.lock();
            if queue.stop {
                return;
            }
            let Some(hasher) = queue.hasher.take() else {
                drop(Shared::wait(&self.shared.decoded_or_stop, queue));
                continue;
            };
            let done = queue.regions.is_empty();
            drop(queue);
            if done {
                *checksum = hasher;
                return;
            }
            self.shared.hash(hasher);
        }
    }
}

impl<'a> OneShot<'a> {
    fn new(shared: Arc<Shared<'a>>, input: &'a [u8], threads: usize) -> Self {
        let pipeline = Pipeline {
            shared,
            window: WINDOW_PER_THREAD * threads.min(WINDOW_MAX_THREADS),
            offload_hash: threads >= HASH_OFFLOAD_MIN_THREADS,
            planning: true,
            ..Pipeline::default()
        };
        Self {
            pipeline,
            input,
            plan_pos: 0,
        }
    }

    // Stage 1 of the next compressed block, skipping the others.
    fn plan_next(
        &mut self,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
    ) -> Result<(), Error> {
        let block = parse_block_header(&self.input[self.plan_pos..])?;
        let start = self.plan_pos + BLOCK_HEADER_SIZE;
        let body = self
            .input
            .get(start..start + block.input_size())
            .ok_or(Error::SrcSizeWrong)?;
        self.plan_pos = start + body.len();
        self.pipeline.planning = !block.last;
        if block.block_type != BlockType::Compressed {
            return Ok(());
        }
        let mut job = self.pipeline.free.pop().unwrap_or_default();
        job.bytes = Cow::Borrowed(body);
        job.body = 0;
        self.pipeline.read_headers(&mut job, literals, sequences)?;
        self.pipeline.submit(job, true);
        Ok(())
    }
}

impl BlockDecoder for OneShot<'_> {
    fn start_frame(&mut self, sequences: &SequencesState, start: usize) {
        self.plan_pos = start;
        self.pipeline.repeat_offsets = sequences.repeat_offsets();
        self.pipeline.dictionary = sequences.dictionary().cloned();
    }

    fn decode_block(
        &mut self,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
        _: &[u8],
        output: Output,
        start: usize,
        history: History<'_>,
    ) -> Result<usize, Error> {
        self.pipeline.block_size_max = history.block_size_max;
        while self.pipeline.has_room() {
            self.plan_next(literals, sequences)?;
        }
        let job = self
            .pipeline
            .next_decoded(true)
            .filter(|job| job.usable)
            .ok_or(Error::CorruptionDetected)?;
        let result = self.pipeline.execute(&job, output, start, history);
        self.pipeline.free.push(job);
        result
    }

    fn hash_block(&mut self, checksum: &mut XxHash64, output: Output, start: usize, len: usize) {
        self.pipeline.hash_block(checksum, output, start, len);
    }

    fn finish_checksum(&mut self, checksum: &mut XxHash64) {
        self.pipeline.finish_checksum(checksum);
    }
}

impl Streaming {
    // `Decoder::decompress_stream` with threads.
    fn run(
        &mut self,
        decoder: &mut Decoder,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        if let Some(error) = self.deferred.take() {
            return Err(error);
        }
        // `Decoder::reset` and `set_dictionary` abandon the frame.
        if self.engaged && decoder.stream.at_frame_start() {
            self.abandon(decoder);
        }
        // The byte held back by the last call comes first, and is already in its job.
        let held = self.closing;
        let skip = usize::from(held && !input.is_empty());
        let (mut taken, mut written) = (0, 0);
        let result = self.drive(
            decoder,
            &input[skip..],
            output,
            (held && skip == 0, &mut taken, &mut written),
        );
        let mut progress = match result {
            // What was decoded before the error is returned first.
            Err(error) if written > 0 => {
                self.deferred = Some(error);
                Progress::new(taken, written, false)
            }
            result => result?,
        };
        progress.consumed += skip;
        if self.closing && !progress.frame_done {
            progress.consumed = progress.consumed.saturating_sub(1);
        }
        Ok(progress)
    }

    // The loop of `run`: frames that do not take the pipeline and the end of those that leave
    // it go to the serial stream machine; the others are fed to it job by job.
    fn drive(
        &mut self,
        decoder: &mut Decoder,
        input: &[u8],
        output: &mut [u8],
        (withhold, taken, written): (bool, &mut usize, &mut usize),
    ) -> Result<Progress, Error> {
        let progress = Progress::new;
        loop {
            if !self.engaged {
                if let Some(used) = self.engage(decoder, &input[*taken..], output.len())? {
                    *taken += used;
                    continue;
                }
                let result = decoder.decompress_stream_with(
                    &input[*taken..],
                    &mut output[*written..],
                    &mut SerialBlocks,
                );
                #[cfg(test)]
                {
                    self.serial_errors += usize::from(result.is_err());
                }
                let serial = result?;
                return Ok(progress(
                    *taken + serial.consumed,
                    *written + serial.written,
                    serial.frame_done,
                ));
            }
            if self.direct == 0 {
                self.start_direct(decoder, &input[*taken..]);
            }
            if self.direct == 0 {
                *taken += self.plan(decoder, &input[*taken..]);
            }
            // Decoded bytes the machine holds go out first, so that no error loses them.
            if decoder.stream.is_flushing() {
                let flushed = decoder.decompress_stream_with(
                    &[],
                    &mut output[*written..],
                    &mut StreamBlocks {
                        pipeline: &mut self.pipeline,
                        job: None,
                    },
                )?;
                *written += flushed.written;
                if decoder.stream.is_flushing() {
                    return Ok(progress(*taken, *written, false));
                }
            }
            // Raw and RLE blocks go from the input to the machine, the others from their job.
            let direct = self.direct > 0;
            if !direct
                && self.current.is_none()
                // A held byte taken in this call is no progress the caller sees.
                && !self.next_job(decoder, input.is_empty(), *taken + *written == usize::from(self.closing && *taken > 0))
            {
                if self.engaged {
                    return Ok(progress(*taken, *written, false));
                }
                continue;
            }
            let (piece, job) = match &self.current {
                _ if direct => (
                    &input[*taken..][..self.direct.min(input.len() - *taken)],
                    None,
                ),
                Some((job, position)) => {
                    // The closing item is complete only with its last byte, which the caller
                    // has not offered again.
                    let last = withhold && self.pipeline.executed == self.pipeline.planned;
                    (
                        &job.bytes[*position..job.bytes.len() - usize::from(last)],
                        Some(job),
                    )
                }
                None => continue,
            };
            let blocks = &mut StreamBlocks {
                pipeline: &mut self.pipeline,
                job,
            };
            let fed = match decoder.decompress_stream_with(piece, &mut output[*written..], blocks) {
                Ok(fed) => fed,
                Err(error) => {
                    self.abandon(decoder);
                    return Err(error);
                }
            };
            *written += fed.written;
            if direct {
                *taken += fed.consumed;
                self.direct -= fed.consumed;
            } else if let Some((job, position)) = self.current.as_mut() {
                *position += fed.consumed;
                if *position == job.bytes.len()
                    && let Some((job, _)) = self.current.take()
                {
                    self.pipeline.free.push(job);
                }
            }
            if fed.frame_done {
                self.abandon(decoder);
                return Ok(progress(*taken, *written, true));
            }
            // With all the input taken, the caller gets to offer more before the jobs in flight
            // are executed: the threads keep their blocks.
            let input_taken = !direct && !input.is_empty() && *taken == input.len() && *written > 0;
            if self.direct > 0
                || *written == output.len()
                || fed.consumed + fed.written == 0
                || input_taken
            {
                return Ok(progress(*taken, *written, false));
            }
        }
    }

    // With no job in flight and no item started, a raw or RLE block at the start of `input`
    // is fed to the machine from there: `direct` is its size.
    fn start_direct(&mut self, decoder: &Decoder, input: &[u8]) {
        let pipeline = &self.pipeline;
        let idle = self.building.is_none()
            && self.current.is_none()
            && pipeline.executed == pipeline.planned
            && pipeline.planning
            && !self.checksum_next;
        if !idle {
            return;
        }
        if let Ok(block) = parse_block_header(input)
            && block.block_type != BlockType::Compressed
            && block.input_size() <= decoder.stream.block_size_max()
        {
            self.checksum_next = block.last && self.has_checksum;
            self.pipeline.planning = !block.last || self.has_checksum;
            self.direct = BLOCK_HEADER_SIZE + block.input_size();
        }
    }

    // Starts the next job: takes the next executed block, or hands over to the serial code when
    // the frame is not planned any further. False if there is nothing to feed for now: the
    // frame is left (`engaged` cleared), or more input is needed, or the next block is not
    // decoded yet and the call made progress (`wait` is unset).
    fn next_job(&mut self, decoder: &mut Decoder, input_ended: bool, wait: bool) -> bool {
        let pipeline = &mut self.pipeline;
        if pipeline.executed == pipeline.planned {
            if pipeline.planning {
                // The input ends inside an item: the serial code takes what there is of it.
                if input_ended && let Some(job) = self.building.take() {
                    pipeline.planning = false;
                    pipeline.submit(job, false);
                    return self.next_job(decoder, input_ended, wait);
                }
                return false;
            }
            // Planning stopped and everything planned is executed: the serial code goes on.
            if !pipeline.serial {
                pipeline.hand_over(&mut decoder.literals, &mut decoder.sequences);
            }
            self.abandon(decoder);
            return false;
        }
        match pipeline.next_decoded(wait) {
            Some(job) => self.current = Some((job, 0)),
            None if wait => panic!("a decoding thread panicked"),
            None => return false,
        }
        true
    }

    // If the frame whose header is at the start of `input` (after the part the stream holds)
    // goes through the threads, decodes its header and returns the bytes of `input` it used.
    // `capacity` is the room in the caller's buffer. The gate: the frame header is whole, the
    // content size is unknown or large, the frame would not be decoded in one pass, and threads
    // are available.
    fn engage(
        &mut self,
        decoder: &mut Decoder,
        input: &[u8],
        capacity: usize,
    ) -> Result<Option<usize>, Error> {
        #[cfg(test)]
        let gate = decoder.gate_bytes as u64;
        #[cfg(not(test))]
        let gate = GATE_BYTES as u64;
        let Some(held) = decoder.stream.header_held() else {
            return Ok(None);
        };
        let mut start = [0; FRAME_HEADER_SIZE_MAX];
        start[..held.len()].copy_from_slice(held);
        let held = held.len();
        let more = input.len().min(start.len() - held);
        start[held..held + more].copy_from_slice(&input[..more]);
        let Ok(FrameStart::Frame(header)) = parse_frame_start(&start[..held + more]) else {
            return Ok(None);
        };
        if header.content_size.is_some_and(|size| size < gate) {
            return Ok(None);
        }
        let one_pass = held == 0
            && header
                .content_size
                .is_some_and(|size| capacity as u64 >= size)
            && find_frame_size(input).is_ok();
        if one_pass || !self.start_workers(decoder.threads) {
            return Ok(None);
        }
        let used = header.header_size - held;
        decoder.decompress_stream_with(&input[..used], &mut [], &mut SerialBlocks)?;
        let pipeline = &mut self.pipeline;
        pipeline.discard_jobs();
        pipeline.planning = true;
        pipeline.serial = false;
        pipeline.staging = true;
        pipeline.dictionary = decoder.sequences.dictionary().cloned();
        pipeline.repeat_offsets = decoder.sequences.repeat_offsets();
        pipeline.block_size_max = decoder.stream.block_size_max();
        pipeline.boundary = Snapshot {
            flags: (decoder.literals.flags(), decoder.sequences.flags()),
            ..Snapshot::default()
        };
        #[cfg(test)]
        {
            pipeline.fallback_at = decoder.fallback_at.map(|at| pipeline.planned + at);
        }
        self.has_checksum = header.checksum;
        self.checksum_next = false;
        self.engaged = true;
        #[cfg(test)]
        {
            self.frames += 1;
        }
        Ok(Some(used))
    }

    // Spawns the workers on first use; false if none could be.
    fn start_workers(&mut self, threads: usize) -> bool {
        if !self.started {
            self.started = true;
            for _ in 1..threads {
                let shared = Arc::clone(&self.pipeline.shared);
                match thread::Builder::new().spawn(move || shared.work()) {
                    Ok(worker) => self.workers.push(worker),
                    Err(_) => break,
                }
            }
            let threads = self.workers.len() + 1;
            self.pipeline.window = WINDOW_PER_THREAD * threads.min(WINDOW_MAX_THREADS);
            self.pipeline.offload_hash = threads >= HASH_OFFLOAD_MIN_THREADS;
        }
        !self.workers.is_empty()
    }

    // Leaves the frame: the serial code (or nobody) goes on. Jobs not executed are dropped
    // (after an error or a reset), and the hash state is back in the decoder.
    fn abandon(&mut self, decoder: &mut Decoder) {
        let pipeline = &mut self.pipeline;
        pipeline.go_serial();
        pipeline.finish_checksum(&mut decoder.checksum);
        pipeline.discard_jobs();
        if let Some((job, _)) = self.current.take() {
            pipeline.free.push(job);
        }
        if let Some(job) = self.building.take() {
            pipeline.free.push(job);
        }
        self.direct = 0;
        self.closing = false;
        self.engaged = false;
    }

    // Stage 1 of the items of the frame, as far as `input` and the window allow: the bytes are
    // copied to the item in progress, and whole items are planned and queued. Returns the bytes
    // taken. Planning stops at an item whose header does not parse. After a block whose stage 1
    // fails, the blocks are left to the serial code, which reports the error.
    fn plan(&mut self, decoder: &mut Decoder, input: &[u8]) -> usize {
        let pipeline = &mut self.pipeline;
        let mut taken = 0;
        while pipeline.has_room() && (taken < input.len() || self.building.is_some()) {
            let mut job = self.building.take().unwrap_or_else(|| {
                (self.item, self.want) = if self.checksum_next {
                    (Item::Checksum, CHECKSUM_SIZE)
                } else {
                    (Item::BlockHeader, BLOCK_HEADER_SIZE)
                };
                let mut job = pipeline.free.pop().unwrap_or_default();
                job.bytes.to_mut().clear();
                job.body = BLOCK_HEADER_SIZE;
                job
            });
            let copied = (self.want - job.bytes.len()).min(input.len() - taken);
            job.bytes
                .to_mut()
                .extend_from_slice(&input[taken..taken + copied]);
            taken += copied;
            if job.bytes.len() < self.want {
                self.building = Some(job);
                break;
            }
            match self.item {
                Item::BlockHeader => match parse_block_header(&job.bytes) {
                    Ok(block) if block.input_size() <= pipeline.block_size_max => {
                        self.checksum_next = block.last && self.has_checksum;
                        self.frame_ends = block.last && !self.has_checksum;
                        self.item = Item::Block(block.block_type);
                        self.want += block.input_size();
                        self.building = Some(job);
                    }
                    _ => {
                        pipeline.planning = false;
                        pipeline.submit(job, false);
                    }
                },
                Item::Block(block_type) => {
                    let mut staged = false;
                    if block_type == BlockType::Compressed && pipeline.staging {
                        let (literals, sequences) = (&mut decoder.literals, &mut decoder.sequences);
                        staged = pipeline.read_headers(&mut job, literals, sequences).is_ok();
                        pipeline.staging = staged;
                    }
                    pipeline.planning = !self.frame_ends;
                    self.closing = self.frame_ends;
                    pipeline.submit(job, staged);
                }
                Item::Checksum => {
                    pipeline.planning = false;
                    self.closing = true;
                    pipeline.submit(job, false);
                }
            }
        }
        taken
    }
}

impl Clone for Streaming {
    // A decoder cloned has its own threads, started when needed.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        self.pipeline.shared.stop();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl BlockDecoder for StreamBlocks<'_, '_> {
    fn decode_block(
        &mut self,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
        block: &[u8],
        output: Output,
        start: usize,
        history: History<'_>,
    ) -> Result<usize, Error> {
        let pipeline = &mut *self.pipeline;
        if !pipeline.serial {
            if let Some(job) = self.job {
                #[cfg(test)]
                let usable = job.usable && pipeline.fallback_at != Some(job.index);
                #[cfg(not(test))]
                let usable = job.usable;
                if usable
                    && job.block().len() == block.len()
                    && let Ok(written) = pipeline.execute(job, output, start, history)
                {
                    pipeline.boundary = Snapshot {
                        huf: job.huf_def,
                        tables: job.table_defs,
                        flags: job.flags,
                    };
                    return Ok(written);
                }
            }
            // From the states before this block: the serial code finds its error, and goes on.
            pipeline.hand_over(literals, sequences);
            #[cfg(test)]
            {
                pipeline.fallbacks += 1;
            }
        }
        decompress_compressed_block(literals, sequences, block, output, start, history)
    }

    fn hash_block(&mut self, checksum: &mut XxHash64, output: Output, start: usize, len: usize) {
        self.pipeline.hash_block(checksum, output, start, len);
    }

    fn finish_checksum(&mut self, checksum: &mut XxHash64) {
        self.pipeline.finish_checksum(checksum);
    }
}

impl Decoder {
    // `decompress_stream` with several threads.
    pub(crate) fn decompress_stream_threaded(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        let mut streaming = self.streaming.take().unwrap_or_default();
        let result = streaming.run(self, input, output);
        self.streaming = Some(streaming);
        result
    }

    // `decompress_frame`, through the pipeline if the frame passes the gate. On any error
    // there, the serial code decodes the frame from its start (with the dictionary flags of
    // `begin_frame` as they were).
    pub(crate) fn decompress_frame_threaded(
        &mut self,
        input: &[u8],
        output: &mut [mem::MaybeUninit<u8>],
    ) -> Result<(usize, usize), Error> {
        #[cfg(test)]
        let (gate_blocks, gate_bytes) = (0, self.gate_bytes);
        #[cfg(not(test))]
        let (gate_blocks, gate_bytes) = (GATE_BLOCKS, GATE_BYTES);
        let passes_gate = scan_frame(input)
            .is_ok_and(|(info, blocks)| blocks >= gate_blocks && info.compressed_len >= gate_bytes);
        if !passes_gate {
            return self.decompress_frame(input, output, &mut SerialBlocks);
        }
        let flags = (
            self.sequences.dictionary_is_cold,
            self.sequences.dictionary_end_kept,
        );
        let result = self.decompress_frame_pipeline(input, output);
        if result.is_ok() {
            return result;
        }
        (
            self.sequences.dictionary_is_cold,
            self.sequences.dictionary_end_kept,
        ) = flags;
        #[cfg(test)]
        {
            self.redone_frames += 1;
        }
        self.decompress_frame(input, output, &mut SerialBlocks)
    }

    // With the workers that could be spawned (all of `threads - 1`, except where threads are
    // unavailable): serial if there are none.
    fn decompress_frame_pipeline(
        &mut self,
        input: &[u8],
        output: &mut [mem::MaybeUninit<u8>],
    ) -> Result<(usize, usize), Error> {
        let shared = Arc::<Shared<'_>>::default();
        thread::scope(|scope| {
            let _stopper = Stopper(&shared);
            let workers = (1..self.threads)
                .take_while(|_| {
                    thread::Builder::new()
                        .spawn_scoped(scope, || shared.work())
                        .is_ok()
                })
                .count();
            if workers == 0 {
                return self.decompress_frame(input, output, &mut SerialBlocks);
            }
            #[cfg(test)]
            {
                self.pipelined_frames += 1;
            }
            let mut pipeline = OneShot::new(Arc::clone(&shared), input, workers + 1);
            self.decompress_frame(input, output, &mut pipeline)
        })
    }
}

#[cfg(test)]
mod tests;
