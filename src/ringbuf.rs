use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;

/// Result of querying the ring buffer from a client's `last_seq`.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplaySlice {
    /// Client already has the latest byte sequence.
    UpToDate { end_seq: u64 },
    /// Client missed bytes that are still inside the ring buffer (exact delta).
    Delta { end_seq: u64, data: Bytes },
    /// Client is fresh (`last_seq == 0`) or fell behind the evicted window; send full buffer.
    FullSnapshot { end_seq: u64, data: Bytes },
}

/// Chunked monotonic-sequence ring buffer for PTY output caching & seamless reconnect/resync.
#[derive(Debug)]
pub struct SeqRingBuffer {
    capacity: usize,
    chunks: VecDeque<Bytes>,
    total_bytes: usize,
    start_seq: u64,
    end_seq: u64,
}

impl SeqRingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(4096),
            chunks: VecDeque::new(),
            total_bytes: 0,
            start_seq: 0,
            end_seq: 0,
        }
    }

    pub fn end_seq(&self) -> u64 {
        self.end_seq
    }

    #[allow(dead_code)]
    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.total_bytes
    }

    /// Push a new chunk of PTY output bytes, returning the new `end_seq`.
    pub fn push(&mut self, chunk: Bytes) -> u64 {
        if chunk.is_empty() {
            return self.end_seq;
        }
        let len = chunk.len();
        self.end_seq = self.end_seq.saturating_add(len as u64);

        if len >= self.capacity {
            // Single chunk larger than capacity: keep only the tail `capacity` bytes.
            let tail = chunk.slice(len - self.capacity..);
            self.chunks.clear();
            self.total_bytes = tail.len();
            self.start_seq = self.end_seq - tail.len() as u64;
            self.chunks.push_back(tail);
            return self.end_seq;
        }

        self.chunks.push_back(chunk);
        self.total_bytes += len;

        while self.total_bytes > self.capacity {
            if let Some(front) = self.chunks.pop_front() {
                self.total_bytes -= front.len();
                self.start_seq = self.start_seq.saturating_add(front.len() as u64);
            } else {
                break;
            }
        }

        self.end_seq
    }

    /// Extract either exact delta bytes from `last_seq` or a full snapshot of the buffer.
    pub fn slice_from(&self, last_seq: u64) -> ReplaySlice {
        if last_seq >= self.end_seq {
            return ReplaySlice::UpToDate {
                end_seq: self.end_seq,
            };
        }

        if last_seq > 0 && last_seq >= self.start_seq {
            let skip = (last_seq - self.start_seq) as usize;
            let data = self.collect_skipping(skip);
            return ReplaySlice::Delta {
                end_seq: self.end_seq,
                data,
            };
        }

        let data = self.collect_skipping(0);
        ReplaySlice::FullSnapshot {
            end_seq: self.end_seq,
            data,
        }
    }

    fn collect_skipping(&self, mut skip: usize) -> Bytes {
        let out_len = self.total_bytes.saturating_sub(skip);
        let mut buf = BytesMut::with_capacity(out_len);
        for chunk in &self.chunks {
            if skip >= chunk.len() {
                skip -= chunk.len();
                continue;
            }
            buf.extend_from_slice(&chunk[skip..]);
            skip = 0;
        }
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_and_eviction_work_cleanly() {
        let mut rb = SeqRingBuffer::new(4096);
        let s1 = rb.push(Bytes::from_static(b"hello "));
        let s2 = rb.push(Bytes::from_static(b"world!"));
        assert_eq!(s1, 6);
        assert_eq!(s2, 12);

        // Fresh client gets full snapshot
        assert_eq!(
            rb.slice_from(0),
            ReplaySlice::FullSnapshot {
                end_seq: 12,
                data: Bytes::from_static(b"hello world!"),
            }
        );

        // Client resuming at seq=6 gets exact delta
        assert_eq!(
            rb.slice_from(6),
            ReplaySlice::Delta {
                end_seq: 12,
                data: Bytes::from_static(b"world!"),
            }
        );

        // Up-to-date client gets nothing
        assert_eq!(rb.slice_from(12), ReplaySlice::UpToDate { end_seq: 12 });

        // Overflow ring buffer
        let big = Bytes::from(vec![b'x'; 5000]);
        let s3 = rb.push(big);
        assert_eq!(s3, 5012);
        assert_eq!(rb.len(), 4096);
        assert_eq!(rb.start_seq(), 916);

        // Client at old seq=12 falls back to FullSnapshot
        match rb.slice_from(12) {
            ReplaySlice::FullSnapshot { end_seq, data } => {
                assert_eq!(end_seq, 5012);
                assert_eq!(data.len(), 4096);
            }
            other => panic!("expected FullSnapshot, got {other:?}"),
        }
    }
}
