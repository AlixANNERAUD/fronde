//! A component's standard error, line by line, as warnings in the hub's log under its name.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// Longer lines are cut.
const MAX_LINE: usize = 1024;
/// Per call: a component that writes more is cut short, so that it cannot flood the log.
const MAX_LINES: usize = 100;

/// The standard error of one call to the component `name`.
pub(crate) struct GuestLog(pub(crate) Arc<str>);

impl IsTerminal for GuestLog {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for GuestLog {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(Lines {
            component: self.0.clone(),
            line: Vec::new(),
            lines_left: MAX_LINES,
        })
    }
}

struct Lines {
    component: Arc<str>,
    line: Vec<u8>,
    lines_left: usize,
}

impl Lines {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.emit();
            } else if self.line.len() < MAX_LINE {
                self.line.push(b);
            }
        }
    }

    fn emit(&mut self) {
        if self.line.is_empty() {
            return;
        }
        match self.lines_left {
            0 => {}
            1 => tracing::warn!(component = %self.component, "more output dropped"),
            _ => {
                // Control characters could forge log lines or terminal escapes.
                let text =
                    String::from_utf8_lossy(&self.line).replace(|c: char| c.is_control(), " ");
                tracing::warn!(component = %self.component, "{}", text.trim_end());
            }
        }
        self.lines_left = self.lines_left.saturating_sub(1);
        self.line.clear();
    }
}

impl AsyncWrite for Lines {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().push(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().emit();
        Poll::Ready(Ok(()))
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Runs `f` and returns what it logged.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buffer {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let out = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || out.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    fn lines() -> Lines {
        Lines {
            component: "calendar".into(),
            line: Vec::new(),
            lines_left: MAX_LINES,
        }
    }

    #[test]
    fn logs_each_line_with_the_component_name() {
        let text = logged(|| {
            let mut l = lines();
            l.push(b"skipping unreadable event /a.ics: bad\nsecond ");
            l.push(b"half\r\n\x1b[31mred\n");
            l.push(b"unfinished");
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].contains("WARN"), "{text}");
        assert!(
            lines[0].contains("skipping unreadable event /a.ics: bad component=calendar"),
            "{text}"
        );
        assert!(
            lines[1].contains("second half component=calendar"),
            "{text}"
        );
        assert!(lines[2].contains(" [31mred"), "escape removed: {text}");
        assert!(
            lines[3].contains("unfinished"),
            "flushed when dropped: {text}"
        );
    }

    #[test]
    fn output_is_capped() {
        let text = logged(|| {
            let mut l = lines();
            l.push(&[b'x'; 3 * MAX_LINE]);
            l.push(b"\n");
            for _ in 0..2 * MAX_LINES {
                l.push(b"y\n");
            }
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0].matches('x').count(), MAX_LINE, "{}", lines[0]);
        assert_eq!(lines.len(), MAX_LINES, "{text}");
        assert!(
            lines[MAX_LINES - 1].contains("more output dropped"),
            "{text}"
        );
    }
}
