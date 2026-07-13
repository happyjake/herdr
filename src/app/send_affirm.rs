//! Applied-send memory for `pane.send_input` re-affirmation.
//!
//! A client whose socket died before the ack cannot know whether its send
//! was applied; after reconnecting it re-issues the same request with the
//! same `send_id`. Remembering recently applied ids makes that re-issue
//! safe: a seen id is acknowledged without writing to the pane again, so
//! at-least-once delivery becomes exactly-once within the memory window.
//!
//! Ids are client-generated UUIDs — globally unique by construction — so
//! one bounded set serves every pane; there is nothing to key by pane, and
//! a pane moving or closing between issue and re-issue changes nothing.
use std::collections::{HashSet, VecDeque};

/// How many applied ids are remembered before the oldest is forgotten. A
/// client re-affirms at most its one unsettled send per pane, so the bound
/// only needs to outlast a reconnect window across every concurrent client.
const APPLIED_SEND_ID_CAPACITY: usize = 256;

#[derive(Default)]
pub(crate) struct AppliedSendIds {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl AppliedSendIds {
    pub(crate) fn contains(&self, send_id: &str) -> bool {
        self.seen.contains(send_id)
    }

    pub(crate) fn record(&mut self, send_id: String) {
        if !self.seen.insert(send_id.clone()) {
            return;
        }
        self.order.push_back(send_id);
        if self.order.len() > APPLIED_SEND_ID_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_id_is_contained_and_an_unknown_one_is_not() {
        let mut ids = AppliedSendIds::default();
        ids.record("a".into());
        assert!(ids.contains("a"));
        assert!(!ids.contains("b"));
    }

    #[test]
    fn re_recording_an_id_does_not_consume_capacity() {
        let mut ids = AppliedSendIds::default();
        for _ in 0..APPLIED_SEND_ID_CAPACITY * 2 {
            ids.record("same".into());
        }
        ids.record("other".into());
        assert!(ids.contains("same"));
        assert!(ids.contains("other"));
    }

    #[test]
    fn the_oldest_id_is_forgotten_past_capacity() {
        let mut ids = AppliedSendIds::default();
        for n in 0..=APPLIED_SEND_ID_CAPACITY {
            ids.record(format!("id-{n}"));
        }
        assert!(!ids.contains("id-0"));
        assert!(ids.contains("id-1"));
        assert!(ids.contains(&format!("id-{APPLIED_SEND_ID_CAPACITY}")));
    }
}
