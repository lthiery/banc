//! Per-test evidence: everything observed while a test ran, attached to the
//! failure when it fails and discarded when it passes.
//!
//! Producers (RTT readers, node subscriptions, instrument drivers) hold a
//! cheap clone and `record()` from any task or thread.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Cap on retained evidence entries. A misbehaving target that logs without
/// bound would otherwise grow host memory for the whole test; past this we
/// drop the oldest entries (the tail is what a failure shows anyway) and
/// count the drops so the record stays honest about being truncated.
const MAX_ENTRIES: usize = 50_000;

#[derive(Clone)]
pub struct Evidence {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    test: String,
    started: Instant,
    entries: VecDeque<Entry>,
    dropped: u64,
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
                entries: VecDeque::new(),
                dropped: 0,
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
        if inner.entries.len() >= MAX_ENTRIES {
            inner.entries.pop_front();
            inner.dropped += 1;
        }
        inner.entries.push_back(Entry {
            at_us,
            source,
            line: line.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Last `n` lines, formatted for inline display under a failure.
    pub fn tail(&self, n: usize) -> String {
        let inner = self.inner.lock().unwrap();
        let skip = inner.entries.len().saturating_sub(n);
        let mut out = String::new();
        if inner.dropped > 0 && skip == 0 {
            let _ = writeln!(out, "[... {} earlier entries dropped ...]", inner.dropped);
        }
        for e in inner.entries.iter().skip(skip) {
            let _ = writeln!(
                out,
                "[{:>10.3}ms {}] {}",
                e.at_us as f64 / 1000.0,
                e.source,
                e.line
            );
        }
        out
    }

    /// Write the retained log under `dir` and return the file path. If the
    /// entry cap was hit the header records how many earlier lines were
    /// dropped, so the file never reads as complete when it is not.
    pub fn persist(&self, dir: &std::path::Path) -> std::io::Result<PathBuf> {
        let inner = self.inner.lock().unwrap();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.evidence.log", sanitize(&inner.test)));
        let mut out = String::new();
        if inner.dropped > 0 {
            let _ = writeln!(
                out,
                "[... {} earlier entries dropped (evidence cap) ...]",
                inner.dropped
            );
        }
        for e in &inner.entries {
            let _ = writeln!(
                out,
                "[{:>10.3}ms {}] {}",
                e.at_us as f64 / 1000.0,
                e.source,
                e.line
            );
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }
}

/// Make a test path safe as a single filename component. Only ASCII
/// alphanumerics, `-`, and `_` pass through; everything else (path
/// separators `/` and `\\`, `:`, and `.` so `..` cannot survive) collapses to
/// `_`, so an externally-assembled test name cannot traverse directories.
fn sanitize(test: &str) -> String {
    test.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

    #[test]
    fn entries_are_capped_and_drops_are_counted() {
        let ev = Evidence::new("t");
        for i in 0..(MAX_ENTRIES + 100) {
            ev.record("test", format!("line {i}"));
        }
        let inner = ev.inner.lock().unwrap();
        assert_eq!(
            inner.entries.len(),
            MAX_ENTRIES,
            "retained set stays capped"
        );
        assert_eq!(inner.dropped, 100, "dropped count is exact");
        // Oldest survivor is the 100th line; the first 100 were evicted.
        assert_eq!(inner.entries.front().unwrap().line, "line 100");
    }

    #[test]
    fn tail_notes_dropped_entries() {
        let ev = Evidence::new("t");
        for i in 0..(MAX_ENTRIES + 5) {
            ev.record("test", format!("line {i}"));
        }
        // A tail covering the whole retained set surfaces the drop notice.
        let tail = ev.tail(MAX_ENTRIES);
        assert!(
            tail.contains("5 earlier entries dropped"),
            "missing drop notice"
        );
    }

    #[test]
    fn sanitize_neutralizes_separators_and_traversal() {
        assert_eq!(sanitize("suite/case:1"), "suite_case_1");
        assert_eq!(sanitize(r"..\..\etc"), "______etc");
        assert_eq!(sanitize("ok.name-1"), "ok_name-1");
    }
}
