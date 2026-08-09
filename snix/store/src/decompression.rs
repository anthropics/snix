use std::{
    io::{self, Cursor},
    pin::Pin,
    task::{Context, Poll},
};

use async_compression::tokio::bufread::{BzDecoder, GzipDecoder, XzDecoder, ZstdDecoder};
use pin_project::pin_project;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, ReadBuf};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const BZIP2_MAGIC: [u8; 3] = *b"BZh";
const XZ_MAGIC: [u8; 6] = [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const BYTES_NEEDED: usize = 6;

#[derive(Debug, Clone, Copy)]
enum Algorithm {
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Algorithm {
    fn from_magic(magic: &[u8]) -> Option<Self> {
        if magic.starts_with(&GZIP_MAGIC) {
            Some(Self::Gzip)
        } else if magic.starts_with(&BZIP2_MAGIC) {
            Some(Self::Bzip2)
        } else if magic.starts_with(&XZ_MAGIC) {
            Some(Self::Xz)
        } else if magic.starts_with(&ZSTD_MAGIC) {
            Some(Self::Zstd)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct SmallBuf<const N: usize> {
    data_len: u8,
    buf: [u8; N],
}

impl<const N: usize> AsRef<[u8]> for SmallBuf<N> {
    fn as_ref(&self) -> &[u8] {
        &self.buf[..self.data_len as usize]
    }
}

impl<const N: usize> std::ops::Deref for SmallBuf<N> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.buf[..self.data_len as usize]
    }
}

type WithPreexistingBuffer<R> = tokio::io::Chain<io::Cursor<SmallBuf<BYTES_NEEDED>>, R>;

#[pin_project(project = DecompressedReaderProj)]
pub enum DecompressedReader<R> {
    Unknown(#[pin] WithPreexistingBuffer<R>),
    Gzip(#[pin] GzipDecoder<WithPreexistingBuffer<R>>),
    Bzip(#[pin] BzDecoder<WithPreexistingBuffer<R>>),
    Xz(#[pin] XzDecoder<WithPreexistingBuffer<R>>),
    Zstd(#[pin] ZstdDecoder<WithPreexistingBuffer<R>>),
}

impl<R: AsyncRead + Unpin> DecompressedReader<R> {
    /// Reads up to `BYTES_NEEDED` bytes from the underlying reader,
    /// and returns those Bytes as a SmallBuf, as well as a reader allowing to read all data
    async fn buffer_magic(
        mut r: R,
    ) -> std::io::Result<(SmallBuf<BYTES_NEEDED>, WithPreexistingBuffer<R>)> {
        let mut buf = [0; BYTES_NEEDED];
        let mut buf_filled = 0;

        while buf_filled < BYTES_NEEDED {
            let bytes_read = r.read(&mut buf).await?;
            // EOF while filling buf
            if bytes_read == 0 {
                tracing::trace!("got EOF while filling buffer");
                break;
            }
            buf_filled += bytes_read;
        }

        let buf = SmallBuf {
            data_len: buf_filled as u8,
            buf,
        };

        Ok((buf.clone(), Cursor::new(buf).chain(r)))
    }
}

impl<R: AsyncBufRead + Unpin> DecompressedReader<R> {
    /// Checks the passed reader for a suitable compression magic to be present,
    /// pulling up to 6 bytes of data.
    /// If there is, returns a reader which allows reading decompressed data.
    /// Else, returns a reader that reads the uncompressed/undetected data (including the up to 6 bytes).
    pub async fn new(reader: R) -> std::io::Result<Self> {
        let (buffer, r) = Self::buffer_magic(reader).await?;

        // r.buffer is guaranteed to have at least BYTES_NEEDED bytes if not EOF before.
        Ok(match Algorithm::from_magic(&buffer) {
            Some(Algorithm::Gzip) => Self::Gzip(GzipDecoder::new(r)),
            Some(Algorithm::Bzip2) => Self::Bzip(BzDecoder::new(r)),
            Some(Algorithm::Xz) => Self::Xz(XzDecoder::new(r)),
            Some(Algorithm::Zstd) => Self::Zstd(ZstdDecoder::new(r)),
            None => Self::Unknown(r),
        })
    }

    /// The decoders don't implement [AsyncBufRead], so we cannot implement it for [DecompressedReader].
    /// However, in the unknown case we only wrap R (and the buffer we read so far), they are [AsyncBufRead].
    /// This provides a way to get a mutable reference to it.
    #[allow(unused)]
    pub fn as_unknown_inner(&mut self) -> Option<&mut WithPreexistingBuffer<R>> {
        if let Self::Unknown(r) = self {
            Some(r)
        } else {
            None
        }
    }
}

impl<R> AsyncRead for DecompressedReader<R>
where
    R: AsyncBufRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            DecompressedReaderProj::Unknown(inner) => inner.poll_read(cx, buf),
            DecompressedReaderProj::Gzip(inner) => inner.poll_read(cx, buf),
            DecompressedReaderProj::Bzip(inner) => inner.poll_read(cx, buf),
            DecompressedReaderProj::Xz(inner) => inner.poll_read(cx, buf),
            DecompressedReaderProj::Zstd(inner) => inner.poll_read(cx, buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use async_compression::tokio::bufread::GzipEncoder;
    use futures::TryStreamExt;
    use rstest::rstest;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio_tar::Archive;

    use super::*;

    #[tokio::test]
    async fn gzip() {
        let data = b"abcdefghijk";
        let mut enc = GzipEncoder::new(&data[..]);
        let mut gzipped = vec![];
        enc.read_to_end(&mut gzipped).await.unwrap();

        let mut reader = DecompressedReader::new(BufReader::new(&gzipped[..]))
            .await
            .expect("new to succeed");
        let mut round_tripped = vec![];
        reader.read_to_end(&mut round_tripped).await.unwrap();

        assert_eq!(data[..], round_tripped[..]);
    }

    #[rstest]
    #[case::gzip(include_bytes!("testdata/blob.tar.gz"))]
    #[case::bzip2(include_bytes!("testdata/blob.tar.bz2"))]
    #[case::xz(include_bytes!("testdata/blob.tar.xz"))]
    #[case::zstd(include_bytes!("testdata/blob.tar.zst"))]
    #[tokio::test]
    async fn compressed_tar(#[case] data: &[u8]) {
        let reader = DecompressedReader::new(BufReader::new(data))
            .await
            .expect("new to succeed");
        let mut archive = Archive::new(reader);
        let mut entries: Vec<_> = archive.entries().unwrap().try_collect().await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path().unwrap().as_ref(), Path::new("empty"));
        let mut data = String::new();
        entries[0].read_to_string(&mut data).await.unwrap();
        assert_eq!(data, "");
    }

    #[tokio::test]
    async fn unknown() {
        let data = b"abcdefghijk";
        let mut reader = DecompressedReader::new(BufReader::new(Cursor::new(data)))
            .await
            .expect("new to succeed");

        // this is expected to implement AsyncBufRead, so we can do the following:
        let inner_reader = reader.as_unknown_inner().expect("to be some");
        let _ = inner_reader.fill_buf().await;

        // ... but we should also be able to just read from the outer:
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .await
            .expect("read_to_end to not fail");

        assert_eq!(data[..], buf[..], "read data should match");
    }
}
