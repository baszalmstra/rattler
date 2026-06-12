//! A single-producer single-consumer pipe that connects an asynchronous
//! producer (a package download) to a synchronous consumer (an extractor
//! running on a blocking thread), buffering a sliding window in memory and
//! spilling to an unnamed temporary file only when the consumer falls behind.
//!
//! The pipe is designed around three hard constraints learned from extracting
//! packages directly from the network:
//!
//! 1. The writer never waits for the reader. If the extractor applied
//!    backpressure to the HTTP stream, servers reset the stream whenever
//!    concurrent extractions saturate the CPU. The most recent data is kept
//!    in a sliding in-memory window; when the window overflows, data the
//!    reader has already consumed is discarded and only unconsumed data is
//!    flushed to an unnamed temporary file. A reader that keeps up with the
//!    download therefore never touches the disk at all, no matter how large
//!    the package is, and a lagging reader costs only the backlog, not the
//!    whole stream. The spill file lives in the OS page cache; physical disk
//!    I/O only happens under memory pressure.
//! 2. The reader is a plain blocking [`Read`] that only ever
//!    touches memory, a [`std::fs::File`] and a condition variable. It never
//!    blocks on the async runtime, making it safe to use on a
//!    `spawn_blocking` thread without risking the deadlocks async-to-sync
//!    bridges are prone to.
//! 3. The writer never touches tokio's blocking pool either (see
//!    [`SpooledPipeWriter::write`]). Readers routinely occupy blocking-pool
//!    threads while waiting for data, so any pool dependency on the write
//!    path deadlocks once the pool is saturated with waiting readers.
//!
//! The pipe is strictly sequential: extraction that needs random access (the
//! zip data-descriptor case, which is driven by the central directory at the
//! *end* of the archive) cannot overlap the download anyway and downloads
//! into a seekable spooled temporary file instead of a pipe.

use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

/// Writes the whole buffer at the given offset without touching a shared file
/// cursor, so the reader and the writer can access the spill file
/// concurrently without locking.
#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let written = file.seek_write(buf, offset)?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[written..];
        offset += written as u64;
    }
    Ok(())
}

/// Fills the whole buffer from the given offset; the counterpart of
/// [`write_all_at`].
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let read = file.seek_read(buf, offset)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[read..];
        offset += read as u64;
    }
    Ok(())
}

/// State shared between the reader and the writer.
struct State {
    /// Sliding in-memory window holding stream bytes
    /// `[window_start, committed)`.
    window: VecDeque<u8>,
    /// Stream offset of the first byte in `window`.
    window_start: u64,
    /// The reader's position, published so the writer can discard consumed
    /// window overflow instead of flushing it.
    reader_pos: u64,
    /// Spill file for window overflow the reader has not consumed yet.
    /// Flushed runs are packed back to back (see `flush_stream_start`), so
    /// the file never contains holes for discarded data — important on
    /// Windows, where NTFS physically zero-fills gaps instead of keeping
    /// them sparse. Created lazily.
    file: Option<Arc<File>>,
    /// Stream offset of the first byte of the current flushed run; the run
    /// occupies file offsets `[flush_file_start, file_len)`. Because the
    /// reader is strictly sequential, a new run only ever starts after the
    /// previous run was fully consumed, so a single live mapping suffices.
    flush_stream_start: u64,
    /// File offset of the first byte of the current flushed run.
    flush_file_start: u64,
    /// Number of bytes written to the spill file so far.
    file_len: u64,
    /// Total number of bytes committed to the pipe and readable.
    committed: u64,
    /// The writer finished successfully; `committed` is the final length.
    eof: bool,
    /// The writer failed. The reader observes this error once it consumed
    /// all committed data.
    error: Option<Arc<std::io::Error>>,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled whenever `committed`, `eof` or `error` changes.
    progress: Condvar,
    memory_limit: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Recreates an [`std::io::Error`] from a shared error so every reader
/// observes the original kind and message.
fn clone_error(error: &Arc<std::io::Error>) -> std::io::Error {
    std::io::Error::new(error.kind(), Arc::clone(error))
}

/// Creates a new spooled pipe that keeps a sliding window of at most
/// `memory_limit` bytes in memory.
pub(crate) fn spooled_pipe(memory_limit: usize) -> (SpooledPipeWriter, SpooledPipeReader) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            window: VecDeque::new(),
            window_start: 0,
            reader_pos: 0,
            file: None,
            flush_stream_start: 0,
            flush_file_start: 0,
            file_len: 0,
            committed: 0,
            eof: false,
            error: None,
        }),
        progress: Condvar::new(),
        memory_limit,
    });
    (
        SpooledPipeWriter {
            shared: shared.clone(),
            done: false,
        },
        SpooledPipeReader { shared, pos: 0 },
    )
}

