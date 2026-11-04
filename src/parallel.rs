//! The in-frame pipeline of NOTES/PARALLEL.md. A frame's compressed blocks go through three
//! stages: 1 reads the headers and Huffman and FSE tables in stream order, ahead of the
//! execution, on the calling thread; 2 decodes literals and sequences from the job's copy of
//! the block, on the workers (or on the caller when it would wait); 3 resolves the repeat
//! offsets and executes in block order, on the caller, the only thread to write the output.
//!
//! One-shot decoding borrows the blocks from the input and scopes its threads to the call; any
//! error redoes the whole frame with the serial code, so the verdict is the serial one.
//! A `Reader` (`Streaming`) keeps its threads, and jobs own the bytes read from the input,
//! which the serial `decompress_stream` machine is fed again, job by job, with the same
//! chunking as the input had. The only thing it takes from the pipeline is the work on a
//! compressed block: a block that stage 1, 2 or 3 fails on is decoded by the serial code from
//! the states before it, which gives the serial error, and the serial code does the rest of the
//! frame.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hash::Hasher;
use core::mem;
use std::io::{self, BufRead};
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
    // One-shot: the block, borrowed from the input. Streaming: the bytes read from the input
    // for this item (block header and block, or the checksum), not a whole block when the
    // input ends.
    bytes: Cow<'a, [u8]>,
    // Where the block starts in `bytes`.
    body: usize,
    // Streaming: the offsets in `bytes` where a chunk of the input ended.
    cuts: Vec<usize>,
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
    // The definitions of the current tables, and the dictionary they may refer to.
    huf: HufDef,
    tables: TableDefs,
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
    // Tests: blocks the serial code took over, and a block to take over although it is usable.
    #[cfg(test)]
    fallbacks: usize,
    #[cfg(test)]
    pub(crate) fallback_at: Option<usize>,
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

// The state of a `Reader` with threads: the workers, which outlive the frames, and the jobs of
// the frame in progress.
#[derive(Debug)]
pub(crate) struct Streaming {
    pipeline: Pipeline<'static>,
    workers: Vec<JoinHandle<()>>,
    threads: usize,
    started: bool,
    // The frame has a checksum: it follows the last block.
    has_checksum: bool,
    checksum_next: bool,
    // The job fed to the decoder, and how much of it was.
    current: Option<(Job<'static>, usize)>,
    #[cfg(test)]
    pub(crate) frames: usize,
}

// Stage 3 of a block of a `Reader`'s frame, called by the stream machine.
struct StreamBlocks<'p, 'j> {
    pipeline: &'p mut Pipeline<'static>,
    job: &'j Job<'static>,
}

impl Job<'_> {
    fn block(&self) -> &[u8] {
        &self.bytes[self.body..]
    }

    // Stage 2: literals into the job's buffer (raw ones stay in the block), sequences raw.
    fn decode_streams(&mut self) -> Result<(), Error> {
        let block = &self.bytes[self.body..];
        let huf = match &self.huf_def {
            _ if !self.plan.is_compressed() => None,
            HufDef::Dictionary => {
                let entropy = self.dictionary.as_ref().and_then(Dictionary::entropy);
                Some(&*entropy.ok_or(Error::CorruptionDetected)?.huffman)
            }
            HufDef::Described(description) => {
                let table = self.huf.get_or_insert_with(HufTable::boxed);
                table.build(description)?;
                Some(&**table)
            }
        };
        self.plan.fill(huf, block, &mut self.literals)?;
        if self.nb_seq == 0 {
            self.sequences.clear();
            return Ok(());
        }
        let tables = self.tables.get_or_insert_with(Tables::boxed);
        tables.build(&self.table_defs, self.dictionary.as_ref())?;
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
        self.planning = false;
    }

    // The serial code takes over from the states after the last block executed, which stage 1
    // no longer keeps (it only knows the definitions of the tables).
    fn hand_over(&mut self, literals: &mut LiteralsState, sequences: &mut SequencesState) {
        let Snapshot { huf, tables, flags } = &self.boundary;
        literals.restore(flags.0, huf);
        sequences.restore(flags.1, self.repeat_offsets, tables);
        self.go_serial();
    }

    const fn has_room(&self) -> bool {
        self.planning && self.planned - self.executed < self.window
    }

