//! The parallel `$MFT` scan.
//!
//! [`Scan`] cuts `$MFT` into chunks of whole records (4 MiB by default), and worker threads
//! claim them in turn: each reads its chunk with positional reads (through `$MFT`'s extents,
//! skipping holes, aligned for devices), applies each record's update sequence in place, and
//! hands every record to a caller's [`Projection`], which turns the chunk into an output. The
//! outputs reach the caller's sink on the calling thread, in record order or as they finish.
//!
//! The projection decides what a record means and how much of it to read: a disk-usage scan
//! reads five fields of live records and skips everything else; a thorough one checks and
//! keeps every byte. Because the projection is a type parameter, each scan compiles to its own
//! loop, and a cheap projection pays for nothing it does not read.
//!
//! A chunk whose read fails is read again record by record, so an unreadable sector costs its
//! own records only ([`Entry::Unreadable`]). Records `$MFT`'s runs do not map are
//! [`Entry::Unmapped`]; records that are not `FILE` records, are torn, or state another number
//! than their place are [`Entry::Damaged`], with their bytes as stored.

use std::{
    collections::BTreeMap,
    io,
    ops::{ControlFlow, Range},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

use crate::{
    Error, FormatError,
    mft::{Mft, Place, read_holey},
    record::{Record, RecordHeader, apply_update_sequence},
    source::{AlignedBuf, ReadAt},
};

/// One slot of `$MFT`, as the scan found it.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    /// A `FILE` record, its update sequence applied (in use or not: see
    /// [`Record::in_use`]).
    Record(Record<'a>),
    /// A slot of zeros: never used, or a hole in the source.
    Empty,
    /// A slot that is not a valid record: no `FILE` signature (`BAAD`, garbage), a torn or
    /// misplaced update sequence, a header out of range, or another record's number.
    Damaged {
        /// Why.
        error: FormatError,
        /// The slot's bytes as stored.
        raw: &'a [u8],
    },
    /// The slot could not be read.
    Unreadable(io::ErrorKind),
    /// `$MFT`'s runs do not map the slot.
    Unmapped,
    /// The in-use bitmap given to [`Scan::in_use`] marks the slot free: it was not read.
    Unused,
}

/// A chunk of records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The chunk's position among the scan's chunks, from 0.
    pub index: usize,
    /// Its first record.
    pub first: u64,
    /// Its records.
    pub records: usize,
}

/// What a scan makes of records: an output per chunk, built on a worker thread.
pub trait Projection: Sync {
    /// What a chunk becomes.
    type Output: Send;
    /// A worker's reusable state (buffers, caches), made once per worker.
    type Scratch: Send;

    /// A new worker's state.
    fn scratch(&self) -> Self::Scratch;

    /// The output of `chunk`, before its records.
    fn begin(&self, scratch: &mut Self::Scratch, chunk: Chunk) -> Self::Output;

    /// One record of the chunk, in order.
    fn entry(
        &self,
        output: &mut Self::Output,
        scratch: &mut Self::Scratch,
        number: u64,
        entry: Entry<'_>,
    );

    /// After the chunk's last record.
    fn end(&self, output: &mut Self::Output, scratch: &mut Self::Scratch) {
        let _ = (output, scratch);
    }
}

/// Counts of a scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Slots scanned.
    pub records: u64,
    /// Chunks delivered.
    pub chunks: usize,
    /// Bytes read from the source.
    pub bytes_read: u64,
    /// Slots that could not be read.
    pub unreadable: u64,
    /// Slots no extent maps.
    pub unmapped: u64,
    /// Slots refused as records.
    pub damaged: u64,
    /// Whether the sink stopped the scan early.
    pub stopped: bool,
}

/// A scan of `$MFT`'s records; see the module documentation.
#[derive(Debug)]
pub struct Scan<'a, S> {
    mft: &'a Mft<S>,
    threads: usize,
    chunk_bytes: usize,
    ordered: bool,
    window: usize,
    check_numbers: bool,
    range: Option<(u64, u64)>,
    cancel: Option<&'a AtomicBool>,
    progress: Option<&'a AtomicU64>,
    in_use: Option<&'a [u8]>,
    wait_for_sink: bool,
}

