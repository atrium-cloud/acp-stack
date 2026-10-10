//! Adapter stderr capture: each line is redacted, bounded, and logged under the
//! `agent_stderr` target at a capped rate, and the newest lines are kept as a tail
//! for initialize failures and `agent.exited`.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;

use super::*;
use crate::redaction::{bounded, redact_text};

// CONSTANTS

/// Log target of the adapter's stderr lines.
pub(super) const AGENT_STDERR_TARGET: &str = "agent_stderr";

/// Longest stderr line logged or kept, after redaction; the rest is replaced by a marker.
const AGENT_STDERR_LINE_MAX_BYTES: usize = 4096;

/// Bytes of a line read before the rest is discarded. Twice the logged cap, so redaction sees
/// more of a long line than is kept.
const AGENT_STDERR_LINE_READ_MAX_BYTES: usize = 2 * AGENT_STDERR_LINE_MAX_BYTES;

/// Lines logged per window; the rest are counted and reported once the window ends.
const AGENT_STDERR_LINES_PER_WINDOW: u32 = 200;
const AGENT_STDERR_WINDOW: Duration = Duration::from_secs(10);

/// Newest stderr bytes kept, as whole lines, for failure reasons and `agent.exited`.
const AGENT_STDERR_TAIL_BYTES: usize = 4096;

/// How long teardown waits for the reader to reach EOF before aborting it. Grandchildren that
/// inherited the pipe can hold it open past the agent's exit.
const AGENT_STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Ids stamped on every stderr line.
pub(super) struct StderrLabels {
    pub(super) agent_id: String,
    pub(super) target_id: Option<String>,
    pub(super) pid: Option<u32>,
}

/// The adapter's newest stderr lines, already redacted and bounded.
#[derive(Default)]
pub(super) struct StderrTail {
    lines: Mutex<TailLines>,
}

/// Kept lines plus their byte count, each line counted with its joining newline.
#[derive(Default)]
struct TailLines {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push(&self, line: String) {
        let mut tail = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        tail.bytes += line.len() + 1;
        tail.lines.push_back(line);
        while tail.bytes > AGENT_STDERR_TAIL_BYTES && tail.lines.len() > 1 {
            if let Some(evicted) = tail.lines.pop_front() {
                tail.bytes -= evicted.len() + 1;
            }
        }
    }

    /// The kept lines joined by newlines; empty when the adapter wrote nothing. Redacted again
    /// as a whole, since a secret value can span lines that were redacted one at a time.
    pub(super) fn snapshot(&self) -> String {
        let joined = {
            let tail = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
            tail.lines
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        };
        redact_text(&joined).into_owned()
    }
}

/// The stderr reader task and the tail it fills.
pub(super) struct StderrCapture {
    pub(super) tail: Arc<StderrTail>,
    task: TokioMutex<Option<JoinHandle<ReadSummary>>>,
}

impl StderrCapture {
    pub(super) fn start(stderr: ChildStderr, labels: StderrLabels) -> Self {
        let tail = Arc::new(StderrTail::default());
        let task = tokio::spawn(read_stderr(
            BufReader::new(stderr),
            labels,
            Arc::clone(&tail),
        ));
        Self {
            tail,
            task: TokioMutex::new(Some(task)),
        }
    }

    /// Give the reader a moment to reach EOF after the agent exited, then stop it.
    pub(super) async fn finish(&self) {
        let Some(mut task) = self.task.lock().await.take() else {
            return;
        };
        if timeout(AGENT_STDERR_DRAIN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }

    /// [`StderrTail::snapshot`] framed for appending to a failure reason; empty when the
    /// adapter wrote nothing.
    pub(super) fn reason_suffix(&self) -> String {
        let tail = self.tail.snapshot();
        if tail.is_empty() {
            String::new()
        } else {
            format!("; agent stderr:\n{tail}")
        }
    }
}

/// Lines a reader logged and suppressed over its life.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReadSummary {
    logged: u32,
    suppressed: u32,
}

