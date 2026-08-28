//! Machine-readable per-test results, appended to `results.jsonl` in the
//! artifacts dir as each trial finishes.
//!
//! The human-facing libtest output stays the interface for people; this file
//! is the interface for machines (CI annotations, dashboards, agents), which
//! otherwise re-parse cargo output. One JSON object per line, one line per
//! executed trial; a run cut short by a crash keeps the lines already
//! written. Appending (not rewriting) also keeps the file correct under
//! cargo-nextest, where every test runs in its own process.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Bumped when the shape of [`TestRecord`] changes incompatibly, so a
/// consumer can refuse records it does not understand instead of misreading
/// them.
pub const SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    /// No rig on this machine; `reason` says why. Reported so a consumer can
    /// tell "ran and passed" from "did not run" without inspecting exit codes.
    Skipped,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TestRecord {
    pub schema: u32,
    /// Test-binary stem ("scenarios_l072"), the suite identity a caller
    /// selected with `--test`.
    pub suite: String,
    pub test: String,
    pub outcome: Outcome,
    /// Failure message or skip reason. Bare, without the evidence tail the
    /// human-facing failure carries; the evidence file is linked instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Attempts consumed (1 unless the test was declared with retries).
    pub attempts: u32,
    pub duration_ms: u64,
    /// Unix seconds when the trial finished.
    pub finished_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<PathBuf>,
}

impl TestRecord {
    pub fn new(suite: &str, test: &str, outcome: Outcome) -> Self {
        TestRecord {
            schema: SCHEMA,
            suite: suite.to_owned(),
            test: test.to_owned(),
            outcome,
            reason: None,
            attempts: 1,
            duration_ms: 0,
            finished_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            evidence: None,
        }
    }
}

/// Append one record to `results.jsonl` under `dir`. Best-effort by design:
/// results are a byproduct of the run, and a full disk or bad permissions
/// must not turn a hardware verdict into an I/O failure.
pub fn append(dir: &Path, record: &TestRecord) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("results.jsonl"))
    else {
        return;
    };
    // One write per record: O_APPEND keeps whole small lines intact even
    // when nextest processes interleave.
    let _ = file.write_all(format!("{line}\n").as_bytes());
}

/// The suite name: the test binary's file stem with cargo's trailing
/// `-<hash>` disambiguator stripped.
pub fn suite_name() -> String {
    let stem = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_owned());
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            name.to_owned()
        }
        _ => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_one_line_per_record() {
        let dir = std::env::temp_dir().join(format!("banc-results-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut rec = TestRecord::new("suite", "a", Outcome::Passed);
        append(&dir, &rec);
        rec.test = "b".into();
        rec.outcome = Outcome::Failed;
        rec.reason = Some("boom".into());
        append(&dir, &rec);
        let text = std::fs::read_to_string(dir.join("results.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: TestRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed.outcome, Outcome::Failed);
        assert_eq!(parsed.reason.as_deref(), Some("boom"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn passed_record_omits_noise_fields() {
        let rec = TestRecord::new("s", "t", Outcome::Passed);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("reason"));
        assert!(!json.contains("evidence"));
    }
}
