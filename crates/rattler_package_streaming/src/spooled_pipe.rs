//! A single-producer single-consumer pipe that connects an asynchronous
//! producer (a package download) to a synchronous consumer (an extractor
//! running on a blocking thread), buffering in memory up to a limit and
//! spilling to an unnamed temporary file beyond that.
//!
//! The pipe is designed around two hard constraints learned from extracting
//! packages directly from the network:
//!
//! 1. The writer never waits for the reader. If the extractor applied
//!    backpressure to the HTTP stream, servers reset the stream whenever
//!    concurrent extractions saturate the CPU. Data the reader has not yet
//!    consumed is kept in memory up to the configured limit and spills to an
//!    unnamed temporary file beyond that, so the download always proceeds at
//!    network speed. The spill file lives in the OS page cache; physical disk
//!    I/O only happens under memory pressure.
//! 2. The reader is a plain blocking [`Read`] + [`Seek`] that only ever
//!    touches memory, a [`std::fs::File`] and a condition variable. It never
//!    blocks on the async runtime, making it safe to use on a
//!    `spawn_blocking` thread without risking the deadlocks async-to-sync
//!    bridges are prone to.
//! 3. The writer never touches tokio's blocking pool either (see
//!    [`SpooledPipeWriter::write`]). Readers routinely occupy blocking-pool
//!    threads while waiting for data, so any pool dependency on the write
//!    path deadlocks once the pool is saturated with waiting readers.
//!
//! All data is retained until the pipe is dropped, so the reader can seek
//! backwards over data it already consumed. The zip data-descriptor fallback
//! relies on this to re-read the package from the start without downloading
//! it a second time.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

/// State shared between the reader and the writer.
struct State {
    /// The first `memory_limit` bytes of the stream. Append-only, so the
    /// reader can copy out of it without invalidation.
    memory: Vec<u8>,
    /// Spill file holding all bytes past `memory_limit`. Created lazily on
    /// the first spill. Byte `memory_limit + n` of the stream lives at offset
    /// `n` in this file.
    file: Option<Arc<Mutex<File>>>,
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

/// Creates a new spooled pipe that keeps at most `memory_limit` bytes in
/// memory before spilling to an unnamed temporary file.
pub(crate) fn spooled_pipe(memory_limit: usize) -> (SpooledPipeWriter, SpooledPipeReader) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            memory: Vec::new(),
            file: None,
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
    /// Appends `data` to the pipe. Data within the memory limit is committed
    /// directly; the remainder is written to the spill file.
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
        // Commit the prefix that still fits within the memory limit.
        let (spilled_offset, spill_start) = {
            let mut state = self.shared.lock();
            let take = (self.shared.memory_limit - state.memory.len()).min(data.len());
            if take > 0 {
                state.memory.extend_from_slice(&data[..take]);
                state.committed += take as u64;
                self.shared.progress.notify_all();
            }
            (state.committed, take)
        };
        if spill_start == data.len() {
            return Ok(());
        }

        // Write the remainder to the spill file. The file is shared with the
        // reader under a mutex and has no stable cursor, so every access
        // seeks explicitly.
        let file = self.spill_file()?;
        let offset = spilled_offset - self.shared.memory_limit as u64;
        {
            let mut file = file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&data[spill_start..])?;
        }

        let mut state = self.shared.lock();
        state.committed += (data.len() - spill_start) as u64;
        self.shared.progress.notify_all();
        drop(state);

        Ok(())
    }

    /// Returns the spill file, creating it on first use.
    fn spill_file(&self) -> std::io::Result<Arc<Mutex<File>>> {
        if let Some(file) = self.shared.lock().file.clone() {
            return Ok(file);
        }
        let file = Arc::new(Mutex::new(tempfile::tempfile()?));
        self.shared.lock().file = Some(file.clone());
        Ok(file)
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

/// The synchronous read half of the pipe. Reads block until the writer
/// commits more data, finishes, or fails. Seeking is supported over the
/// entire stream; `SeekFrom::End` blocks until the writer is done.
pub(crate) struct SpooledPipeReader {
    shared: Arc<Shared>,
    pos: u64,
}

impl SpooledPipeReader {
    /// Blocks until the writer finished and returns the total stream length.
    fn total_len(&self) -> std::io::Result<u64> {
        let mut state = self.shared.lock();
        loop {
            if let Some(error) = &state.error {
                return Err(clone_error(error));
            }
            if state.eof {
                return Ok(state.committed);
            }
            state = self
                .shared
                .progress
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
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

        let memory_len = state.memory.len() as u64;
        if self.pos < memory_len {
            let start = usize::try_from(self.pos).expect("memory positions fit in usize");
            let len = out.len().min(state.memory.len() - start);
            out[..len].copy_from_slice(&state.memory[start..start + len]);
            self.pos += len as u64;
            return Ok(len);
        }

        // Committed data past the in-memory prefix is always on disk.
        let len = usize::try_from(state.committed - self.pos)
            .unwrap_or(usize::MAX)
            .min(out.len());
        let file = state
            .file
            .clone()
            .expect("committed data past the memory limit must have a spill file");
        let offset = self.pos - self.shared.memory_limit as u64;
        drop(state);

        let mut file = file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut out[..len])?;
        drop(file);

        self.pos += len as u64;
        Ok(len)
    }
}

impl Seek for SpooledPipeReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.total_len()?) + i128::from(delta),
        };
        self.pos = u64::try_from(new_pos).map_err(|_out_of_range| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot seek before the start of the stream",
            )
        })?;
        Ok(self.pos)
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
    use std::{
        io::{Read, Seek, SeekFrom},
        time::Duration,
    };

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

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_spills_to_disk_above_limit() {
        let data = test_data(1024 * 1024);
        let (mut writer, reader) = spooled_pipe(64 * 1024);
        let consumer = read_to_end_blocking(reader);

        write_chunks(&mut writer, &data, 13 * 1024);
        assert!(
            writer.shared.lock().file.is_some(),
            "data above the memory limit must spill to a file"
        );
        writer.finish();

        let (_, result) = consumer.await.unwrap();
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
                        // every pipe to spill.
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

    /// Seeking back to the start after consuming the stream re-reads the
    /// retained data. The zip data-descriptor fallback depends on this.
    #[tokio::test(flavor = "multi_thread")]
    async fn seek_back_and_reread() {
        let data = test_data(512 * 1024);
        let (mut writer, reader) = spooled_pipe(64 * 1024);

        write_chunks(&mut writer, &data, 32 * 1024);
        writer.finish();

        let (mut reader, result) = read_to_end_blocking(reader).await.unwrap();
        assert_eq!(result.unwrap(), data);

        let total = data.len() as u64;
        let (reader, tail) = tokio::task::spawn_blocking(move || {
            assert_eq!(reader.seek(SeekFrom::End(-10)).unwrap(), total - 10);
            let mut tail = Vec::new();
            reader.read_to_end(&mut tail).unwrap();
            (reader, tail)
        })
        .await
        .unwrap();
        assert_eq!(tail, data[data.len() - 10..]);

        let (_, result) = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            reader.seek(SeekFrom::Start(0)).unwrap();
            let mut out = Vec::new();
            let result = reader.read_to_end(&mut out).map(|_| out);
            (reader, result)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), data);
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
