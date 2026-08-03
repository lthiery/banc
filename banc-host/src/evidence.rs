//! Per-test evidence: everything observed while a test ran, attached to the
//! failure when it fails and discarded when it passes.
//!
//! Producers (RTT readers, node subscriptions, instrument drivers) hold a
//! cheap clone and `record()` from any task or thread.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct Evidence {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    test: String,
    started: Instant,
    entries: Vec<Entry>,
}

struct Entry {
    at_us: u128,
    source: &'static str,
    line: String,
}

impl Evidence {
    pub fn new(test: &str) -> Self {
        Evidence {
            inner: Arc::new(Mutex::new(Inner {
                test: test.to_owned(),
                started: Instant::now(),
                entries: Vec::new(),
            })),
        }
    }

    /// Record one line from a named source ("defmt", "assistant:a0", ...).
    /// Timestamped with host time relative to test start; this timestamp is
    /// for correlating the narrative, never for timing assertions — those use
    /// assistant-local timestamps carried inside the events themselves.
    pub fn record(&self, source: &'static str, line: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap();
        let at_us = inner.started.elapsed().as_micros();
        inner.entries.push(Entry { at_us, source, line: line.into() });
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Last `n` lines, formatted for inline display under a failure.
    pub fn tail(&self, n: usize) -> String {
        let inner = self.inner.lock().unwrap();
        let skip = inner.entries.len().saturating_sub(n);
        let mut out = String::new();
        for e in &inner.entries[skip..] {
            let _ = writeln!(out, "[{:>10.3}ms {}] {}", e.at_us as f64 / 1000.0, e.source, e.line);
        }
        out
    }

    /// Write the full log under `dir` and return the file path.
    pub fn persist(&self, dir: &std::path::Path) -> std::io::Result<PathBuf> {
        let inner = self.inner.lock().unwrap();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.evidence.log", inner.test.replace(['/', ':'], "_")));
        let mut out = String::new();
        for e in &inner.entries {
            let _ = writeln!(out, "[{:>10.3}ms {}] {}", e.at_us as f64 / 1000.0, e.source, e.line);
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_last_lines() {
        let ev = Evidence::new("t");
        for i in 0..10 {
            ev.record("test", format!("line {i}"));
        }
        let tail = ev.tail(3);
        assert!(tail.contains("line 7") && tail.contains("line 9"));
        assert!(!tail.contains("line 6"));
    }

    #[test]
    fn persist_writes_file() {
        let ev = Evidence::new("suite/case:1");
        ev.record("x", "hello");
        let dir = std::env::temp_dir().join("banc-evidence-test");
        let path = ev.persist(&dir).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello"));
        std::fs::remove_file(path).ok();
    }
}