impl<S> Clone for Scan<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> Copy for Scan<'_, S> {}

/// The default chunk: 4 MiB, 4,096 records of 1 KiB.
pub const DEFAULT_CHUNK_BYTES: usize = 4 << 20;

impl<'a, S: ReadAt> Scan<'a, S> {
    /// A scan of every record of `mft`, on every core, delivered in record order.
    #[must_use]
    pub fn new(mft: &'a Mft<S>) -> Self {
        Self {
            mft,
            threads: 0,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            ordered: true,
            window: 0,
            check_numbers: true,
            range: None,
            cancel: None,
            progress: None,
            in_use: None,
            wait_for_sink: false,
        }
    }

    /// Worker threads (0, the default: one per core).
    #[must_use]
    pub const fn threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// Bytes per chunk, rounded down to whole records (at least one).
    #[must_use]
    pub const fn chunk_bytes(mut self, bytes: usize) -> Self {
        self.chunk_bytes = bytes;
        self
    }

    /// Whether outputs reach the sink in record order (the default) or as they finish.
    #[must_use]
    pub const fn ordered(mut self, ordered: bool) -> Self {
        self.ordered = ordered;
        self
    }

    /// Most chunks read but not yet delivered (0, the default: twice the threads). Bounds the
    /// memory outputs hold.
    #[must_use]
    pub const fn window(mut self, chunks: usize) -> Self {
        self.window = chunks;
        self
    }

    /// Whether a record that states another number than its place is [`Entry::Damaged`] (the
    /// default): its bytes belong to another record.
    #[must_use]
    pub const fn check_record_numbers(mut self, check: bool) -> Self {
        self.check_numbers = check;
        self
    }

    /// Scans only the records of `range`.
    #[must_use]
    pub const fn range(mut self, range: Range<u64>) -> Self {
        self.range = Some((range.start, range.end));
        self
    }

