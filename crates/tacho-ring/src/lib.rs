#![forbid(unsafe_code)]

//! # tacho-ring
//!
//! A lock-free, single-producer, multi-reader ring buffer composed of 4 KiB chunks
//! for pre-encoded Perfetto packets.
//!
//! ## How the Seqlock Works
//!
//! Each 4 KiB chunk contains an atomic generation counter (`gen: AtomicU32`),
//! a payload length (`len: AtomicU32`), and 511 64-bit payload words (`words: [AtomicU64; 511]`).
//!
//! The writer owns exclusive write access (`RingWriter: &mut self`) and updates chunks using a
//! sequential lock (seqlock) protocol:
//! 1. `gen += 1` with `Release` ordering. Since `gen` starts even, this makes `gen` odd,
//!    signaling to all concurrent readers that a modification is in progress.
//! 2. The writer stores the packet words with `Relaxed` ordering.
//! 3. The writer stores the new length with `Release` ordering.
//! 4. `gen += 1` with `Release` ordering, returning `gen` to an even value.
//!
//! Concurrent readers (`RingReader`) snapshot chunks without taking any locks:
//! 1. The reader loads `g1 = gen` with `Acquire` ordering. If `g1` is odd, a writer is currently
//!    updating the chunk, so the reader immediately retries.
//! 2. The reader loads `len` and reads `words[..ceil(len/8)]` with `Relaxed` ordering into a local buffer.
//! 3. An `Acquire` fence ensures that reading the payload words is not reordered after the next check.
//! 4. The reader loads `g2 = gen` with `Relaxed` ordering. If `g1 == g2`, no writer modified the chunk
//!    during the read; the chunk is whole and consistent.
//! 5. If `g1 != g2` (or if `g1` was odd), the read was torn or raced with a writer. The reader retries
//!    up to 3 times. If all 3 attempts fail, the chunk is counted as skipped (`chunks_skipped += 1`).

#[cfg(loom)]
pub mod sync {
    pub use loom::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    pub use loom::sync::Arc;
    pub use loom::thread;
}

#[cfg(not(loom))]
pub mod sync {
    pub use std::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    pub use std::sync::Arc;
    pub use std::thread;
}

use sync::*;

/// Size of each chunk in bytes (4 KiB for production, reduced under loom).
#[cfg(not(loom))]
pub const CHUNK_SIZE: usize = 4096;
#[cfg(loom)]
pub const CHUNK_SIZE: usize = 40;

/// Number of 64-bit words in a chunk payload.
#[cfg(not(loom))]
pub const CHUNK_WORDS: usize = 511;
#[cfg(loom)]
pub const CHUNK_WORDS: usize = 4;

/// Maximum payload capacity of a single chunk in bytes (511 * 8 = 4088 bytes).
pub const CHUNK_PAYLOAD_CAPACITY: usize = CHUNK_WORDS * 8;

/// Minimum number of chunks in a ring buffer.
pub const MIN_CHUNKS: usize = 2;

/// Error returned when a packet exceeds the maximum chunk payload capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "packet length exceeds chunk payload capacity of {CHUNK_PAYLOAD_CAPACITY} bytes"
        )
    }
}

impl std::error::Error for TooLarge {}

/// Statistics returned by a snapshot operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotStats {
    /// Total valid payload bytes read and passed to the callback.
    pub bytes: usize,
    /// Total valid chunks successfully copied.
    pub chunks: usize,
    /// Total chunks that failed verification after retries and were skipped.
    pub chunks_skipped: usize,
    /// Total chunks overwritten since the ring buffer was constructed.
    pub overwritten: u64,
}

impl SnapshotStats {
    /// Returns overwritten count as usize.
    #[inline]
    pub fn overwritten_usize(&self) -> usize {
        self.overwritten as usize
    }
}

/// A 4 KiB chunk in the ring buffer.
///
/// Memory layout:
/// - `gen`: generation counter for seqlock synchronization (odd while writer inside).
/// - `len`: valid payload bytes currently stored in this chunk (0..=4088).
/// - `words`: 511 64-bit atomic words storing payload data in little-endian byte order.
pub struct Chunk {
    pub gen: AtomicU32,
    pub len: AtomicU32,
    pub words: [AtomicU64; CHUNK_WORDS],
}

