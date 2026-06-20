//! Recording a camera's MJPEG stream to disk for the "plain" timelapse. For a
//! camera that exposes a real `/stream`, this captures the actual continuous video
//! instead of time-sampling `/snapshot`. The copy + reconnect logic is pure (the
//! stream opener and the sink are injected), so it's unit-tested without a network:
//! tests feed canned readers and a `Vec` sink, and drive the cancel signal.

use std::io::{Read, Write};

use super::camera::StreamOpen;

/// How a single-connection copy ended.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyEnd {
    /// Upstream closed the body (EOF) — the caller reconnects if still recording.
    Eof,
    /// The recorder was asked to stop.
    Cancelled,
    /// The per-run byte cap was reached.
    CapReached,
    /// A read (upstream) or write (sink) error ended this connection — reconnect,
    /// counting it as a failure. The bytes written *before* the error are still
    /// reported so the caller's byte cap and totals stay correct.
    Errored,
}

/// Aggregate outcome of a full recording run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecordStats {
    /// Total bytes written to the sink across all connections.
    pub bytes: u64,
    /// Successful stream opens.
    pub connections: u32,
    /// Failed attempts (an open error, or a mid-stream read error).
    pub failures: u32,
}

/// Copy from `reader` to `sink` until EOF, cancellation, the byte cap, or an
/// error. `cancel` is checked between reads — a read already in flight finishes
/// first, so this is cooperative (paired with a short read timeout on the real
/// stream). Always returns the bytes written (even on error), so the caller's cap
/// and totals stay correct across reconnects. Never writes past `cap_remaining`.
pub fn copy_until(
    reader: &mut dyn Read,
    sink: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
    cap_remaining: u64,
    buf: &mut [u8],
) -> (CopyEnd, u64) {
    let mut written: u64 = 0;
    loop {
        if cancel() {
            return (CopyEnd::Cancelled, written);
        }
        if written >= cap_remaining {
            return (CopyEnd::CapReached, written);
        }
        let n = match reader.read(buf) {
            Ok(0) => return (CopyEnd::Eof, written),
            Ok(n) => n,
            Err(_) => return (CopyEnd::Errored, written),
        };
        // Don't overshoot the cap — write only what fits, then stop.
        let take = (n as u64).min(cap_remaining - written) as usize;
        if sink.write_all(&buf[..take]).is_err() {
            return (CopyEnd::Errored, written);
        }
        written += take as u64;
        if take < n {
            return (CopyEnd::CapReached, written);
        }
    }
}