    /// Stops the scan (with [`Error::Cancelled`]) once `flag` is set.
    #[must_use]
    pub const fn cancel(mut self, flag: &'a AtomicBool) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// Adds every scanned record to `counter`.
    #[must_use]
    pub const fn progress(mut self, counter: &'a AtomicU64) -> Self {
        self.progress = Some(counter);
        self
    }

    /// Reads only records `bitmap` marks in use (`$MFT`'s `$BITMAP`: bit `n % 8` of byte
    /// `n / 8` for record `n`); the others are [`Entry::Unused`] and chunks without a record in
    /// use are not read at all. For callers that want live records only.
    #[must_use]
    pub const fn in_use(mut self, bitmap: &'a [u8]) -> Self {
        self.in_use = Some(bitmap);
        self
    }

    /// Whether a worker waits for the sink to be done with its output before it reads another
    /// chunk (off by default). An output's memory, allocated on its worker, is then freed by a
    /// sink that drops it while that worker allocates nothing. Allocators that lock to free
    /// another thread's memory (glibc's) otherwise contend with the worker on every free: for
    /// outputs of many small allocations, that is most of the sink's time, and more threads make
    /// it slower. Each worker then has one output in flight, so give the scan more threads.
    #[must_use]
    pub const fn wait_for_sink(mut self, wait: bool) -> Self {
        self.wait_for_sink = wait;
        self
    }

    /// The records scanned.
    fn records(&self) -> Range<u64> {
        let all = self.mft.records();
        match self.range {
            Some((start, end)) => start.min(all)..end.min(all),
            None => 0..all,
        }
    }

    /// Runs `projection` over every chunk and hands each output to `sink`, which may stop the
    /// scan early with [`ControlFlow::Break`].
    ///
    /// # Errors
    /// [`Error::Cancelled`] when cancelled. Unreadable, unmapped and damaged records are
    /// entries, never errors.
    pub fn run<P, F>(&self, projection: &P, mut sink: F) -> Result<Summary, Error>
    where
        P: Projection,
        F: FnMut(Chunk, P::Output) -> ControlFlow<()>,
    {
        let records = self.records();
        let record_size = self.mft.record_size() as usize;
        let per_chunk = (self.chunk_bytes / record_size).max(1);
        let total = records.end.saturating_sub(records.start);
        let chunks = usize::try_from(total.div_ceil(per_chunk as u64)).unwrap_or(usize::MAX);
        let threads = match self.threads {
            0 => std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get),
            threads => threads,
        }
        .clamp(1, chunks.max(1));
        let window = if self.window == 0 {
            threads * 2
        } else {
            self.window
        }
        .max(1);
        let shared = Shared {
            next: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            delivered: Mutex::new(0),
            advanced: Condvar::new(),
            counts: Mutex::new(Summary::default()),
            released: if self.wait_for_sink {
                (0..threads).map(|_| Release::default()).collect()
            } else {
                Vec::new()
            },
        };
        let plan = Plan {
            records,
            per_chunk,
            chunks,
            window,
        };
        let mut summary = std::thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel::<(Chunk, P::Output, usize)>(window);
            for worker in 0..threads {
                let tx = tx.clone();
                let (shared, plan) = (&shared, &plan);
                scope.spawn(move || self.work(projection, shared, plan, worker, &tx));
            }
            drop(tx);
            let _stop = StopOnPanic(&shared);
            let mut summary = Summary::default();
            let mut pending = BTreeMap::new();
            let mut next = 0usize;
            let mut deliver =
                |chunk: Chunk, output: P::Output, worker: usize, summary: &mut Summary| {
                    summary.chunks += 1;
                    if sink(chunk, output).is_break() {
                        summary.stopped = true;
                        shared.stop();
                    }
                    shared.release(worker);
                };
            for (chunk, output, worker) in &rx {
                if shared.stop.load(Ordering::Relaxed) {
                    continue;
                }
                if !self.ordered {
                    deliver(chunk, output, worker, &mut summary);
                    continue;
                }
                pending.insert(chunk.index, (chunk, output, worker));
                while let Some((chunk, output, worker)) = pending.remove(&next) {
                    next += 1;
                    shared.delivered(next);
                    deliver(chunk, output, worker, &mut summary);
                    if summary.stopped {
                        break;
                    }
                }
            }
            summary
        });
        let counts = shared.counts.into_inner().unwrap_or_default();
        summary.records = counts.records;
        summary.bytes_read = counts.bytes_read;
        summary.unreadable = counts.unreadable;
        summary.unmapped = counts.unmapped;
        summary.damaged = counts.damaged;
        if !summary.stopped && self.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        Ok(summary)
    }

    /// Runs `projection` and returns every output in record order.
    ///
    /// # Errors
    /// As [`Self::run`].
    pub fn collect<P: Projection>(&self, projection: &P) -> Result<Vec<P::Output>, Error> {
        let mut outputs = Vec::new();
        self.ordered(true).run(projection, |_, output| {
            outputs.push(output);
            ControlFlow::Continue(())
        })?;
        Ok(outputs)
    }

    /// A worker: claims chunks until none is left or the scan stops.
    fn work<P: Projection>(
        &self,
        projection: &P,
        shared: &Shared,
        plan: &Plan,
        worker: usize,
        tx: &mpsc::SyncSender<(Chunk, P::Output, usize)>,
    ) {
        let record_size = self.mft.record_size() as usize;
        let align = self.mft.source().alignment();
        let mut buf = AlignedBuf::new(plan.per_chunk * record_size, align);
        let mut states = vec![Slot::Read; plan.per_chunk];
        let mut scratch = projection.scratch();
        let mut retry = Vec::new();
        let mut counts = Summary::default();
        let mut sent = 0u64;
        let _stop = StopOnPanic(shared);
        loop {
            if shared.stop.load(Ordering::Relaxed)
                || self.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                shared.stop();
                break;
            }
            let index = shared.next.fetch_add(1, Ordering::Relaxed);
            if index >= plan.chunks {
                break;
            }
            if self.ordered && !shared.wait_for_window(index, plan.window) {
                break;
            }
            let first = plan.records.start + (index * plan.per_chunk) as u64;
            let count = usize::try_from((plan.records.end - first).min(plan.per_chunk as u64))
                .unwrap_or(plan.per_chunk);
            let chunk = Chunk {
                index,
                first,
                records: count,
            };
            let states = states.get_mut(..count).unwrap_or_default();
            counts.bytes_read += self.read_chunk(chunk, buf.as_mut_slice(), states, &mut retry);
            let mut output = projection.begin(&mut scratch, chunk);
            let bytes = buf.as_mut_slice();
            for (i, state) in states.iter().enumerate() {
                let number = first + i as u64;
                let Some(slot) = bytes.get_mut(i * record_size..(i + 1) * record_size) else {
                    break;
                };
                let entry = match *state {
                    Slot::Read => classify(slot, number, self.check_numbers),
                    Slot::Unmapped => Entry::Unmapped,
                    Slot::Unreadable(kind) => Entry::Unreadable(kind),
                    Slot::Unused => Entry::Unused,
                };
                match entry {
                    Entry::Damaged { .. } => counts.damaged += 1,
                    Entry::Unreadable(_) => counts.unreadable += 1,
                    Entry::Unmapped => counts.unmapped += 1,
                    _ => {}
                }
                projection.entry(&mut output, &mut scratch, number, entry);
            }
            projection.end(&mut output, &mut scratch);
            counts.records += count as u64;
            if let Some(progress) = self.progress {
                progress.fetch_add(count as u64, Ordering::Relaxed);
            }
            if tx.send((chunk, output, worker)).is_err() {
                break;
            }
            sent += 1;
            if self.wait_for_sink && !shared.wait_released(worker, sent) {
                break;
            }
        }
        if let Ok(mut total) = shared.counts.lock() {
            total.records += counts.records;
            total.bytes_read += counts.bytes_read;
            total.unreadable += counts.unreadable;
            total.unmapped += counts.unmapped;
            total.damaged += counts.damaged;
        }
    }

    /// Reads `chunk` into `buf`, marking each slot's state; returns the bytes read.
    fn read_chunk(
        &self,
        chunk: Chunk,
        buf: &mut [u8],
        states: &mut [Slot],
        retry: &mut Vec<usize>,
    ) -> u64 {
        let record_size = self.mft.record_size() as usize;
        states.fill(Slot::Read);
        if let Some(bitmap) = self.in_use {
            let mut any = false;
            for (i, state) in states.iter_mut().enumerate() {
                let number = chunk.first + i as u64;
                if is_set(bitmap, number) {
                    any = true;
                } else {
                    *state = Slot::Unused;
                }
            }
            if !any {
                return 0;
            }
        }
        let start = chunk.first * record_size as u64;
        let end = start + (chunk.records * record_size) as u64;
        let mut read = 0u64;
        retry.clear();
        for (offset, len, place) in self.mft.map().pieces(start..end) {
            let at = usize::try_from(offset - start).unwrap_or(usize::MAX);
            let len_usize = usize::try_from(len).unwrap_or(usize::MAX);
            let slots = at / record_size..(at + len_usize).div_ceil(record_size);
            let Some(slot) = buf.get_mut(at..at.saturating_add(len_usize)) else {
                continue;
            };
            match place {
                Place::Volume(position) => match read_holey(self.mft.source(), position, slot) {
                    Ok(()) => read += len,
                    Err(_) => retry.extend(slots),
                },
                Place::Sparse => slot.fill(0),
                Place::Unmapped => {
                    for state in states.get_mut(slots).unwrap_or_default() {
                        *state = Slot::Unmapped;
                    }
                }
            }
        }
        if !retry.is_empty() {
            retry.dedup();
            let mut one = Vec::new();
            for &i in retry.iter() {
                let Some(state) = states.get_mut(i) else {
                    continue;
                };
                if !matches!(state, Slot::Read) {
                    continue;
                }
                match self.mft.read_record(chunk.first + i as u64, &mut one) {
                    Ok(()) => {
                        if let Some(slot) = buf.get_mut(i * record_size..(i + 1) * record_size) {
                            slot.copy_from_slice(&one);
                            read += record_size as u64;
                        }
                    }
                    Err(Error::Io(error)) => *state = Slot::Unreadable(error.kind()),
                    Err(Error::Unmapped { .. }) => *state = Slot::Unmapped,
                    Err(_) => *state = Slot::Unreadable(io::ErrorKind::Other),
                }
            }
        }
        read
    }
}

