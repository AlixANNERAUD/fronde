//! A component's standard error, line by line, as warnings in the hub's log under its name.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// Longer lines are cut.
const MAX_LINE: usize = 1024;
/// Per call: a component that writes more is cut short, so that it cannot flood the log.
const MAX_LINES: usize = 100;

/// The standard error of one call to a component. WASI opens a new stream each time the guest
/// asks for its standard error, so both the pending partial line and the line budget belong to the
/// call and every stream shares them.
pub(crate) struct GuestLog {
    shared: Arc<Shared>,
}

impl GuestLog {
    /// For one call to the component `name`.
    pub(crate) fn new(name: &str) -> Self {
        Self {
            shared: Arc::new(Shared {
                component: name.into(),
                state: Mutex::new(State {
                    line: Vec::new(),
                    lines_left: MAX_LINES,
                }),
            }),
        }
    }
}

impl Drop for GuestLog {
    fn drop(&mut self) {
        self.shared.emit();
    }
}

impl IsTerminal for GuestLog {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for GuestLog {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(Lines {
            shared: self.shared.clone(),
        })
    }
}

struct Shared {
    component: Arc<str>,
    state: Mutex<State>,
}

struct State {
    line: Vec<u8>,
    lines_left: usize,
}

impl Shared {
    fn push(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for &b in bytes {
            if b == b'\n' {
                self.emit_locked(&mut state);
            } else if state.line.len() < MAX_LINE {
                state.line.push(b);
            }
        }
    }

    fn emit(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.emit_locked(&mut state);
    }

    fn emit_locked(&self, state: &mut State) {
        if state.line.is_empty() {
            return;
        }
        let left = state.lines_left;
        state.lines_left = left.saturating_sub(1);
        match left {
            0 => {}
            1 => tracing::warn!(component = %self.component, "more output dropped"),
            _ => {
                // Control characters could forge log lines or terminal escapes.
                let text =
                    String::from_utf8_lossy(&state.line).replace(|c: char| c.is_control(), " ");
                tracing::warn!(component = %self.component, "{}", text.trim_end());
            }
        }
        state.line.clear();
    }
}

struct Lines {
    shared: Arc<Shared>,
}

impl AsyncWrite for Lines {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.shared.push(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Write `bytes` to a stream as WASI would.
    fn write(stream: &mut Pin<Box<dyn AsyncWrite + Send + Sync>>, bytes: &[u8]) {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let written = stream.as_mut().poll_write(&mut cx, bytes);
        assert!(matches!(written, Poll::Ready(Ok(n)) if n == bytes.len()));
    }

    fn lines_left(log: &GuestLog) -> usize {
        log.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lines_left
    }

    #[test]
    fn logs_each_line_with_the_component_name() {
        let text = logged(|| {
            let log = GuestLog::new("calendar");
            let mut stream = Box::into_pin(log.async_stream());
            write(
                &mut stream,
                b"skipping unreadable event /a.ics: bad\nsecond ",
            );
            write(&mut stream, b"half\r\n\x1b[31mred\n");
            write(&mut stream, b"unfinished");
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
            let log = GuestLog::new("calendar");
            let mut stream = Box::into_pin(log.async_stream());
            write(&mut stream, &[b'x'; 3 * MAX_LINE]);
            write(&mut stream, b"\n");
            for _ in 0..2 * MAX_LINES {
                write(&mut stream, b"y\n");
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

    #[test]
    fn partial_line_continues_across_streams() {
        let text = logged(|| {
            let log = GuestLog::new("calendar");
            let mut first = Box::into_pin(log.async_stream());
            write(&mut first, b"first half ");
            drop(first);
            assert_eq!(lines_left(&log), MAX_LINES);

            let mut second = Box::into_pin(log.async_stream());
            write(&mut second, b"second half");
            assert_eq!(lines_left(&log), MAX_LINES);
            write(&mut second, b"\n");
            drop(second);
            assert_eq!(lines_left(&log), MAX_LINES - 1);
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(
            lines[0].contains("first half second half component=calendar"),
            "{text}"
        );
    }

    #[test]
    fn the_cap_is_per_call_whatever_the_streams() {
        let text = logged(|| {
            let log = GuestLog::new("calendar");
            // The guest fetches its standard error again for each write.
            for _ in 0..2 {
                let mut stream = Box::into_pin(log.async_stream());
                for _ in 0..MAX_LINES {
                    write(&mut stream, b"y\n");
                }
            }
            write(&mut Box::into_pin(log.async_stream()), b"later\n");
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_LINES, "{text}");
        assert!(
            lines[MAX_LINES - 1].contains("more output dropped"),
            "{text}"
        );
        assert!(!text.contains("later"), "{text}");

        // The next call starts with a full budget.
        let text = logged(|| {
            let mut stream = Box::into_pin(GuestLog::new("calendar").async_stream());
            write(&mut stream, b"again\n");
        });
        assert!(text.contains("again component=calendar"), "{text}");
    }
}
