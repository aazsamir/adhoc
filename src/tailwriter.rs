//! Forwards a helper's output to its destination and remembers the last few
//! lines. If the helper dies during startup, those lines are the best
//! explanation available.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Mutex;

pub struct TailWriter {
    inner: Mutex<Inner>,
    limit: usize,
}

#[derive(Default)]
struct Inner {
    /// Bytes of the current line, up to the next '\n'.
    pending: Vec<u8>,
    /// The last complete lines.
    tail: VecDeque<String>,
}

impl TailWriter {
    pub fn new(limit: usize) -> TailWriter {
        TailWriter {
            inner: Mutex::new(Inner::default()),
            limit,
        }
    }

    /// Writes `bytes` to `dest` and remembers them.
    pub fn forward<W: Write + ?Sized>(&self, dest: &mut W, bytes: &[u8]) -> io::Result<()> {
        dest.write_all(bytes)?;
        let _ = dest.flush();
        self.record(bytes);
        Ok(())
    }

    /// Remembers already-forwarded bytes.
    pub fn record(&self, bytes: &[u8]) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        inner.pending.extend_from_slice(bytes);
        while let Some(newline) = inner.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = inner.pending.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line);
            self.remember(&mut inner, &text);
        }
    }

    fn remember(&self, inner: &mut Inner, line: &str) {
        inner.tail.push_back(trim_newline(line));
        while inner.tail.len() > self.limit {
            inner.tail.pop_front();
        }
    }

    /// Returns the collected lines and empties the buffer.
    pub fn tail(&self) -> Vec<String> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut lines: Vec<String> = inner.tail.drain(..).collect();
        let fragment = trim_newline(&String::from_utf8_lossy(&inner.pending));
        if !fragment.is_empty() {
            lines.push(fragment);
        }
        inner.pending.clear();
        lines
    }
}

fn trim_newline(text: &str) -> String {
    text.trim_end_matches(['\r', '\n']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_last_lines() {
        let writer = TailWriter::new(3);
        writer.record(b"a\nb\nc\nd\ne\n");
        assert_eq!(writer.tail(), vec!["c", "d", "e"]);
        // Draining empties the buffer.
        assert!(writer.tail().is_empty());
    }

    #[test]
    fn handles_partial_lines_and_crlf() {
        let writer = TailWriter::new(5);
        writer.record(b"first\r\nsec");
        writer.record(b"ond\npartial");
        assert_eq!(writer.tail(), vec!["first", "second", "partial"]);
    }

    #[test]
    fn forwards_bytes() {
        let writer = TailWriter::new(2);
        let mut sink: Vec<u8> = Vec::new();
        writer.forward(&mut sink, b"hello\n").unwrap();
        assert_eq!(sink, b"hello\n");
        assert_eq!(writer.tail(), vec!["hello"]);
    }

    #[test]
    fn concurrent_writers_do_not_lose_lines() {
        let writer = std::sync::Arc::new(TailWriter::new(1000));
        let mut handles = Vec::new();
        for index in 0..4 {
            let writer = writer.clone();
            handles.push(std::thread::spawn(move || {
                for line in 0..100 {
                    writer.record(format!("{index}-{line}\n").as_bytes());
                }
                let mut sink = std::io::sink();
                writer.forward(&mut sink, b"tail\n").unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let lines = writer.tail();
        // 4 threads * (100 numbered lines + 1 "tail").
        assert_eq!(lines.len(), 404);
        assert_eq!(lines.last().unwrap(), "tail");
    }
}