    fn free_job(&mut self) -> Job<'a> {
        self.free.pop().unwrap_or_default()
    }

    fn recycle(&mut self, job: Job<'a>) {
        self.free.push(job);
    }

    // Stage 1 of a compressed block: its headers and tables, in stream order. Checks that
    // depend on the output room are stage 3's.
    fn read_headers(
        &mut self,
        job: &mut Job<'a>,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
    ) -> Result<(), Error> {
        let block_size_max = self.block_size_max;
        let block = &job.bytes[job.body..];
        if block.len() > block_size_max {
            return Err(Error::SrcSizeWrong);
        }
        job.plan = literals.plan_lazy(block, block_size_max, block_size_max, &mut self.huf)?;
        job.huf_def = self.huf;
        let (nb_seq, stream) =
            sequences.read_header_lazy(&block[job.plan.section_size..], &mut self.tables)?;
        job.nb_seq = nb_seq;
        job.stream_start = block.len() - stream.len();
        job.cold = sequences.begin_block(nb_seq);
        job.table_defs = self.tables;
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

    // The job of the next block to execute, once its stage 2 is done. Takes a pending job to
    // decode itself rather than wait. `None` if no block is planned, or a worker died.
    fn next_decoded(&mut self) -> Option<Job<'a>> {
        if self.executed == self.planned {
            return None;
        }
        let job = loop {
            let mut queue = self.shared.lock();
            if let Some(at) = queue.decoded.iter().position(|j| j.index == self.executed) {
                break queue.decoded.swap_remove(at);
            }
            if queue.stop {
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
        let mut job = self.pipeline.free_job();
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
            .next_decoded()
            .filter(|job| job.usable)
            .ok_or(Error::CorruptionDetected)?;
        let result = self.pipeline.execute(&job, output, start, history);
        self.pipeline.recycle(job);
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
    pub(crate) fn new(threads: usize) -> Self {
        Self {
            pipeline: Pipeline::default(),
            workers: Vec::new(),
            threads,
            started: false,
            has_checksum: false,
            checksum_next: false,
            current: None,
            #[cfg(test)]
            frames: 0,
        }
    }

    // If the frame at the start of `input` goes through the threads, decodes its header and
    // returns its size. `capacity` is the room in the caller's buffer. The gate: the frame
    // header is whole in `input`, the content size is unknown or large, `decompress_stream`
    // would not decode the frame in one pass, and threads are available.
    pub(crate) fn start_frame(
        &mut self,
        decoder: &mut Decoder,
        input: &[u8],
        capacity: usize,
    ) -> Result<Option<usize>, Error> {
        #[cfg(test)]
        let gate = decoder.gate_bytes as u64;
        #[cfg(not(test))]
        let gate = GATE_BYTES as u64;
        let idle = self.current.is_none() && self.pipeline.executed == self.pipeline.planned;
        if !(idle && decoder.stream.at_frame_start()) {
            return Ok(None);
        }
        let Ok(FrameStart::Frame(header)) = parse_frame_start(input) else {
            return Ok(None);
        };
        let one_pass = header
            .content_size
            .is_some_and(|size| capacity as u64 >= size)
            && find_frame_size(input).is_ok();
        if header.content_size.is_some_and(|size| size < gate) || one_pass || !self.start_workers()
        {
            return Ok(None);
        }
        decoder.decompress_stream(&input[..header.header_size], &mut [])?;
        let pipeline = &mut self.pipeline;
        pipeline.planned = 0;
        pipeline.executed = 0;
        pipeline.planning = true;
        pipeline.serial = false;
        pipeline.huf = HufDef::default();
        pipeline.tables = TableDefs::default();
        pipeline.dictionary = decoder.sequences.dictionary().cloned();
        pipeline.repeat_offsets = decoder.sequences.repeat_offsets();
        pipeline.block_size_max = decoder.stream.block_size_max();
        pipeline.boundary = Snapshot {
            flags: (decoder.literals.flags(), decoder.sequences.flags()),
            ..Snapshot::default()
        };
        self.has_checksum = header.checksum;
        self.checksum_next = false;
        #[cfg(test)]
        {
            self.frames += 1;
        }
        Ok(Some(header.header_size))
    }

    // Spawns the workers on first use; false if none could be.
    fn start_workers(&mut self) -> bool {
        if !self.started {
            self.started = true;
            for _ in 1..self.threads {
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

    // Feeds `decoder` the next piece of what was read ahead for the frame, writing to
    // `buf[written..]`; `None` if nothing is left. A piece ends where a chunk of the input did,
    // and the flag says if `decompress_stream` would have returned there.
    pub(crate) fn step<R: BufRead>(
        &mut self,
        inner: &mut R,
        decoder: &mut Decoder,
        buf: &mut [u8],
        written: usize,
    ) -> io::Result<Option<(Progress, bool)>> {
        if self.current.is_none() {
            while self.pipeline.has_room() {
                self.plan_next(inner, decoder);
            }
            if self.pipeline.executed == self.pipeline.planned {
                // Planning stopped and everything planned is executed: the serial code goes on.
                if !self.pipeline.serial {
                    self.pipeline
                        .hand_over(&mut decoder.literals, &mut decoder.sequences);
                }
                self.pipeline.finish_checksum(&mut decoder.checksum);
                return Ok(None);
            }
            let job = self.pipeline.next_decoded();
            self.current = Some((
                job.ok_or_else(|| io::Error::other("a decoding thread panicked"))?,
                0,
            ));
        }
        let Some((job, position)) = self.current.as_mut() else {
            return Ok(None);
        };
        let len = job.bytes.len();
        let end = job
            .cuts
            .iter()
            .copied()
            .find(|&cut| cut > *position)
            .unwrap_or(len);
        let piece = &job.bytes[*position..end];
        let mut blocks = StreamBlocks {
            pipeline: &mut self.pipeline,
            job,
        };
        // After an error, the rest of the input read ahead is fed as it comes, by the serial code.
        let result = decoder.decompress_stream_with(piece, &mut buf[written..], &mut blocks);
        if !result.as_ref().is_ok_and(|progress| !progress.frame_done) {
            // The serial code takes over the checksum too.
            self.pipeline.go_serial();
            self.pipeline.finish_checksum(&mut decoder.checksum);
        }
        let progress = result?;
        let returns = progress.consumed < piece.len()
            || progress.frame_done
            || job.cuts.binary_search(&end).is_ok();
        *position += progress.consumed;
        if *position == job.bytes.len()
            && let Some((job, _)) = self.current.take()
        {
            self.pipeline.recycle(job);
        }
        Ok(Some((progress, returns)))
    }

    // Stage 1 of the next item of the frame, read from `inner`: a block, or the checksum. The
    // planning stops at the first item that is not whole, or whose headers do not parse; the
    // serial code reads what follows, and reports the error.
    fn plan_next<R: BufRead>(&mut self, inner: &mut R, decoder: &mut Decoder) {
        let pipeline = &mut self.pipeline;
        let mut job = pipeline.free_job();
        job.bytes.to_mut().clear();
        job.cuts.clear();
        job.body = BLOCK_HEADER_SIZE;
        let mut needs_stage_2 = false;
        let complete = if self.checksum_next {
            self.checksum_next = false;
            pipeline.planning = false;
            take(inner, CHECKSUM_SIZE, &mut job)
        } else if !take(inner, BLOCK_HEADER_SIZE, &mut job) {
            false
        } else if let Ok(block) = parse_block_header(&job.bytes)
            && block.input_size() <= pipeline.block_size_max
        {
            self.checksum_next = block.last && self.has_checksum;
            pipeline.planning = !block.last || self.has_checksum;
            let complete = take(inner, block.input_size(), &mut job);
            if complete && block.block_type == BlockType::Compressed {
                let (literals, sequences) = (&mut decoder.literals, &mut decoder.sequences);
                needs_stage_2 = pipeline.read_headers(&mut job, literals, sequences).is_ok();
                pipeline.planning &= needs_stage_2;
            }
            complete
        } else {
            false
        };
        pipeline.planning &= complete;
        if job.bytes.is_empty() {
            pipeline.recycle(job);
        } else {
            pipeline.submit(job, needs_stage_2);
        }
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
            let job = self.job;
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

// Appends `n` bytes of the input to the job, noting the offsets where a chunk of the input
// ended. False if the input ends or fails first (the serial code meets it again).
fn take<R: BufRead>(inner: &mut R, mut n: usize, job: &mut Job<'static>) -> bool {
    let bytes = job.bytes.to_mut();
    while n > 0 {
        let Ok(chunk) = inner.fill_buf() else {
            return false;
        };
        let taken = n.min(chunk.len());
        if taken == 0 {
            return false;
        }
        bytes.extend_from_slice(&chunk[..taken]);
        if taken == chunk.len() {
            job.cuts.push(bytes.len());
        }
        inner.consume(taken);
        n -= taken;
    }
    true
}

impl Decoder {
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
