use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use remote_steer_core::{FfbCommand, FfbReply, FfbReplyKind, WheelStateSnapshot};

#[derive(Debug, Clone)]
pub struct PendingFfbCommand {
    pub command: FfbCommand,
    pub attempts: u32,
    pub last_sent: Instant,
}

#[derive(Debug)]
pub struct FfbReliability {
    retry_after: Duration,
    pending: BTreeMap<u64, PendingFfbCommand>,
}

#[derive(Debug, Default)]
pub struct InputStaleDrop {
    last_seq: Option<u64>,
}

impl FfbReliability {
    pub fn new(retry_after: Duration) -> Self {
        Self {
            retry_after,
            pending: BTreeMap::new(),
        }
    }

    pub fn track_sent(&mut self, command: FfbCommand, now: Instant) {
        self.pending.insert(
            command.command_id,
            PendingFfbCommand {
                command,
                attempts: 1,
                last_sent: now,
            },
        );
    }

    pub fn apply_reply(&mut self, reply: &FfbReply) -> bool {
        match reply.kind {
            FfbReplyKind::Ack | FfbReplyKind::Rejected { .. } => {
                self.pending.remove(&reply.command_id).is_some()
            }
        }
    }

    pub fn due_retries(&mut self, now: Instant) -> Vec<FfbCommand> {
        let mut due = Vec::new();
        for pending in self.pending.values_mut() {
            if now.duration_since(pending.last_sent) >= self.retry_after {
                pending.attempts += 1;
                pending.last_sent = now;
                due.push(pending.command.clone());
            }
        }
        due
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

impl InputStaleDrop {
    pub fn accept(&mut self, snapshot: &WheelStateSnapshot) -> bool {
        if self.last_seq.is_some_and(|last| snapshot.seq <= last) {
            return false;
        }
        self.last_seq = Some(snapshot.seq);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_steer_core::{FfbCommandKind, FfbReplyKind};

    #[test]
    fn stale_input_is_dropped() {
        let mut filter = InputStaleDrop::default();
        assert!(filter.accept(&WheelStateSnapshot::empty(10, 0)));
        assert!(!filter.accept(&WheelStateSnapshot::empty(9, 0)));
        assert!(!filter.accept(&WheelStateSnapshot::empty(10, 0)));
        assert!(filter.accept(&WheelStateSnapshot::empty(11, 0)));
    }

    #[test]
    fn ffb_ack_clears_pending() {
        let mut tracker = FfbReliability::new(Duration::from_millis(10));
        tracker.track_sent(
            FfbCommand {
                command_id: 1,
                kind: FfbCommandKind::ResetState,
            },
            Instant::now(),
        );
        assert_eq!(tracker.pending_len(), 1);
        assert!(tracker.apply_reply(&FfbReply {
            command_id: 1,
            kind: FfbReplyKind::Ack,
        }));
        assert_eq!(tracker.pending_len(), 0);
    }
}
