#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplState {
    Handshake,
    FullSync,
    Online,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplOffset(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplProgress {
    pub state: ReplState,
    pub primary_offset: ReplOffset,
    pub replica_ack_offset: ReplOffset,
}

impl Default for ReplProgress {
    fn default() -> Self {
        Self {
            state: ReplState::Handshake,
            primary_offset: ReplOffset(0),
            replica_ack_offset: ReplOffset(0),
        }
    }
}

impl ReplProgress {
    pub fn on_full_sync_start(&mut self) {
        self.state = ReplState::FullSync;
    }

    pub fn on_online(&mut self) {
        self.state = ReplState::Online;
    }

    pub fn append_primary_bytes(&mut self, bytes: u64) {
        self.primary_offset.0 = self.primary_offset.0.saturating_add(bytes);
    }

    pub fn ack_replica_offset(&mut self, offset: ReplOffset) {
        if offset.0 > self.replica_ack_offset.0 {
            self.replica_ack_offset = offset;
        }
    }

    #[must_use]
    pub fn lag_bytes(&self) -> u64 {
        self.primary_offset
            .0
            .saturating_sub(self.replica_ack_offset.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{ReplOffset, ReplProgress, ReplState};

    #[test]
    fn lag_is_monotonic_with_unacked_primary_writes() {
        let mut repl = ReplProgress::default();
        repl.on_online();
        assert_eq!(repl.state, ReplState::Online);
        repl.append_primary_bytes(128);
        assert_eq!(repl.lag_bytes(), 128);
        repl.ack_replica_offset(ReplOffset(64));
        assert_eq!(repl.lag_bytes(), 64);
    }
}
