use std::{
    io,
    io::{Read, Seek, Write},
};
use tempfile::SpooledTempFile;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Represents a stream of data that is either coming in asynchronously from a remote source or from
/// a synchronous location (like the filesystem).
///
/// It is often useful to make this distinction because reading from a remote source is often slower
/// than reading synchronously (from disk or memory).
pub enum StreamingOrLocal {
    /// Represents an asynchronous stream of data.
    Streaming(Box<dyn AsyncRead + Unpin + Send>),

    /// Represents a synchronous stream of data.
    Local(Box<dyn Read + Send>),
}

impl StreamingOrLocal {
    /// Constructs a new [`StreamingOrLocal`] from a asynchronous source.
    pub fn from_streaming<R: AsyncRead + Unpin + Send + 'static>(data: impl Into<Box<R>>) -> Self {
        Self::Streaming(data.into())
    }

    /// Constructs a new [`StreamingOrLocal`] from a synchronous source.
    pub fn from_local<R: Read + Send + 'static>(data: impl Into<Box<R>>) -> Self {
        Self::Local(data.into())
    }

    /// Stream in the contents of the stream and make sure we have a fast locally accessible stream.
    ///
    /// If the stream is already local this will simply return that stream. If however the file is
    /// remote it will first be read to a temporary spooled file.
    pub async fn into_local(self) -> io::Result<Box<dyn Read + Send>> {
        match self {
            StreamingOrLocal::Streaming(mut stream) => {
                // Create a [`SpooledTempFile`] which is a blob of memory that is kept in memory if
                // it does not grow beyond 5MB, otherwise it is written to disk.
                let mut local_file = SpooledTempFile::new(5 * 1024 * 1024);

                // Stream in the bytes and copy them to the temporary file.
                let mut buf = [0u8; 1024 * 8];
                loop {
                    let bytes_read = stream.read(&mut buf).await?;
                    if bytes_read == 0 {
                        break;
                    }
                    local_file.write_all(&buf)?;
                }

                // Restart the file from the start so we can start reading from it.
                local_file.rewind()?;

                Ok(Box::new(local_file))
            }
            StreamingOrLocal::Local(stream) => Ok(stream),
        }
    }
}