/// What the scan knows of a slot before looking at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Read,
    Unmapped,
    Unreadable(io::ErrorKind),
    Unused,
}

/// The scan's chunking.
#[derive(Debug)]
struct Plan {
    records: Range<u64>,
    per_chunk: usize,
    chunks: usize,
    window: usize,
}

/// State the workers and the delivering thread share.
#[derive(Debug)]
struct Shared {
    next: AtomicUsize,
    stop: AtomicBool,
    /// Chunks delivered in order so far (ordered scans).
    delivered: Mutex<usize>,
    advanced: Condvar,
    counts: Mutex<Summary>,
    /// Each worker's outputs the sink is done with ([`Scan::wait_for_sink`]; empty without).
    released: Vec<Release>,
}

/// A worker's outputs the sink is done with, and its wake-up: one per worker, so that a release
/// wakes only the worker it concerns.
#[derive(Debug, Default)]
struct Release {
    done: Mutex<u64>,
    ready: Condvar,
}

/// Stops the scan when dropped by a panic: a worker that panics leaves its chunk undelivered,
/// and a sink that panics delivers nothing more, so without it the other workers would wait
/// for the window forever and the scope would never end. The panic then goes on.
struct StopOnPanic<'a>(&'a Shared);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.stop();
        }
    }
}

