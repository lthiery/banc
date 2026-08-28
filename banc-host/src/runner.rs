//! libtest-mimic glue: async tests with fixture injection, honest runtime
//! self-skip when no rig is present, and evidence attached to failures.
//!
//! Suites are `harness = false` test binaries whose `main` calls [`run`].
//! Works under plain `cargo test` and under cargo-nextest (which runs each
//! test in its own process — hence the file lock in [`crate::rig`]).
//!
//! Each trial runs on its own tokio runtime, dropped when the trial ends:
//! every task the test (or its fixtures' libraries) spawned is torn down
//! before the next trial starts, so leaked tasks cannot hold sockets or
//! other resources across tests. The [`Rig`] outlives all of them: it is
//! acquired synchronously (no runtime in scope) and holds only
//! runtime-independent resources — connections belong to the per-test
//! fixtures.

use crate::evidence::Evidence;
use crate::rig::{Acquire, Rig};
use libtest_mimic::{Arguments, Completion, Failed, Trial};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

pub struct TestCx {
    pub rig: Arc<Rig>,
    pub evidence: Evidence,
    /// 1-based attempt number, for tests declared with [`BancTest::attempts`].
    pub attempt: u32,
}

pub type TestFuture = Pin<Box<dyn Future<Output = Result<(), Failed>> + Send>>;

pub struct BancTest {
    name: String,
    attempts: u32,
    f: Box<dyn Fn(TestCx) -> TestFuture + Send>,
}

impl BancTest {
    pub fn new(name: impl Into<String>, f: impl Fn(TestCx) -> TestFuture + Send + 'static) -> Self {
        BancTest {
            name: name.into(),
            attempts: 1,
            f: Box::new(f),
        }
    }

    /// Allow up to `attempts` runs of this test, passing on the first success.
    /// For scenarios whose medium is inherently noisy (RF over the air):
    /// assert protocol-level outcomes, tolerate a retry. Each attempt runs on
    /// a fresh runtime; evidence accumulates across attempts with a boundary
    /// marker, so a flaky pass still shows its failed attempt.
    pub fn attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }
}

/// Outcome of the once-per-process rig acquisition, shared across trials.
enum RigState {
    Ready(Arc<Rig>),
    Skip(String),
    Fail(String),
}

pub fn run(tests: Vec<BancTest>) -> ExitCode {
    let mut args = Arguments::from_args();
    // Hardware is exclusive; never run trials concurrently in-process.
    args.test_threads = Some(1);

    let rig_state: Arc<OnceLock<RigState>> = Arc::new(OnceLock::new());

    let trials: Vec<Trial> = tests
        .into_iter()
        .map(|test| {
            let rig_state = rig_state.clone();
            let name = test.name.clone();
            Trial::ignorable_test(test.name, move || {
                let state = rig_state.get_or_init(|| match Rig::acquire() {
                    Ok(rig) => RigState::Ready(Arc::new(rig)),
                    Err(Acquire::Skip(reason)) => RigState::Skip(reason),
                    Err(Acquire::Fail(e)) => RigState::Fail(format!("{e:#}")),
                });
                let rig = match state {
                    RigState::Ready(rig) => rig.clone(),
                    RigState::Skip(reason) => return Ok(Completion::ignored_with(reason.clone())),
                    RigState::Fail(e) => return Err(Failed::from(format!("rig unavailable: {e}"))),
                };
                let evidence = Evidence::new(&name);
                let mut result = Ok(());
                for attempt in 1..=test.attempts {
                    if attempt > 1 {
                        evidence.record("runner", format!("--- attempt {attempt} ---"));
                    }
                    // Fresh runtime per attempt: tasks leaked by a failed
                    // attempt are torn down before the retry starts.
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("building tokio runtime");
                    let cx = TestCx {
                        rig: rig.clone(),
                        evidence: evidence.clone(),
                        attempt,
                    };
                    result = rt.block_on((test.f)(cx));
                    match &result {
                        Ok(()) => break,
                        Err(failed) if attempt < test.attempts => {
                            evidence.record(
                                "runner",
                                format!(
                                    "attempt {attempt} failed: {}",
                                    failed.message().unwrap_or("test failed")
                                ),
                            );
                        }
                        Err(_) => {}
                    }
                }
                match result {
                    Ok(()) => Ok(Completion::Completed),
                    Err(failed) => {
                        let mut msg = failed
                            .message()
                            .map(|m| m.to_string())
                            .unwrap_or_else(|| "test failed".to_owned());
                        if !evidence.is_empty() {
                            let dir = rig.artifacts_dir();
                            match evidence.persist(&dir) {
                                Ok(path) => {
                                    msg.push_str(&format!(
                                        "\n--- evidence (tail) ---\n{}full log: {}",
                                        evidence.tail(40),
                                        path.display()
                                    ));
                                }
                                Err(e) => {
                                    msg.push_str(&format!(
                                        "\n--- evidence (tail; persist failed: {e}) ---\n{}",
                                        evidence.tail(40)
                                    ));
                                }
                            }
                        }
                        Err(Failed::from(msg))
                    }
                }
            })
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    conclusion.exit_code()
}