impl Chunk {
    /// Creates a new, empty chunk.
    pub fn new() -> Self {
        Self {
            gen: AtomicU32::new(0),
            len: AtomicU32::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::new()
    }
}

struct SharedRing {
    chunks: Box<[Chunk]>,
    writer_chunk: AtomicUsize,
    overwritten: AtomicU64,
}

/// Single-producer writer for the ring buffer.
///
/// Only one instance exists per ring buffer. It is `Send` but explicitly not `Clone`.
pub struct RingWriter {
    shared: Arc<SharedRing>,
    current_chunk: usize,
    current_chunk_len: usize,
    total_chunks_started: u64,
}

/// Multi-reader handle for snapshotting the ring buffer.
///
/// Any number of readers can exist concurrently. It is `Clone`, `Send`, and `Sync`.
#[derive(Clone)]
pub struct RingReader {
    shared: Arc<SharedRing>,
}

/// The ring buffer root handle.
///
/// Owns the writer and one reader, and provides convenience methods to split them
/// or interact with them directly.
pub struct Ring {
    writer: RingWriter,
    reader: RingReader,
}

impl Ring {
    /// Constructs a new ring buffer.
    ///
    /// Rounds `bytes` down to whole 4 KiB chunks.
    /// Panics if the resulting chunk count is less than 2 chunks (8192 bytes).
    pub fn new(bytes: usize) -> Self {
        let num_chunks = bytes / CHUNK_SIZE;
        assert!(
            num_chunks >= MIN_CHUNKS,
            "ring buffer requires at least {MIN_CHUNKS} chunks ({} bytes), got {bytes} bytes",
            MIN_CHUNKS * CHUNK_SIZE
        );

        let mut chunks = Vec::with_capacity(num_chunks);
        for _ in 0..num_chunks {
            chunks.push(Chunk::new());
        }

        let shared = Arc::new(SharedRing {
            chunks: chunks.into_boxed_slice(),
            writer_chunk: AtomicUsize::new(0),
            overwritten: AtomicU64::new(0),
        });

        let writer = RingWriter {
            shared: Arc::clone(&shared),
            current_chunk: 0,
            current_chunk_len: 0,
            total_chunks_started: 0,
        };

        let reader = RingReader { shared };

        Self { writer, reader }
    }

    /// Splits the ring buffer into its single writer and a reader.
    #[inline]
    pub fn split(self) -> (RingWriter, RingReader) {
        (self.writer, self.reader)
    }

    /// Alias for [`split`](Self::split).
    #[inline]
    pub fn into_parts(self) -> (RingWriter, RingReader) {
        (self.writer, self.reader)
    }

    /// Returns a mutable reference to the writer.
    #[inline]
    pub fn writer(&mut self) -> &mut RingWriter {
        &mut self.writer
    }

    /// Clones a new reader handle.
    #[inline]
    pub fn reader(&self) -> RingReader {
        self.reader.clone()
    }

    /// Writes a framed packet into the ring buffer.
    #[inline]
    pub fn write(&mut self, framed_packet: &[u8]) -> Result<(), TooLarge> {
        self.writer.write(framed_packet)
    }

    /// Snapshots the ring buffer into the caller's callback.
    #[inline]
    pub fn snapshot(&self, out: &mut impl FnMut(&[u8])) -> SnapshotStats {
        self.reader.snapshot(out)
    }

    /// Returns the number of chunks in this ring buffer.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.reader.chunk_count()
    }
}