async fn read_stderr(
    mut reader: impl AsyncBufRead + Unpin,
    labels: StderrLabels,
    tail: Arc<StderrTail>,
) -> ReadSummary {
    let mut window = RateWindow::new(Instant::now());
    let mut summary = ReadSummary::default();
    loop {
        let line = match read_capped_line(&mut reader, AGENT_STDERR_LINE_READ_MAX_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(
                    target: AGENT_STDERR_TARGET,
                    agent_id = %labels.agent_id,
                    target_id = labels.target_id.as_deref().unwrap_or_default(),
                    pid = labels.pid,
                    %error,
                    "reading agent stderr failed; later stderr output is not captured"
                );
                break;
            }
        };
        let text = String::from_utf8_lossy(&line.bytes);
        // A cut line can end in the head of a secret no rule recognizes on its own, and
        // redaction shrinkage can pull that head into the logged part, so it is dropped. A line
        // with no whitespace is one such head and keeps only the marker.
        let text = if line.discarded > 0 {
            text.trim_end_matches(|character: char| !character.is_whitespace())
        } else {
            &text
        };
        let mut redacted = bounded(&redact_text(text), AGENT_STDERR_LINE_MAX_BYTES).into_owned();
        if line.discarded > 0 {
            redacted.push_str(&format!(" [{} more bytes not read]", line.discarded));
        }
        match window.admit(Instant::now()) {
            Admission::Log { suppressed_before } => {
                log_suppressed(&labels, suppressed_before);
                log_line(&labels, &redacted);
                summary.logged += 1;
            }
            Admission::Suppress => summary.suppressed += 1,
        }
        tail.push(redacted);
    }
    log_suppressed(&labels, window.suppressed);
    summary
}

fn log_line(labels: &StderrLabels, line: &str) {
    tracing::warn!(
        target: AGENT_STDERR_TARGET,
        agent_id = %labels.agent_id,
        target_id = labels.target_id.as_deref().unwrap_or_default(),
        pid = labels.pid,
        "{line}"
    );
}

fn log_suppressed(labels: &StderrLabels, suppressed: u32) {
    if suppressed == 0 {
        return;
    }
    tracing::warn!(
        target: AGENT_STDERR_TARGET,
        agent_id = %labels.agent_id,
        target_id = labels.target_id.as_deref().unwrap_or_default(),
        pid = labels.pid,
        suppressed,
        window_secs = AGENT_STDERR_WINDOW.as_secs(),
        "agent stderr lines over the rate limit were not logged"
    );
}

/// One stderr line without its newline: the first `max_bytes` read, plus how many more bytes
/// the line carried and were discarded unread into memory.
struct CappedLine {
    bytes: Vec<u8>,
    discarded: usize,
}

