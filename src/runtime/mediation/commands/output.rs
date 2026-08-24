//! Output streaming primitives for the command gateway: chunk queue element,
//! cumulative byte counter, and the UTF-8 boundary helpers the readers use.

use std::time::Duration;

/// Per-read upper bound on bytes held in memory before a chunk is emitted;
/// `[commands].max_output_bytes` caps the cumulative total separately.
pub(crate) const BOUNDED_READ_CHUNK_BYTES: usize = 4 * 1024;

/// Hard cap on the post-`child.wait()` drain: a child that detaches a grandchild
/// via `setsid`/`nohup` keeps the pipes open past our process-group kill, and the
/// drain would otherwise wait for EOF forever, wedging the row in `running`.
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
    /// `None` distinguishes a kernel-signal exit from a normal status code.
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

/// Longest prefix of `buf` ending on a complete UTF-8 codepoint boundary, as
/// `(decoded_end, residue_start)`; trailing partial sequences defer to the next read.
pub(crate) fn utf8_split_boundary(buf: &[u8]) -> (usize, usize) {
    let len = buf.len();
    for offset in 1..=3 {
        if offset > len {
            break;
        }
        let i = len - offset;
        let byte = buf[i];
        // Continuation byte.
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        // 4-byte sequence leader.
        if byte & 0b1111_1000 == 0b1111_0000 && offset < 4 {
            return (i, i);
        }
        // 3-byte sequence leader.
        if byte & 0b1111_0000 == 0b1110_0000 && offset < 3 {
            return (i, i);
        }
        // 2-byte sequence leader.
        if byte & 0b1110_0000 == 0b1100_0000 && offset < 2 {
            return (i, i);
        }
        return (len, len);
    }
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
/// sequences across reads so a split glyph is decoded once, not twice lossily.
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
    let mut carryover: Vec<u8> = Vec::with_capacity(4);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                if !carryover.is_empty() {
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
                // Bound the carryover at 3 bytes: a stream of stray continuation
                // bytes would otherwise grow it unbounded as the helper keeps deferring.
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
                    // Receiver gone: the normal teardown path on cancel/timeout.
                    return;
                }
            }
            Err(error) => {
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
        assert_eq!(floor_char_boundary(input, 2), 1);
        assert_eq!(floor_char_boundary(input, 0), 0);
        assert_eq!(floor_char_boundary(input, 999), input.len());
    }

    #[test]
    fn utf8_split_boundary_defers_partial_codepoints() {
        // First byte of 'é' alone must be deferred.
        let buf = b"a\xC3";
        assert_eq!(utf8_split_boundary(buf), (1, 1));
        let buf = b"a\xC3\xA9";
        assert_eq!(utf8_split_boundary(buf), (3, 3));
        let buf = b"hello";
        assert_eq!(utf8_split_boundary(buf), (5, 5));
        // Two leading bytes of a 4-byte sequence.
        let buf = b"\xF0\x9F";
        assert_eq!(utf8_split_boundary(buf), (0, 0));
    }
}