/// The asynchronous write half of the pipe. Writes never wait for the
/// reader; dropping the writer without calling [`SpooledPipeWriter::finish`]
/// or [`SpooledPipeWriter::fail`] poisons the pipe with a
/// [`std::io::ErrorKind::BrokenPipe`] error.
pub(crate) struct SpooledPipeWriter {
    shared: Arc<Shared>,
    done: bool,
}

impl SpooledPipeWriter {
    /// Appends `data` to the pipe. The window overflow this causes is either
    /// discarded (already consumed by the reader) or flushed to the spill
    /// file, so a reader that keeps up never causes disk I/O.
    ///
    /// This is deliberately a synchronous, blocking-pool-free operation even
    /// though it is called from async code: spill writes go to an unlinked
    /// temporary file whose pages land in the OS page cache, which is
    /// microseconds-fast. Dispatching them through `spawn_blocking` instead
    /// deadlocks when the blocking pool is small: every pool thread can be
    /// occupied by an extraction blocked on this very pipe waiting for more
    /// data, leaving no thread to ever run the write that would unblock it
    /// (observed with rattler-bin's `max_blocking_threads(num_cores)`). The
    /// writer staying off the blocking pool guarantees downloads always run
    /// to completion, which in turn guarantees blocked readers always wake.
    pub(crate) fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut state = self.shared.lock();
        state.window.extend(data.iter().copied());
        state.committed += data.len() as u64;
        self.shared.progress.notify_all();

        // Evict the window overflow.
        while state.window.len() > self.shared.memory_limit {
            let excess = state.window.len() - self.shared.memory_limit;
            let consumed_front = state.reader_pos.saturating_sub(state.window_start);
            if consumed_front > 0 {
                // The reader already consumed the front of the window; drop
                // it without ever touching the disk.
                let drop_len = excess.min(usize::try_from(consumed_front).unwrap_or(usize::MAX));
                state.window.drain(..drop_len);
                state.window_start += drop_len as u64;
            } else {
                // Flush unconsumed overflow to the spill file. Copy it out
                // so it stays readable from the window until the write
                // completes, and write without holding the lock. Only the
                // writer mutates the window, so the front is unchanged when
                // the lock is re-acquired.
                let flush: Vec<u8> = state.window.iter().take(excess).copied().collect();
                // Drops since the last flush leave a gap in the stream;
                // start a new run packed directly after the previous one.
                // The previous run is necessarily fully consumed: drops only
                // happen below the reader position.
                if state.flush_stream_start + (state.file_len - state.flush_file_start)
                    != state.window_start
                {
                    state.flush_stream_start = state.window_start;
                    state.flush_file_start = state.file_len;
                }
                let offset = state.file_len;
                let file = if let Some(file) = &state.file {
                    file.clone()
                } else {
                    let file = Arc::new(tempfile::tempfile()?);
                    state.file = Some(file.clone());
                    file
                };
                drop(state);
                write_all_at(&file, &flush, offset)?;
                state = self.shared.lock();
                state.window.drain(..flush.len());
                state.window_start += flush.len() as u64;
                state.file_len += flush.len() as u64;
            }
        }