/// Read up to the next `\n`, keeping at most `max_bytes`; `None` at EOF with nothing read.
async fn read_capped_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_bytes: usize,
) -> std::io::Result<Option<CappedLine>> {
    let mut line = CappedLine {
        bytes: Vec::new(),
        discarded: 0,
    };
    let mut read_any = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(read_any.then_some(line));
        }
        read_any = true;
        let (chunk, consumed, ended) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(newline) => (&buffer[..newline], newline + 1, true),
            None => (buffer, buffer.len(), false),
        };
        let room = max_bytes.saturating_sub(line.bytes.len());
        let kept = chunk.len().min(room);
        line.bytes.extend_from_slice(&chunk[..kept]);
        line.discarded += chunk.len() - kept;
        reader.consume(consumed);
        if ended {
            if line.discarded == 0 && line.bytes.last() == Some(&b'\r') {
                line.bytes.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Admission {
    /// Log the line; `suppressed_before` lines from the window that just ended went unlogged.
    Log {
        suppressed_before: u32,
    },
    Suppress,
}

/// Fixed-window line budget: [`AGENT_STDERR_LINES_PER_WINDOW`] per [`AGENT_STDERR_WINDOW`].
struct RateWindow {
    started: Instant,
    admitted: u32,
    suppressed: u32,
}

impl RateWindow {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            admitted: 0,
            suppressed: 0,
        }
    }

    fn admit(&mut self, now: Instant) -> Admission {
        let mut suppressed_before = 0;
        if now.duration_since(self.started) >= AGENT_STDERR_WINDOW {
            suppressed_before = std::mem::take(&mut self.suppressed);
            self.started = now;
            self.admitted = 0;
        }
        if self.admitted < AGENT_STDERR_LINES_PER_WINDOW {
            self.admitted += 1;
            Admission::Log { suppressed_before }
        } else {
            self.suppressed += 1;
            Admission::Suppress
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(input: &[u8], max_bytes: usize) -> Vec<(String, usize)> {
        let mut reader = BufReader::with_capacity(16, input);
        let mut lines = Vec::new();
        while let Some(line) = read_capped_line(&mut reader, max_bytes)
            .await
            .expect("read")
        {
            lines.push((
                String::from_utf8_lossy(&line.bytes).into_owned(),
                line.discarded,
            ));
        }
        lines
    }

    #[tokio::test]
    async fn capped_reader_splits_lines_and_discards_past_the_cap() {
        let input = b"first\r\nsecond line that is long\nlast-no-newline";
        assert_eq!(
            read_all(input, 10).await,
            vec![
                ("first".to_owned(), 0),
                ("second lin".to_owned(), 14),
                ("last-no-ne".to_owned(), 5),
            ]
        );
    }

    #[tokio::test]
    async fn capped_reader_reports_eof_on_empty_input() {
        assert!(read_all(b"", 10).await.is_empty());
        assert_eq!(read_all(b"\n", 10).await, vec![(String::new(), 0)]);
    }

    fn test_labels() -> StderrLabels {
        StderrLabels {
            agent_id: "placebo".to_owned(),
            target_id: None,
            pid: None,
        }
    }

    #[tokio::test]
    async fn reader_logs_up_to_the_budget_and_keeps_every_line_in_the_tail() {
        let input: String = (0..205).map(|index| format!("line {index}\n")).collect();
        let tail = Arc::new(StderrTail::default());
        let summary = read_stderr(
            BufReader::new(input.as_bytes()),
            test_labels(),
            Arc::clone(&tail),
        )
        .await;
        assert_eq!(
            summary,
            ReadSummary {
                logged: AGENT_STDERR_LINES_PER_WINDOW,
                suppressed: 5
            }
        );
        assert!(tail.snapshot().ends_with("line 203\nline 204"));
    }

    #[tokio::test]
    async fn reader_drops_the_partial_token_at_a_read_cut() {
        // The JWT-shaped token redacts to a placeholder, shrinking the line by ~8 KiB, so
        // without the drop the head cut at the read limit would land inside the logged part.
        let jwt = format!(
            "{}.{}.{}",
            "a".repeat(4000),
            "b".repeat(3970),
            "c".repeat(10)
        );
        let mut input = format!(
            "{jwt}{}",
            " ".repeat(AGENT_STDERR_LINE_READ_MAX_BYTES - 6 - jwt.len())
        );
        input.push_str(" KeyHead-continues-past-the-cut\n");
        let tail = Arc::new(StderrTail::default());
        read_stderr(
            BufReader::new(input.as_bytes()),
            test_labels(),
            Arc::clone(&tail),
        )
        .await;
        let snapshot = tail.snapshot();
        assert!(!snapshot.contains("KeyHe"), "{snapshot}");
        assert!(snapshot.ends_with("more bytes not read]"), "{snapshot}");
    }

    #[test]
    fn tail_snapshot_redacts_a_secret_spanning_lines() {
        crate::redaction::register_secret_values(["MultiLineKey-7f2\nMultiLineKey-8a4"]);
        let tail = StderrTail::default();
        tail.push("MultiLineKey-7f2".to_owned());
        tail.push("MultiLineKey-8a4".to_owned());
        assert_eq!(tail.snapshot(), "[redacted]");
    }

    #[test]
    fn rate_window_suppresses_past_the_budget_and_reports_at_rollover() {
        let start = Instant::now();
        let mut window = RateWindow::new(start);
        for _ in 0..AGENT_STDERR_LINES_PER_WINDOW {
            assert_eq!(
                window.admit(start),
                Admission::Log {
                    suppressed_before: 0
                }
            );
        }
        assert_eq!(window.admit(start), Admission::Suppress);
        assert_eq!(window.admit(start), Admission::Suppress);
        assert_eq!(
            window.admit(start + AGENT_STDERR_WINDOW),
            Admission::Log {
                suppressed_before: 2
            }
        );
        assert_eq!(window.suppressed, 0);
    }

    #[test]
    fn tail_keeps_the_newest_whole_lines_within_its_budget() {
        let tail = StderrTail::default();
        let line = "x".repeat(1500);
        for index in 0..4 {
            tail.push(format!("{index}{line}"));
        }
        let snapshot = tail.snapshot();
        assert!(
            snapshot.len() <= AGENT_STDERR_TAIL_BYTES,
            "{}",
            snapshot.len()
        );
        assert!(snapshot.starts_with('2'), "oldest lines evicted whole");
        assert_eq!(snapshot.lines().count(), 2);
    }

    #[test]
    fn tail_keeps_the_newest_line_even_past_its_budget() {
        let tail = StderrTail::default();
        tail.push("y".repeat(AGENT_STDERR_LINE_MAX_BYTES + 30));
        assert_eq!(tail.snapshot().len(), AGENT_STDERR_LINE_MAX_BYTES + 30);
    }
}