impl Shared {
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        {
            let _guard = self.delivered.lock();
            self.advanced.notify_all();
        }
        for release in &self.released {
            let _guard = release.done.lock();
            release.ready.notify_all();
        }
    }

    /// The sink is done with an output of `worker` (when workers wait for it).
    fn release(&self, worker: usize) {
        if let Some(release) = self.released.get(worker) {
            if let Ok(mut done) = release.done.lock() {
                *done += 1;
            }
            release.ready.notify_one();
        }
    }

    /// Waits until the sink is done with `worker`'s first `sent` outputs; `false` if the scan
    /// stopped meanwhile.
    fn wait_released(&self, worker: usize, sent: u64) -> bool {
        let Some(release) = self.released.get(worker) else {
            return false;
        };
        let Ok(mut done) = release.done.lock() else {
            return false;
        };
        while *done < sent {
            if self.stop.load(Ordering::Relaxed) {
                return false;
            }
            done = match release.ready.wait(done) {
                Ok(guard) => guard,
                Err(_) => return false,
            };
        }
        !self.stop.load(Ordering::Relaxed)
    }

    fn delivered(&self, count: usize) {
        if let Ok(mut delivered) = self.delivered.lock() {
            *delivered = count;
        }
        self.advanced.notify_all();
    }

    /// Waits until chunk `index` is within `window` of the chunks delivered; `false` if the
    /// scan stopped meanwhile.
    fn wait_for_window(&self, index: usize, window: usize) -> bool {
        let Ok(mut delivered) = self.delivered.lock() else {
            return false;
        };
        while index >= *delivered + window {
            if self.stop.load(Ordering::Relaxed) {
                return false;
            }
            delivered = match self.advanced.wait(delivered) {
                Ok(guard) => guard,
                Err(_) => return false,
            };
        }
        !self.stop.load(Ordering::Relaxed)
    }
}

/// Whether bit `number` of `bitmap` is set (bits past the bitmap are clear).
fn is_set(bitmap: &[u8], number: u64) -> bool {
    usize::try_from(number / 8)
        .ok()
        .and_then(|byte| bitmap.get(byte))
        .is_some_and(|byte| byte & (1 << (number % 8)) != 0)
}