impl RingWriter {
    /// Writes a framed packet into the ring buffer.
    ///
    /// The packet must fit in one chunk payload (at most 4088 bytes), otherwise `Err(TooLarge)`
    /// is returned. Packets never straddle chunks: if the packet does not fit in the currently
    /// open chunk, the writer advances to the next chunk. Wrapping overwrites the oldest chunk
    /// whole and increments the `overwritten` counter.
    ///
    /// Does not allocate memory.
    pub fn write(&mut self, framed_packet: &[u8]) -> Result<(), TooLarge> {
        let packet_len = framed_packet.len();
        if packet_len > CHUNK_PAYLOAD_CAPACITY {
            return Err(TooLarge);
        }
        if packet_len == 0 {
            return Ok(());
        }

        let num_chunks = self.shared.chunks.len();
        let fits_in_current = self.total_chunks_started > 0
            && (self.current_chunk_len + packet_len <= CHUNK_PAYLOAD_CAPACITY);

        let target_chunk_idx = if fits_in_current {
            self.current_chunk
        } else {
            // Need to advance to the next chunk (or open chunk 0 on first write).
            let next_idx = if self.total_chunks_started == 0 {
                0
            } else {
                (self.current_chunk + 1) % num_chunks
            };

            self.current_chunk = next_idx;
            self.current_chunk_len = 0;
            self.total_chunks_started += 1;

            if self.total_chunks_started > num_chunks as u64 {
                // We have wrapped around and are overwriting an existing chunk.
                self.shared.overwritten.fetch_add(1, Ordering::Release);
            }

            // Publish the writer's new chunk position.
            self.shared.writer_chunk.store(next_idx, Ordering::Release);
            next_idx
        };

        let chunk = &self.shared.chunks[target_chunk_idx];
        let old_len = self.current_chunk_len;
        let new_len = old_len + packet_len;

        // Seqlock write sequence:
        // 1. Increment gen with Release (making it odd while writer is inside).
        chunk.gen.fetch_add(1, Ordering::Release);
        fence(Ordering::SeqCst);

        // 2. Store payload words with Relaxed ordering.
        store_words(chunk, old_len, framed_packet);

        // 3. Store new length with Release ordering.
        chunk.len.store(new_len as u32, Ordering::Release);

        // 4. Increment gen with Release (making it even to signal write completion).
        chunk.gen.fetch_add(1, Ordering::Release);

        self.current_chunk_len = new_len;
        Ok(())
    }

    /// Creates a reader handle for this ring.
    #[inline]
    pub fn reader(&self) -> RingReader {
        RingReader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Returns the number of chunks in this ring buffer.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.shared.chunks.len()
    }
}

impl RingReader {
    /// Copies every non-empty chunk oldest-to-newest by the writer's position,
    /// calling `out` once per chunk with the valid payload bytes.
    ///
    /// Reuses a single chunk-sized copy buffer on the stack. Performs zero heap allocation.
    pub fn snapshot(&self, out: &mut impl FnMut(&[u8])) -> SnapshotStats {
        let num_chunks = self.shared.chunks.len();
        // Sample writer position and overwritten counter at snapshot start.
        let writer_pos = self.shared.writer_chunk.load(Ordering::Acquire);
        let overwritten = self.shared.overwritten.load(Ordering::Acquire);

        let mut stats = SnapshotStats {
            bytes: 0,
            chunks: 0,
            chunks_skipped: 0,
            overwritten,
        };

        let mut words_buf = [0u64; CHUNK_WORDS];
        let mut copy_buf = [0u8; CHUNK_PAYLOAD_CAPACITY];

        // Determine chunk traversal order:
        // - If ring has not wrapped yet (overwritten == 0), chunks 0..=writer_pos are the valid
        //   chunks written so far, ordered 0 to writer_pos.
        // - If ring has wrapped (overwritten > 0), the oldest chunk is (writer_pos + 1) % num_chunks
        //   and the newest is writer_pos.
        let (start_offset, count) = if overwritten == 0 {
            (0, writer_pos + 1)
        } else {
            ((writer_pos + 1) % num_chunks, num_chunks)
        };

        for i in 0..count {
            let chunk_idx = (start_offset + i) % num_chunks;
            let chunk = &self.shared.chunks[chunk_idx];

            let mut succeeded = false;
            // Retry up to 3 times before marking chunk skipped.
            for _attempt in 0..3 {
                let g1 = chunk.gen.load(Ordering::Acquire);
                if g1 % 2 != 0 {
                    // Writer is currently writing to this chunk; yield and retry.
                    thread::yield_now();
                    continue;
                }

                let chunk_len = chunk.len.load(Ordering::Relaxed) as usize;
                if chunk_len > CHUNK_PAYLOAD_CAPACITY {
                    // Inconsistent length read during race; yield and retry.
                    thread::yield_now();
                    continue;
                }

                if chunk_len == 0 {
                    // Chunk is empty; verify writer did not write to it during read.
                    fence(Ordering::Acquire);
                    let g2 = chunk.gen.load(Ordering::Relaxed);
                    if g1 == g2 {
                        // Consistently empty; not an error, nothing to emit.
                        succeeded = true;
                        break;
                    }
                    thread::yield_now();
                    continue;
                }

                let num_words = chunk_len.div_ceil(8);
                for (w, word_slot) in words_buf.iter_mut().take(num_words).enumerate() {
                    *word_slot = chunk.words[w].load(Ordering::Relaxed);
                }

                // Acquire fence ensures word loads happen before reading g2.
                fence(Ordering::Acquire);
                let g2 = chunk.gen.load(Ordering::Relaxed);

                if g1 == g2 {
                    // Valid, non-torn chunk read. Unpack words to bytes in little-endian order.
                    for (w, &word) in words_buf.iter().take(num_words).enumerate() {
                        let word_bytes = word.to_le_bytes();
                        let start = w * 8;
                        let end = (start + 8).min(chunk_len);
                        copy_buf[start..end].copy_from_slice(&word_bytes[..end - start]);
                    }

                    // Emit only the valid payload bytes (bytes after len are never emitted).
                    out(&copy_buf[..chunk_len]);
                    stats.bytes += chunk_len;
                    stats.chunks += 1;
                    succeeded = true;
                    break;
                }

                thread::yield_now();
            }

            if !succeeded {
                stats.chunks_skipped += 1;
            }
        }

        stats
    }

