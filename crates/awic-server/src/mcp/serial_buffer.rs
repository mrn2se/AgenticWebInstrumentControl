//! Per-session bounded byte buffer backing the `serial_read`/`serial_expect`
//! MCP tools (SPECIFICATION.md §8.3, §11.2.1).
//!
//! Terminal output normally flows to a human over `0xA7` WebSocket frames,
//! replayed from a session's own 64 KiB scrollback on join
//! (`crate::terminal::SCROLLBACK_CAPACITY`). An agent instead polls
//! `serial_read`, so this is a *second*, smaller ring buffer per MCP
//! session: bytes are pushed here by `mcp::host::spawn_drain` as they arrive
//! and consumed by `serial_read`. Bounded independently of the terminal
//! module's own scrollback so an MCP client that never reads still can't
//! grow this without bound — the same "bounded" contract §8.3 puts on every
//! tool result applies just as well to what a tool buffers between calls.

use std::collections::VecDeque;

/// Smaller than the terminal module's 64 KiB human scrollback: this buffer
/// exists to be drained promptly by an agent polling `serial_read`, not to
/// hold a long replay history.
pub const SERIAL_BUFFER_CAPACITY: usize = 16 * 1024;

#[derive(Default)]
pub struct SerialBuffer {
    buf: VecDeque<u8>,
    /// Bytes evicted from the front because the buffer filled up before the
    /// last `serial_read` — reported on the next read rather than silently
    /// lost (SPEC §8.2: "says when data was dropped").
    dropped: u64,
}

impl SerialBuffer {
    pub fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.buf.len() >= SERIAL_BUFFER_CAPACITY {
                self.buf.pop_front();
                self.dropped += 1;
            }
            self.buf.push_back(b);
        }
    }

    /// Drains up to `max_bytes` from the front, returning the bytes, how
    /// many were dropped since the *previous* drain, and whether more bytes
    /// remain buffered beyond `max_bytes`.
    pub fn drain(&mut self, max_bytes: usize) -> (Vec<u8>, u64, bool) {
        let take = max_bytes.min(self.buf.len());
        let data: Vec<u8> = self.buf.drain(..take).collect();
        let dropped = std::mem::take(&mut self.dropped);
        let more_available = !self.buf.is_empty();
        (data, dropped, more_available)
    }

    /// The whole buffer, without consuming it — `serial_expect` scans this
    /// repeatedly so a later `serial_read` still sees the same bytes.
    pub fn peek_all(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_bytes_in_order_and_leaves_the_rest() {
        let mut buf = SerialBuffer::default();
        buf.push(b"hello world");
        let (data, dropped, more) = buf.drain(5);
        assert_eq!(data, b"hello");
        assert_eq!(dropped, 0);
        assert!(more);
        let (rest, _, more) = buf.drain(100);
        assert_eq!(rest, b" world");
        assert!(!more);
    }

    #[test]
    fn overflow_evicts_oldest_and_counts_drops() {
        let mut buf = SerialBuffer::default();
        buf.push(&vec![0u8; SERIAL_BUFFER_CAPACITY + 10]);
        let (data, dropped, more) = buf.drain(usize::MAX);
        assert_eq!(data.len(), SERIAL_BUFFER_CAPACITY);
        assert_eq!(dropped, 10);
        assert!(!more);
    }

    #[test]
    fn dropped_count_resets_after_being_reported() {
        let mut buf = SerialBuffer::default();
        buf.push(&vec![0u8; SERIAL_BUFFER_CAPACITY + 5]);
        let (_, dropped, _) = buf.drain(1);
        assert_eq!(dropped, 5);
        let (_, dropped, _) = buf.drain(1);
        assert_eq!(
            dropped, 0,
            "a drop reported once must not be reported again"
        );
    }

    #[test]
    fn peek_does_not_consume() {
        let mut buf = SerialBuffer::default();
        buf.push(b"abc");
        assert_eq!(buf.peek_all(), b"abc");
        assert_eq!(buf.peek_all(), b"abc", "peek must not drain");
    }
}
