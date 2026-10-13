//! Stream every frame in an archived rollout without a subprocess or native library.

use std::io::{self, BufRead, Chain, Cursor, Read};

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

/// Bound the decoder's history window before it can buffer any block output.
const MAX_WINDOW_BYTES: u64 = 16 * 1024 * 1024;
type FrameInput<R> = Chain<Cursor<Vec<u8>>, R>;

pub(super) struct Decoder<R: BufRead> {
    source: Option<R>,
    frame: Option<StreamingDecoder<FrameInput<R>, FrameDecoder>>,
}

impl<R: BufRead> Decoder<R> {
    pub fn new(source: R) -> Self {
        Self {
            source: Some(source),
            frame: None,
        }
    }
}

impl<R: BufRead> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(frame) = &mut self.frame {
                let read = frame.read(buf)?;
                if read > 0 {
                    return Ok(read);
                }
            }
            if let Some(frame) = self.frame.take() {
                self.source = Some(frame.into_inner().into_inner().1);
            }
            let Some(mut source) = self.source.take() else {
                return Ok(0);
            };
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            let Some(header) = checked_header(&mut source)? else {
                return Ok(0);
            };
            // Replay only the small validated header. The decoder still checks
            // the complete format, but never sees an unbounded window.
            self.frame = Some(
                StreamingDecoder::new(Cursor::new(header).chain(source))
                    .map_err(io::Error::other)?,
            );
        }
    }
}

fn checked_header(source: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut header = vec![0; 4];
    loop {
        if source.fill_buf()?.is_empty() {
            return Ok(None);
        }
        source.read_exact(&mut header)?;
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if !(0x184d2a50..=0x184d2a5f).contains(&magic) {
            break;
        }
        let mut size = [0; 4];
        source.read_exact(&mut size)?;
        let mut remaining = u64::from(u32::from_le_bytes(size));
        while remaining > 0 {
            let available = source.fill_buf()?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated zstd metadata frame",
                ));
            }
            let taken = remaining.min(available.len() as u64) as usize;
            source.consume(taken);
            remaining -= taken as u64;
        }
    }
    if header != [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid zstd frame magic",
        ));
    }
    read_header_bytes(source, &mut header, 1)?;
    let descriptor = header[4];
    if descriptor & 0x08 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reserved zstd frame bit is set",
        ));
    }
    let single_segment = descriptor & 0x20 != 0;
    let window = if single_segment {
        None
    } else {
        read_header_bytes(source, &mut header, 1)?;
        let base = 1_u64 << (10 + (header[5] >> 3));
        Some(base + (base / 8) * u64::from(header[5] & 7))
    };
    let dictionary_bytes = [0, 1, 2, 4][usize::from(descriptor & 3)];
    read_header_bytes(source, &mut header, dictionary_bytes)?;
    let size_bytes = match descriptor >> 6 {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let size_start = header.len();
    read_header_bytes(source, &mut header, size_bytes)?;
    let mut size = [0; 8];
    size[..size_bytes].copy_from_slice(&header[size_start..]);
    let content_size = u64::from_le_bytes(size) + if size_bytes == 2 { 256 } else { 0 };
    if window.unwrap_or(content_size) > MAX_WINDOW_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zstd window exceeds catalog limit",
        ));
    }
    Ok(Some(header))
}

fn read_header_bytes(source: &mut impl Read, header: &mut Vec<u8>, count: usize) -> io::Result<()> {
    let start = header.len();
    header.resize(start + count, 0);
    source.read_exact(&mut header[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct Counted {
        input: Cursor<Vec<u8>>,
        bytes: Arc<AtomicUsize>,
    }

    impl Read for Counted {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let read = self.input.read(out)?;
            self.bytes.fetch_add(read, Ordering::SeqCst);
            Ok(read)
        }
    }

    impl BufRead for Counted {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.input.fill_buf()
        }
        fn consume(&mut self, count: usize) {
            self.input.consume(count);
            self.bytes.fetch_add(count, Ordering::SeqCst);
        }
    }

    fn rle_frame(size: u32) -> Vec<u8> {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
        frame.extend_from_slice(&size.to_le_bytes());
        let mut remaining = size;
        while remaining > 0 {
            let block = remaining.min(128 * 1024);
            remaining -= block;
            let header = (block << 3) | 2 | u32::from(remaining == 0);
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
            frame.push(b' ');
        }
        frame
    }

    #[test]
    fn search_sessions_zstd_bounds_windows_before_reading_compressed_blocks() {
        let mut small = Decoder::new(Cursor::new(rle_frame(1024)));
        let mut byte = [0];
        small.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b' ']);

        let frame = rle_frame(32 * 1024 * 1024);
        assert_eq!(frame.len(), 1033);
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut decoder = Decoder::new(Counted {
            input: Cursor::new(frame),
            bytes: Arc::clone(&bytes),
        });
        let error = decoder
            .read(&mut byte)
            .expect_err("large window must be refused before decoding");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            bytes.load(Ordering::SeqCst) <= 18,
            "compressed blocks were read before the window was bounded"
        );

        // A streaming frame declares its window separately from content size.
        let mut streaming = Decoder::new(Cursor::new(vec![
            0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x78, 0x0b, 0, 0, b' ',
        ]));
        assert_eq!(
            streaming.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
