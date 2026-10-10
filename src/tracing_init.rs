use std::io::{self, Write};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// CONSTANTS

const LOG_LEVEL: LevelFilter = LevelFilter::WARN;

pub fn init() {
    let result = log_subscriber(io::stderr).try_init();
    if let Err(error) = result {
        eprintln!("failed to initialize tracing subscriber: {error}");
    }
}

// ANSI stays off: colour codes split field names (`\x1b[3mapi_key\x1b[0m`), which would hide
// them from the redactor's sensitive-name match.
fn log_subscriber<W>(writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(RedactingMakeWriter { inner: writer }),
        )
        .with(LOG_LEVEL)
}

/// Redacts each formatted event before it reaches the inner writer, so no log line can carry a
/// registered secret value or a credential-shaped token.
struct RedactingMakeWriter<M> {
    inner: M,
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
        }
    }
}

/// Valid only behind the fmt layer, which hands over each event as one complete buffer in a
/// single `write_all`, so redacting per call sees whole lines. A caller that split an event
/// across writes could split a secret past the redactor, hence the type stays private.
struct RedactingWriter<W> {
    inner: W,
}

impl<W: Write> Write for RedactingWriter<W> {
    // Reporting `buf.len()` keeps `write_all` from re-sending a redacted remainder.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.inner
            .write_all(crate::redaction::redact_text(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_events(emit: impl FnOnce()) -> String {
        let captured = Captured::default();
        let sink = captured.clone();
        tracing::subscriber::with_default(log_subscriber(move || sink.clone()), emit);
        let bytes = captured
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes).expect("utf8 log output")
    }

    #[test]
    fn redacting_writer_scrubs_registered_values_and_credential_shapes() {
        crate::redaction::register_secret_values(["TracingSecret-4mQ9z"]);
        let output = capture_events(|| {
            tracing::warn!(
                reason = "upstream echoed TracingSecret-4mQ9z",
                api_key = "plainFieldValue42",
                "request failed with key sk-AbCdEf123456"
            );
        });
        assert!(!output.contains("TracingSecret-4mQ9z"), "{output}");
        assert!(!output.contains("sk-AbCdEf123456"), "{output}");
        assert!(!output.contains("plainFieldValue42"), "{output}");
        assert_eq!(output.matches("[redacted]").count(), 3, "{output}");
    }

    #[test]
    fn info_lines_are_filtered_out() {
        let output = capture_events(|| {
            tracing::info!("info line");
            tracing::warn!("warn line");
        });
        assert!(!output.contains("info line"), "{output}");
        assert!(output.contains("warn line"), "{output}");
    }
}