/// Classifies a slot read whole: a record (fixed up in place), zeros, or damage.
#[inline]
fn classify(slot: &mut [u8], number: u64, check_numbers: bool) -> Entry<'_> {
    if slot.first_chunk::<4>() == Some(b"FILE") {
        let header = match RecordHeader::read(slot) {
            Ok(header) => header,
            Err(error) => return Entry::Damaged { error, raw: slot },
        };
        if check_numbers && let Err(error) = header.check_number(number) {
            return Entry::Damaged { error, raw: slot };
        }
        if let Err(error) = apply_update_sequence(
            slot,
            usize::from(header.update_sequence_offset),
            usize::from(header.update_sequence_count),
        ) {
            return Entry::Damaged { error, raw: slot };
        }
        return Entry::Record(Record::from_parts(header, slot));
    }
    if slot.iter().all(|byte| *byte == 0) {
        return Entry::Empty;
    }
    Entry::Damaged {
        error: FormatError::Signature {
            found: crate::bytes::field(slot, 0),
        },
        raw: slot,
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use std::{
        collections::HashMap,
        sync::Mutex,
        thread::{self, ThreadId},
    };

    use super::*;
    use crate::testing::corpus::{CorpusConfig, mft};

    /// Records in use per chunk, and the thread that counted them; a thread that begins a chunk
    /// while the sink still holds its last one is recorded in `overlaps`.
    struct InUse {
        held: Mutex<HashMap<ThreadId, bool>>,
        overlaps: AtomicUsize,
    }

    impl Projection for InUse {
        type Output = (u64, ThreadId);
        type Scratch = ();

        fn scratch(&self) {}

        fn begin(&self, (): &mut (), _: Chunk) -> (u64, ThreadId) {
            let me = thread::current().id();
            if self.held.lock().unwrap().insert(me, true) == Some(true) {
                self.overlaps.fetch_add(1, Ordering::Relaxed);
            }
            (0, me)
        }

        fn entry(&self, out: &mut (u64, ThreadId), (): &mut (), _: u64, entry: Entry<'_>) {
            if let Entry::Record(record) = entry {
                out.0 += u64::from(record.in_use());
            }
        }
    }

    impl InUse {
        fn new() -> Self {
            Self {
                held: Mutex::new(HashMap::new()),
                overlaps: AtomicUsize::new(0),
            }
        }

        /// The sink is done with an output of `thread`.
        fn done(&self, thread: ThreadId) {
            self.held.lock().unwrap().insert(thread, false);
        }
    }

    /// Waiting for the sink delivers what not waiting does, in either order; no worker begins a
    /// chunk while the sink holds its last output; and a sink that stops, or a cancel, ends the
    /// scan as it does without waiting.
    #[test]
    fn workers_wait_for_the_sink() {
        let (bytes, _) = mft(&CorpusConfig::realistic(6_000));
        let mft = Mft::from_file(bytes.as_slice()).unwrap();
        let base = Scan::new(&mft).chunk_bytes(32 << 10);
        let expected: Vec<u64> = base
            .collect(&InUse::new())
            .unwrap()
            .into_iter()
            .map(|(count, _)| count)
            .collect();
        assert!(expected.len() > 100);
        for threads in [1, 3, 8] {
            for ordered in [true, false] {
                let projection = InUse::new();
                let scan = base.threads(threads).ordered(ordered).wait_for_sink(true);
                let mut counts = vec![None; expected.len()];
                let summary = scan
                    .run(&projection, |chunk, (count, thread)| {
                        counts[chunk.index] = Some(count);
                        // The sink holds the output for a while: the worker must not go on.
                        thread::yield_now();
                        projection.done(thread);
                        ControlFlow::Continue(())
                    })
                    .unwrap();
                assert_eq!(summary.chunks, expected.len());
                let counts: Vec<u64> = counts.into_iter().map(Option::unwrap).collect();
                assert_eq!(counts, expected, "{threads} threads, ordered {ordered}");
                assert_eq!(projection.overlaps.load(Ordering::Relaxed), 0);
            }
        }
        // Without waiting, workers do run ahead of the sink: the check above can fail.
        let projection = InUse::new();
        Scan::new(&mft)
            .chunk_bytes(32 << 10)
            .threads(4)
            .run(&projection, |_, (_, thread)| {
                thread::sleep(std::time::Duration::from_millis(1));
                projection.done(thread);
                ControlFlow::Continue(())
            })
            .unwrap();
        assert!(projection.overlaps.load(Ordering::Relaxed) > 0);
        // A stop, and a cancel, end it.
        for ordered in [true, false] {
            let scan = base.threads(4).ordered(ordered).wait_for_sink(true);
            let mut seen = 0;
            let summary = scan
                .run(&InUse::new(), |_, _| {
                    seen += 1;
                    if seen == 10 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .unwrap();
            assert!(summary.stopped);
            assert_eq!(summary.chunks, 10);
            let cancel = AtomicBool::new(false);
            let result = scan.cancel(&cancel).run(&InUse::new(), |_, _| {
                cancel.store(true, Ordering::Relaxed);
                ControlFlow::Continue(())
            });
            assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        }
    }
}