    /// Returns the number of chunks in this ring buffer.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.shared.chunks.len()
    }
}

/// Packs byte payload into chunk words using exact little-endian representation.
fn store_words(chunk: &Chunk, old_len: usize, data: &[u8]) {
    let start_byte = old_len;
    let end_byte = old_len + data.len();
    let start_word = start_byte / 8;
    let end_word = end_byte.div_ceil(8);

    for w_idx in start_word..end_word {
        let w_byte_start = w_idx * 8;
        let w_byte_end = w_byte_start + 8;

        let overlap_start = w_byte_start.max(start_byte);
        let overlap_end = w_byte_end.min(end_byte);

        let data_start = overlap_start - start_byte;
        let data_end = overlap_end - start_byte;

        let word_offset_start = overlap_start - w_byte_start;
        let word_offset_end = overlap_end - w_byte_start;

        if word_offset_start == 0 && word_offset_end == 8 {
            // Full 8-byte word direct from data.
            let word_bytes: [u8; 8] = data[data_start..data_end]
                .try_into()
                .expect("slice length is exactly 8");
            chunk.words[w_idx].store(u64::from_le_bytes(word_bytes), Ordering::Relaxed);
        } else {
            // Partial word update.
            let mut word_bytes = if w_byte_start < old_len {
                // Preserve previously written bytes from the current chunk cycle.
                chunk.words[w_idx].load(Ordering::Relaxed).to_le_bytes()
            } else {
                [0u8; 8]
            };
            word_bytes[word_offset_start..word_offset_end]
                .copy_from_slice(&data[data_start..data_end]);
            chunk.words[w_idx].store(u64::from_le_bytes(word_bytes), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_size_and_constants() {
        assert_eq!(CHUNK_SIZE, 4096);
        assert_eq!(CHUNK_WORDS, 511);
        assert_eq!(CHUNK_PAYLOAD_CAPACITY, 4088);
        #[cfg(not(loom))]
        {
            assert_eq!(std::mem::size_of::<Chunk>(), 4096);
        }
    }

    #[test]
    fn test_min_chunks_panic() {
        let result = std::panic::catch_unwind(|| {
            Ring::new(4095);
        });
        assert!(result.is_err());

        let result = std::panic::catch_unwind(|| {
            Ring::new(4096); // 1 chunk -> panics
        });
        assert!(result.is_err());

        let ring = Ring::new(8192); // 2 chunks -> ok
        assert_eq!(ring.chunk_count(), 2);
    }

    #[test]
    fn test_too_large_error() {
        let mut ring = Ring::new(8192);
        let too_big = vec![0u8; 4089];
        assert_eq!(ring.write(&too_big), Err(TooLarge));

        let max_fit = vec![0u8; 4088];
        assert_eq!(ring.write(&max_fit), Ok(()));
    }

    #[test]
    fn test_write_and_snapshot_basic() {
        let mut ring = Ring::new(8192);
        let p1 = b"hello";
        let p2 = b"world";
        ring.write(p1).unwrap();
        ring.write(p2).unwrap();

        let mut chunks = Vec::new();
        let stats = ring.snapshot(&mut |payload| {
            chunks.push(payload.to_vec());
        });

        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.bytes, 10);
        assert_eq!(stats.chunks_skipped, 0);
        assert_eq!(stats.overwritten, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], b"helloworld");
    }
}
