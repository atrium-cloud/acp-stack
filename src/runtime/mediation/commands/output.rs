//! Output streaming primitives for the command gateway.
//!
//! `OutputChunk` is the queue element passed from the per-pipe reader tasks to
//! the supervisor; `OutputCounter` enforces `[commands].max_output_bytes` on
//! the cumulative total. Byte-cap constants and the UTF-8 boundary helpers
//! that the reader uses to avoid mid-codepoint splits live here too.

use std::time::Duration;

/// Per-read upper bound on bytes the reader will hold in memory before
/// emitting a chunk. A command that emits a giant line without a newline
/// (`yes`, `dd if=/dev/zero …`) still produces chunks of at most this many
/// bytes — `[commands].max_output_bytes` then caps the cumulative total.
pub(crate) const BOUNDED_READ_CHUNK_BYTES: usize = 4 * 1024;

/// Hard cap on the post-`child.wait()` drain. Without this, a child that
/// detaches a grandchild via `setsid`/`nohup` keeps the stdout/stderr pipes
/// open after our process-group kill (the grandchild lives in a different
/// pgid), and the drain loop would wait for EOF forever, wedging the
/// command row in `running`. 5s is generous for legitimate finalization
/// (descendant ACKs, last buffered output) and short enough that an escaped
/// descendant doesn't keep a supervisor task alive indefinitely.
pub(crate) const POST_WAIT_DRAIN_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct OutputChunk {
    pub(crate) stream: String,
    pub(crate) data: String,
}

#[derive(Debug)]
pub(super) struct OutputCounter {
    pub(super) used: usize,
    pub(super) max: usize,
    pub(super) exhausted: bool,
    pub(super) seq: u64,
}

impl OutputCounter {
    pub(super) fn new(max: usize) -> Self {
        Self {
            used: 0,
            max,
            exhausted: false,
            seq: 0,
        }
    }

    pub(super) fn remaining(&self) -> usize {
        self.max.saturating_sub(self.used)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Outcome {
    /// `Option<i32>` so we can distinguish a kernel-signal exit (None) from a
    /// normal status code (Some).
    Exited(Option<i32>),
    Canceled,
    TimedOut,
    SpawnError,
    PersistenceError,
}

pub(super) trait OptionFlatten {
    fn flatten_to_i32(self) -> Option<i32>;
}

impl OptionFlatten for Option<i32> {
    fn flatten_to_i32(self) -> Option<i32> {
        self
    }
}

/// Find the longest prefix of `buf` that ends on a complete UTF-8 codepoint
/// boundary, and return `(decoded_end, residue_start)`. Trailing bytes that
/// could still form a valid codepoint (1-3 leading bytes of a multi-byte
/// sequence) are deferred into `residue_start..` for the next read to
/// complete. Invalid bytes inside an otherwise-complete prefix are kept and
/// decoded lossy by the caller.
pub(crate) fn utf8_split_boundary(buf: &[u8]) -> (usize, usize) {
    // Look back up to 3 bytes for an incomplete UTF-8 leading sequence.
    let len = buf.len();
    for offset in 1..=3 {
        if offset > len {
            break;
        }
        let i = len - offset;
        let byte = buf[i];
        // Continuation byte: keep scanning back.
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        // 4-byte sequence leader
        if byte & 0b1111_1000 == 0b1111_0000 && offset < 4 {
            return (i, i);
        }
        // 3-byte sequence leader
        if byte & 0b1111_0000 == 0b1110_0000 && offset < 3 {
            return (i, i);
        }
        // 2-byte sequence leader
        if byte & 0b1110_0000 == 0b1100_0000 && offset < 2 {
            return (i, i);
        }
        // Single-byte ASCII or fully-complete multi-byte sequence: split
        // right after this byte.
        return (len, len);
    }
    // All bytes were continuations (or buffer < 1) — defer everything.
    (0, 0)
}

pub(crate) fn floor_char_boundary(input: &str, max: usize) -> usize {
    if max >= input.len() {
        return input.len();
    }
    let mut cutoff = max;
    while cutoff > 0 && !input.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    cutoff
}

/// Pump one child pipe into bounded `OutputChunk`s, carrying partial UTF-8
/// sequences across reads so a multi-byte glyph split across the read
/// boundary is decoded once, not twice with replacement chars on either side.
pub(crate) async fn read_stream<R>(
    reader: R,
    stream: &'static str,
    tx: tokio::sync::mpsc::Sender<OutputChunk>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut reader = reader;
    let mut buffer = vec![0u8; BOUNDED_READ_CHUNK_BYTES];
    // Bounded to 3 bytes max — the maximum residue from any valid UTF-8
    // prefix.
    let mut carryover: Vec<u8> = Vec::with_capacity(4);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                if !carryover.is_empty() {
                    // Flush any leftover bytes that never completed a UTF-8
                    // sequence (a child that printed garbage and exited).
                    let chunk = String::from_utf8_lossy(&carryover).into_owned();
                    let _ = tx
                        .send(OutputChunk {
                            stream: stream.to_owned(),
                            data: chunk,
                        })
                        .await;
                }
                return;
            }
            Ok(read) => {
                let mut combined = std::mem::take(&mut carryover);
                combined.extend_from_slice(&buffer[..read]);
                let (decoded_end, residue_start) = utf8_split_boundary(&combined);
                // Bound the carryover at 3 bytes. A child emitting an endless
                // stream of stray continuation bytes (e.g. raw binary garbage
                // starting with 0x80…) would otherwise grow this buffer
                // unbounded because the boundary helper keeps deferring.
                // Flush anything longer than that as lossy text and reset.
                let (decoded_end, residue_start) = if combined.len() - residue_start > 3 {
                    (combined.len(), combined.len())
                } else {
                    (decoded_end, residue_start)
                };
                carryover = combined[residue_start..].to_vec();
                let chunk = String::from_utf8_lossy(&combined[..decoded_end]).into_owned();
                if !chunk.is_empty()
                    && tx
                        .send(OutputChunk {
                            stream: stream.to_owned(),
                            data: chunk,
                        })
                        .await
                        .is_err()
                {
                    // Receiver gone: the owner moved on. Exit quietly; this is
                    // the normal teardown path on cancel/timeout.
                    return;
                }
            }
            Err(error) => {
                // Surface the read failure so a broken pipe or kernel error
                // is visible in the durable trail, not silently dropped.
                tracing::warn!(error = %error, stream = stream, "command output reader hit IO error");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_respects_utf8() {
        let input = "héllo";
        // 'é' is two bytes (0xC3, 0xA9). Cap at 2 should land at byte 1 (after 'h').
        assert_eq!(floor_char_boundary(input, 2), 1);
        assert_eq!(floor_char_boundary(input, 0), 0);
        assert_eq!(floor_char_boundary(input, 999), input.len());
    }

    #[test]
    fn utf8_split_boundary_defers_partial_codepoints() {
        // 'é' = [0xC3, 0xA9]. First byte alone must be deferred.
        let buf = b"a\xC3";
        assert_eq!(utf8_split_boundary(buf), (1, 1));
        // Complete 'é' should be fully consumed.
        let buf = b"a\xC3\xA9";
        assert_eq!(utf8_split_boundary(buf), (3, 3));
        // Plain ASCII is split right at the end.
        let buf = b"hello";
        assert_eq!(utf8_split_boundary(buf), (5, 5));
        // Two leading bytes of a 4-byte sequence must be deferred.
        let buf = b"\xF0\x9F"; // start of '🚀'
        assert_eq!(utf8_split_boundary(buf), (0, 0));
    }
}