        Ok(())
    }

    /// Marks the pipe as complete; the reader observes end-of-file after
    /// consuming all committed data.
    pub(crate) fn finish(mut self) {
        self.done = true;
        let mut state = self.shared.lock();
        state.eof = true;
        self.shared.progress.notify_all();
    }

    /// Poisons the pipe with `error`; the reader observes it after consuming
    /// all committed data.
    pub(crate) fn fail(mut self, error: std::io::Error) {
        self.done = true;
        let mut state = self.shared.lock();
        state.error = Some(Arc::new(error));
        self.shared.progress.notify_all();
    }
}

impl Drop for SpooledPipeWriter {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Make sure a reader blocked on more data always wakes up, even when
        // the future driving the download is dropped mid-stream.
        let mut state = self.shared.lock();
        if !state.eof && state.error.is_none() {
            state.error = Some(Arc::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the download feeding this extraction was dropped before completing",
            )));
        }
        self.shared.progress.notify_all();
    }
}

/// The synchronous, strictly sequential read half of the pipe. Reads block
/// until the writer commits more data, finishes, or fails.
pub(crate) struct SpooledPipeReader {
    shared: Arc<Shared>,
    pos: u64,
}

/// Copies `window[start..start + out.len()]` into `out`.
fn copy_from_window(window: &VecDeque<u8>, start: usize, out: &mut [u8]) {
    let (front, back) = window.as_slices();
    let len = out.len();
    if start + len <= front.len() {
        out.copy_from_slice(&front[start..start + len]);
    } else if start >= front.len() {
        let back_start = start - front.len();
        out.copy_from_slice(&back[back_start..back_start + len]);
    } else {
        let split = front.len() - start;
        out[..split].copy_from_slice(&front[start..]);
        out[split..].copy_from_slice(&back[..len - split]);
    }
}

impl Read for SpooledPipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        let mut state = self.shared.lock();
        // Serve any committed data before surfacing end-of-file or a writer
        // error; the error belongs to the failure frontier of the stream.
        while self.pos >= state.committed {
            if let Some(error) = &state.error {
                return Err(clone_error(error));
            }
            if state.eof {
                return Ok(0);
            }
            state = self
                .shared
                .progress
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }

        if self.pos < state.window_start {
            // The reader lagged behind and the data it needs was flushed.
            // A sequential reader can never enter a discarded region: the
            // writer only discards data below the published reader position.
            let len = usize::try_from(state.window_start - self.pos)
                .unwrap_or(usize::MAX)
                .min(out.len());
            let file = state
                .file
                .clone()
                .expect("retained data below the window must have been flushed");
            // A lagging reader is always inside the current flushed run; the
            // writer cannot start a new one before this one is consumed.
            debug_assert!(self.pos >= state.flush_stream_start);
            let offset = state.flush_file_start + (self.pos - state.flush_stream_start);
            state.reader_pos = self.pos + len as u64;
            drop(state);

            read_exact_at(&file, &mut out[..len], offset)?;
            self.pos += len as u64;
            return Ok(len);
        }

        // Serve from the in-memory window.
        let start =
            usize::try_from(self.pos - state.window_start).expect("window offsets fit in usize");
        let len = usize::try_from(state.committed - self.pos)
            .unwrap_or(usize::MAX)
            .min(out.len());
        copy_from_window(&state.window, start, &mut out[..len]);
        self.pos += len as u64;
        state.reader_pos = self.pos;
        Ok(len)
    }
}

