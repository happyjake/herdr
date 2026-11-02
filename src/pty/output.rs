//! Byte positions advance on PTY reads; retention is enabled only by a caller.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

const CAPTURE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutputPosition {
    epoch: u64,
    offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutputCapture {
    position: OutputPosition,
    bytes: VecDeque<u8>,
}

pub(crate) struct OutputRead {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(crate) struct OutputJournal(Mutex<(OutputPosition, Option<VecDeque<u8>>)>);

impl Default for OutputJournal {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(1);
        NEXT.fetch_max(now, Ordering::Relaxed);
        let epoch = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(Mutex::new((OutputPosition { epoch, offset: 0 }, None)))
    }
}

impl OutputJournal {
    /// One operation per read chunk; inactive captures copy no output bytes.
    pub(crate) fn record(&self, bytes: &[u8]) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.0.offset = state.0.offset.saturating_add(bytes.len() as u64);
        if let Some(retained) = &mut state.1 {
            let bytes = &bytes[bytes.len().saturating_sub(CAPTURE_BYTES)..];
            let remove = retained
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(CAPTURE_BYTES);
            retained.drain(..remove);
            retained.extend(bytes);
        }
    }

    pub(crate) fn begin(&self) -> OutputPosition {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.1 = Some(VecDeque::new());
        state.0
    }

    pub(crate) fn read_after(&self, position: OutputPosition) -> Option<OutputRead> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if position.epoch != state.0.epoch || position.offset > state.0.offset {
            return None;
        }
        let bytes = state.1.as_ref()?;
        let first = state.0.offset.saturating_sub(bytes.len() as u64);
        let skip = position.offset.saturating_sub(first) as usize;
        Some(OutputRead {
            bytes: bytes.iter().skip(skip).copied().collect(),
            truncated: position.offset < first,
        })
    }

    pub(crate) fn end(&self) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).1 = None;
    }

    pub(crate) fn snapshot(&self) -> Option<OutputCapture> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        Some(OutputCapture {
            position: state.0,
            bytes: state.1.as_ref()?.clone(),
        })
    }

    pub(crate) fn restore(&self, mut capture: OutputCapture) {
        let remove = capture.bytes.len().saturating_sub(CAPTURE_BYTES);
        capture.bytes.drain(..remove);
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) =
            (capture.position, Some(capture.bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_position_excludes_prior_identical_bytes_even_after_truncation() {
        let output = OutputJournal::default();
        output.record(b"debrief v1: 0 memories saved\n");
        assert!(output.snapshot().is_none());
        let start = output.begin();
        output.record(&vec![b'x'; CAPTURE_BYTES + 100]);
        output.record(b"\ndebrief v1: 0 memories saved\n");
        let reply = output.read_after(start).unwrap();
        assert!(reply.truncated);
        assert_eq!(reply.bytes.len(), CAPTURE_BYTES);
        assert!(reply.bytes.ends_with(b"\ndebrief v1: 0 memories saved\n"));
        let next = output.begin();
        assert!(next.offset > start.offset);
        assert!(output.read_after(next).unwrap().bytes.is_empty());
        output.end();
        assert!(output.snapshot().is_none());
    }

    #[test]
    fn output_position_and_bytes_survive_handoff_but_reject_another_runtime() {
        let output = OutputJournal::default();
        let start = output.begin();
        output.record(b"first");
        let restored = OutputJournal::default();
        assert!(restored.read_after(start).is_none());
        let json = serde_json::to_string(&output.snapshot().unwrap()).unwrap();
        restored.restore(serde_json::from_str(&json).unwrap());
        restored.record(b" second");
        assert_eq!(restored.read_after(start).unwrap().bytes, b"first second");
    }
}