/// Record `open`'s stream into `sink`: open → copy → on EOF/error reconnect (after
/// `backoff`) until `cancel` fires or `max_bytes` is reached. `backoff(attempt)` is
/// the inter-attempt pause (the real one sleeps; tests pass a no-op). The loop only
/// ends on cancel/cap — a persistently-failing source keeps retrying because the
/// recorder is meant to run for as long as the print is active, and the caller
/// flips `cancel` when the print ends.
pub fn record_loop(
    open: &StreamOpen,
    sink: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
    max_bytes: u64,
    buf_size: usize,
    backoff: &dyn Fn(u32),
) -> RecordStats {
    let mut stats = RecordStats::default();
    let mut buf = vec![0u8; buf_size.max(1)];
    let mut attempts: u32 = 0;
    loop {
        if cancel() || stats.bytes >= max_bytes {
            return stats;
        }
        if attempts > 0 {
            backoff(attempts);
            if cancel() {
                return stats; // asked to stop during the backoff
            }
        }
        attempts += 1;
        match (open)() {
            Ok(mut opened) => {
                stats.connections += 1;
                let (end, n) = copy_until(
                    &mut *opened.reader,
                    sink,
                    cancel,
                    max_bytes - stats.bytes,
                    &mut buf,
                );
                stats.bytes += n; // always count what was written, even on error
                match end {
                    CopyEnd::Eof => {}                       // reconnect
                    CopyEnd::Errored => stats.failures += 1, // count + reconnect
                    CopyEnd::Cancelled | CopyEnd::CapReached => return stats,
                }
            }
            Err(_) => stats.failures += 1, // open error → backoff + retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::camera::OpenedCameraStream;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn never() -> bool {
        false
    }

    // ── copy_until ──

    #[test]
    fn copy_until_writes_everything_then_reports_eof() {
        let mut r = Cursor::new(b"hello world".to_vec());
        let mut sink: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4];
        let (end, n) = copy_until(&mut r, &mut sink, &never, 1_000, &mut buf);
        assert_eq!(end, CopyEnd::Eof);
        assert_eq!(n, 11);
        assert_eq!(sink, b"hello world");
    }

    #[test]
    fn copy_until_stops_on_cancel_without_reading() {
        let mut r = Cursor::new(b"data".to_vec());
        let mut sink: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8];
        let (end, n) = copy_until(&mut r, &mut sink, &|| true, 1_000, &mut buf);
        assert_eq!(end, CopyEnd::Cancelled);
        assert_eq!(n, 0);
        assert!(sink.is_empty());
    }

    /// Yields `data`, then fails every subsequent read (a mid-stream drop).
    struct BytesThenErr {
        data: Vec<u8>,
        pos: usize,
    }
    impl Read for BytesThenErr {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "drop",
                ));
            }
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn copy_until_keeps_bytes_written_before_a_read_error() {
        // The bug the cap relies on NOT having: a mid-stream error must still report
        // the bytes already written, or a flaky stream blows past the cap.
        let mut r = BytesThenErr {
            data: b"abc".to_vec(),
            pos: 0,
        };
        let mut sink: Vec<u8> = Vec::new();
        let mut buf = [0u8; 2];
        let (end, n) = copy_until(&mut r, &mut sink, &never, 1_000, &mut buf);
        assert_eq!(end, CopyEnd::Errored);
        assert_eq!(n, 3);
        assert_eq!(sink, b"abc");
    }

    #[test]
    fn copy_until_never_writes_past_the_cap() {
        let mut r = Cursor::new(b"0123456789".to_vec());
        let mut sink: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8];
        let (end, n) = copy_until(&mut r, &mut sink, &never, 4, &mut buf);
        assert_eq!(end, CopyEnd::CapReached);
        assert_eq!(n, 4);
        assert_eq!(sink, b"0123");
    }

    // ── record_loop ──

    /// A `StreamOpen` that yields each entry in turn: `Some(bytes)` opens a reader
    /// over those bytes (then EOF), `None` is an open failure. Out of entries ⇒
    /// open failure. Also exposes the attempt counter for the cancel closure.
    fn seq_opener(entries: Vec<Option<Vec<u8>>>) -> (StreamOpen, Arc<AtomicUsize>) {
        let idx = Arc::new(AtomicUsize::new(0));
        let counter = idx.clone();
        let open: StreamOpen = Arc::new(move || {
            let i = idx.fetch_add(1, Ordering::SeqCst);
            match entries.get(i) {
                Some(Some(data)) => Ok(OpenedCameraStream {
                    content_type: "multipart/x-mixed-replace".to_string(),
                    reader: Box::new(Cursor::new(data.clone())),
                }),
                _ => Err("no stream".to_string()),
            }
        });
        (open, counter)
    }

    #[test]
    fn record_loop_reconnects_after_each_eof() {
        // Two readers, EOF each; a byte cap spanning both forces a reconnect
        // between them — connections == 2 and the concatenated output prove the
        // recorder resumed onto a fresh connection.
        let (open, _) = seq_opener(vec![Some(b"aaa".to_vec()), Some(b"bbb".to_vec())]);
        let mut sink: Vec<u8> = Vec::new();
        let stats = record_loop(&open, &mut sink, &never, 6, 8, &|_| {});
        assert_eq!(stats.connections, 2);
        assert_eq!(stats.failures, 0);
        assert_eq!(sink, b"aaabbb");
    }

    #[test]
    fn record_loop_counts_a_failed_open_and_retries() {
        // First open fails (failures == 1), the retry succeeds and is recorded.
        let (open, _) = seq_opener(vec![None, Some(b"ok".to_vec())]);
        let mut sink: Vec<u8> = Vec::new();
        let stats = record_loop(&open, &mut sink, &never, 2, 8, &|_| {});
        assert_eq!(stats.failures, 1);
        assert_eq!(stats.connections, 1);
        assert_eq!(sink, b"ok");
    }

    #[test]
    fn record_loop_stops_immediately_when_cancelled_up_front() {
        let (open, _idx) = seq_opener(vec![Some(b"x".to_vec())]);
        let mut sink: Vec<u8> = Vec::new();
        let stats = record_loop(&open, &mut sink, &|| true, 1_000, 8, &|_| {});
        assert_eq!(stats.connections, 0);
        assert_eq!(stats.bytes, 0);
        assert!(sink.is_empty());
    }

    #[test]
    fn record_loop_honours_the_byte_cap() {
        // One long reader, cap below its length: copy stops at the cap and the run
        // ends (CapReached), no reconnect.
        let (open, _idx) = seq_opener(vec![Some(vec![7u8; 100])]);
        let mut sink: Vec<u8> = Vec::new();
        let stats = record_loop(&open, &mut sink, &never, 10, 8, &|_| {});
        assert_eq!(stats.bytes, 10);
        assert_eq!(stats.connections, 1);
        assert_eq!(sink.len(), 10);
    }
}