/// Drains `reader` into the pipe using `buffer_size` sized chunks. On success
/// the pipe is marked complete; on failure the error is forwarded into the
/// pipe (so the extraction observes it too) as well as returned.
pub(crate) async fn copy_to_pipe(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    mut writer: SpooledPipeWriter,
    buffer_size: usize,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut buf = vec![0u8; buffer_size];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                writer.finish();
                return Ok(());
            }
            Ok(len) => {
                if let Err(error) = writer.write(&buf[..len]) {
                    let result = std::io::Error::new(error.kind(), error.to_string());
                    writer.fail(error);
                    return Err(result);
                }
            }
            Err(error) => {
                let result = std::io::Error::new(error.kind(), error.to_string());
                writer.fail(error);
                return Err(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Read, time::Duration};

    use super::*;

    /// Deterministic pseudo-random test data.
    fn test_data(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 9) % 251) as u8)
            .collect()
    }

    fn write_chunks(writer: &mut SpooledPipeWriter, data: &[u8], chunk_size: usize) {
        for chunk in data.chunks(chunk_size) {
            writer.write(chunk).unwrap();
        }
    }

    fn read_to_end_blocking(
        mut reader: SpooledPipeReader,
    ) -> tokio::task::JoinHandle<(SpooledPipeReader, std::io::Result<Vec<u8>>)> {
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let result = reader.read_to_end(&mut out).map(|_| out);
            (reader, result)
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_stays_in_memory_below_limit() {
        let data = test_data(64 * 1024);
        let (mut writer, reader) = spooled_pipe(1024 * 1024);

        write_chunks(&mut writer, &data, 8 * 1024);
        assert!(
            writer.shared.lock().file.is_none(),
            "data below the memory limit must not create a spill file"
        );
        writer.finish();

        let (_, result) = read_to_end_blocking(reader).await.unwrap();
        assert_eq!(result.unwrap(), data);
    }

    /// The core property of the sliding window: a reader that keeps up with
    /// the writer never causes any disk I/O, no matter how large the stream.
    #[test]
    fn fast_consumer_keeps_large_stream_off_disk() {
        let data = test_data(2 * 1024 * 1024);
        let (mut writer, mut reader) = spooled_pipe(64 * 1024);

        let mut out = vec![0u8; 32 * 1024];
        for chunk in data.chunks(32 * 1024) {
            writer.write(chunk).unwrap();
            reader.read_exact(&mut out[..chunk.len()]).unwrap();
            assert_eq!(&out[..chunk.len()], chunk);
        }
        assert!(
            writer.shared.lock().file.is_none(),
            "a reader that keeps up must keep the stream entirely off the disk"
        );
        writer.finish();
    }

    /// Window overflow the reader has not consumed yet must be flushed to
    /// disk (never dropped) and re-readable.
    #[tokio::test(flavor = "multi_thread")]
    async fn unconsumed_overflow_spills_to_disk() {
        let data = test_data(1024 * 1024);
        let (mut writer, reader) = spooled_pipe(64 * 1024);

        write_chunks(&mut writer, &data, 13 * 1024);
        assert!(
            writer.shared.lock().file.is_some(),
            "unconsumed data above the memory limit must spill to a file"
        );
        writer.finish();

        let (_, result) = read_to_end_blocking(reader).await.unwrap();
        assert_eq!(result.unwrap(), data);
    }

    /// A slow consumer must never block the writer: the entire stream must be
    /// committable while the reader has not consumed a single byte.
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_consumer_does_not_block_writer() {
        let data = test_data(2 * 1024 * 1024);
        let (mut writer, reader) = spooled_pipe(16 * 1024);

        tokio::time::timeout(Duration::from_secs(30), async {
            write_chunks(&mut writer, &data, 64 * 1024);
            writer.finish();
        })
        .await
        .expect("the writer must not wait for the reader");

        let (_, result) = read_to_end_blocking(reader).await.unwrap();
        assert_eq!(result.unwrap(), data);
    }

    /// A slow producer (low download speed): the reader blocks until data
    /// trickles in and still observes the full stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_producer_blocks_reader_until_data_arrives() {
        let data = test_data(256 * 1024);
        let (mut writer, reader) = spooled_pipe(32 * 1024);
        let consumer = read_to_end_blocking(reader);

        for chunk in data.chunks(4 * 1024) {
            tokio::time::sleep(Duration::from_millis(2)).await;
            writer.write(chunk).unwrap();
        }
        writer.finish();

        let (_, result) = consumer.await.unwrap();
        assert_eq!(result.unwrap(), data);
    }

    /// Regression test for a deadlock observed against rattler-bin's runtime
    /// (`max_blocking_threads(num_cores)`): readers occupy blocking-pool
    /// threads while waiting for pipe data, so with more pipes than pool
    /// threads the pool is saturated by waiting readers. The writers must
    /// make progress without the blocking pool or nothing ever wakes the
    /// readers. Spill writes dispatched through `spawn_blocking` deadlocked
    /// here.
    #[test]
    fn no_deadlock_when_blocking_pool_is_saturated_by_readers() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let all_pipes = async {
            let pipes: Vec<_> = (0..6)
                .map(|_| {
                    tokio::spawn(async {
                        let data = test_data(256 * 1024);
                        // A memory limit far below the data size forces
                        // every pipe to evict its window.
                        let (mut writer, mut reader) = spooled_pipe(16 * 1024);
                        let consumer = tokio::task::spawn_blocking(move || {
                            let mut out = Vec::new();
                            reader.read_to_end(&mut out).map(|_| out)
                        });
                        // Trickle the data so readers spend most of their
                        // time blocked on the condition variable.
                        for chunk in data.chunks(16 * 1024) {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            writer.write(chunk).unwrap();
                        }
                        writer.finish();
                        assert_eq!(consumer.await.unwrap().unwrap(), data);
                    })
                })
                .collect::<Vec<_>>();
            for pipe in pipes {
                pipe.await.unwrap();
            }
        };

        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(60), all_pipes).await })
            .expect("the pipes deadlocked on a saturated blocking pool");
    }

    /// Alternating lag and catch-up phases create multiple flushed runs.
    /// They are packed back to back in the spill file — leaving holes for
    /// the discarded gaps would make NTFS physically zero-fill them — and
    /// the data must stay correct across run boundaries.
    #[test]
    fn flush_runs_are_packed_without_holes() {
        let data = test_data(1024 * 1024);
        let chunk = 32 * 1024;
        let (mut writer, mut reader) = spooled_pipe(2 * chunk);

        let mut received = Vec::new();
        let mut buf = vec![0u8; chunk];
        for (index, piece) in data.chunks(chunk).enumerate() {
            writer.write(piece).unwrap();
            // Phases of 8 chunks: stall the reader (flushing a run), then
            // catch up and consume in lock-step (discarding, which creates
            // the gap before the next run).
            let stalling = (index / 8) % 2 == 0;
            if !stalling {
                while received.len() < (index + 1) * chunk {
                    let read = reader.read(&mut buf).unwrap();
                    received.extend_from_slice(&buf[..read]);
                }
            }
        }
        let shared = writer.shared.clone();
        writer.finish();

        let file_len = {
            let state = shared.lock();
            let file = state.file.as_ref().expect("stalled phases must flush");
            assert_eq!(
                file.metadata().unwrap().len(),
                state.file_len,
                "the spill file must contain exactly the flushed bytes, without holes"
            );
            state.file_len
        };
        assert!(
            file_len < data.len() as u64 / 2,
            "discarded data must not have been written to the spill file"
        );

        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, data);
    }

    /// A writer failure is observed only after all committed data is served.
    #[tokio::test(flavor = "multi_thread")]
    async fn error_surfaces_after_committed_data() {
        let data = test_data(128 * 1024);
        let (mut writer, mut reader) = spooled_pipe(16 * 1024);

        write_chunks(&mut writer, &data, 16 * 1024);
        writer.fail(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "stream reset by peer",
        ));

        let error = tokio::task::spawn_blocking(move || {
            let mut out = vec![0u8; data.len()];
            reader.read_exact(&mut out).unwrap();
            assert_eq!(out, data);
            reader.read(&mut [0u8; 1]).unwrap_err()
        })
        .await
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        assert!(error.to_string().contains("stream reset by peer"));
    }

    /// Dropping the writer without finishing poisons the pipe so a blocked
    /// reader always wakes up.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_writer_unblocks_reader_with_error() {
        let (mut writer, mut reader) = spooled_pipe(16 * 1024);
        let data = test_data(8 * 1024);
        write_chunks(&mut writer, &data, 8 * 1024);

        let consumer = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            reader.read_to_end(&mut out).unwrap_err()
        });
        drop(writer);

        let error = consumer.await.unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
