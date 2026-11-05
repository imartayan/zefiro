# Optimizations in zefiro

A zstd file holds one or more frames, each split into blocks of at most 128 KiB of output. Decoding a compressed block takes four steps:

1. Read the tables: Huffman for the literals, FSE (Finite State Entropy) for the sequences. A block can also reuse the tables of an earlier block.
2. Decode the literals, the bytes that are not copied from earlier output.
3. Decode the sequences. Each one gives a literal length, a match length to copy, and an offset pointing back into earlier output.
4. Execute the sequences: write the literals and copy the matches to the output, in order.

Steps 2 and 3 are entropy decoding, and step 4 is copying bytes. Most of the time goes to these. On one thread, zefiro runs about as fast as `libzstd`. With 4 threads it is usually 2 to 3 times faster than on one thread on large frames, because the two kinds of work can be separated.

The [README](README.md#performance) has the benchmark tables.

## Single-threaded decoding

Most of this work was catching up. The first draft of zefiro was safe Rust and ran at less than half the speed of `libzstd`.

The biggest step came from `libzstd` itself. Instead of copying exactly the number of bytes a match asks for, it copies fixed 16-byte chunks and lets the last one run past the end. The overshoot is harmless because subsequent writes overwrite it, and the output buffer has slack at the end. This removes branches on the length, which are hard to predict since most copies are short.

Several smaller gains come from [rust-zstd](https://github.com/physwkim/rust-zstd), another fast Rust decoder. The main one is in building Huffman tables: symbol weights are counted in registers instead of memory, and table cells are filled in whole runs instead of one at a time. This matters on small frames, where building the table takes a large share of the time.

One change to the sequence loop is specific to zefiro. A sequence can either specify a new offset or reuse a recently used one. Branching between the two is often hard to predict. Updating the FSE state before taking that branch means a misprediction no longer throws that state update away.

Some tricks did not carry over. Wider AVX2 copies made zefiro slower, probably because most copies are short. Decoding one sequence ahead of the one being copied helps on ARM but hurts on x86, which has fewer registers, so that loop differs between the two.

## Multi-threaded decoding

Steps 2 and 3 only depend on the block's own bytes and on its tables, so blocks can go through them in any order. Step 4 copies from output that is already written, so it has to run in order. Block headers give the size of each block, so all the blocks can be located up front.

That gives three stages, an idea adapted from rust-zstd:

1. The calling thread reads the headers, in order.
2. Worker threads build the tables and decode the literals and sequences into buffers (steps 1 to 3).
3. The calling thread executes the sequences, in order (step 4).

Decoding a block can still depend on earlier blocks in two ways:

A block can say "use the same tables as before", and the block that defined them may not be decoded yet. The definition is just bytes in the compressed data, though. The first stage notes where it is, and the worker builds its tables from those bytes without waiting.

A sequence can say "use the same offset as an earlier sequence", which may refer to one from the previous block. The worker doesn't know what that offset was, so it records a placeholder (which of the 3 recent slots to use), and the third stage fills in the actual number once that previous block finishes.

A frame can also end with a checksum of its output, which has to be computed in order too. It only reads finished output, so with enough threads a worker does it, leaving the calling thread free to execute.

## Limits of parallel decoding

Each match can copy from anything written before it, so the third stage runs on one thread. More cores speed up the decoding but not the copying, which is why the speedup levels off around 2 to 3 times. Splitting the work also has a cost, so a single thread keeps one loop for steps 3 and 4, and so do small frames, which have too little work to share.

Letting workers execute what they can and leaving the rest for later would not help much. On most files, most bytes depend on an earlier block, so little work would move. Filling in those bytes also loses the 16-byte trick, since overshooting would overwrite what is already there.
