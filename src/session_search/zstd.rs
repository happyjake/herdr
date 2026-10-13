//! Stream every frame in an archived rollout without a subprocess or native library.

use std::io::{self, BufRead, Read};

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

pub(super) struct Decoder<R: BufRead> {
    source: Option<R>,
    frame: Option<StreamingDecoder<R, FrameDecoder>>,
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
                self.source = Some(frame.into_inner());
            }
            let Some(mut source) = self.source.take() else {
                return Ok(0);
            };
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.frame = Some(StreamingDecoder::new(source).map_err(io::Error::other)?);
        }
    }
}
