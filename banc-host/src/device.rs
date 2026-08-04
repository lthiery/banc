//! The device half of a paired scenario: one on-target embedded-test,
//! run as a child `cargo test` invocation in the firmware crate (which
//! builds, flashes and executes it via that crate's configured runner,
//! e.g. probe-rs).
//!
//! Lifecycle contract, enforced by [`crate::paired_suite!`]:
//! - the test is spawned with kill-on-drop, so a host-side scenario that
//!   fails early takes the build/flash child down with its trial runtime
//!   instead of leaving the probe busy for the next scenario;
//! - every scenario ends by awaiting [`DeviceTest::verdict`]; a scenario
//!   that never started its device test fails rather than silently
//!   passing on host-side evidence alone.

use libtest_mimic::Failed;
use std::path::PathBuf;
use std::process::Stdio;

/// How to invoke the device-side test crate. One per suite, cloned into
/// each scenario.
#[derive(Clone)]
pub struct DeviceSuite {
    /// Directory of the firmware crate whose tests run on-target.
    pub crate_dir: PathBuf,
    /// Test target within that crate: `cargo test --test <this>`.
    pub test_target: String,
    /// Extra cargo args, e.g. `["--release", "--offline"]`. Empty by default.
    pub cargo_args: Vec<String>,
    /// Program to invoke; "cargo" unless overridden (unit tests use this).
    pub program: String,
}

impl DeviceSuite {
    pub fn new(crate_dir: impl Into<PathBuf>, test_target: impl Into<String>) -> Self {
        DeviceSuite {
            crate_dir: crate_dir.into(),
            test_target: test_target.into(),
            cargo_args: Vec::new(),
            program: "cargo".into(),
        }
    }

    /// A handle for one on-target test, not yet started: the scenario
    /// decides when to flash (typically after its network fixture is up).
    pub fn test(&self, test_path: impl Into<String>) -> DeviceTest {
        DeviceTest { suite: self.clone(), test_path: test_path.into(), state: State::Idle }
    }
}

enum State {
    Idle,
    Running(tokio::task::JoinHandle<Result<std::process::ExitStatus, String>>),
    Done(Result<(), String>),
}

pub struct DeviceTest {
    suite: DeviceSuite,
    test_path: String,
    state: State,
}

impl DeviceTest {
    /// Exact on-target test this handle runs (`--exact` filter).
    pub fn test_path(&self) -> &str {
        &self.test_path
    }

    /// Build/flash/run the on-target test. Output is inherited so device
    /// logs interleave with the host's. Must be called from within the
    /// trial's runtime; panics if started twice.
    pub fn start(&mut self) {
        assert!(
            matches!(self.state, State::Idle),
            "device test '{}' started twice",
            self.test_path
        );
        // A missing crate dir would otherwise surface as a silent no-show:
        // the child dies unspawned, the device transmits nothing, and the
        // scenario times out on its first expect with no hint why (seen
        // 2026-08-04, CI dispatched on a ref without the fw crate).
        if !self.suite.crate_dir.is_dir() {
            self.state = State::Done(Err(format!(
                "device crate dir {} does not exist",
                self.suite.crate_dir.display()
            )));
            return;
        }
        let mut cmd = tokio::process::Command::new(&self.suite.program);
        cmd.arg("test")
            .args(&self.suite.cargo_args)
            .args(["--test", &self.suite.test_target, "--", "--exact", &self.test_path])
            .current_dir(&self.suite.crate_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        self.state =
            State::Running(tokio::spawn(async move { cmd.status().await.map_err(|e| e.to_string()) }));
    }

    /// If the device side has already failed (spawn error, missing crate
    /// dir, early exit), return its error without waiting. Context for
    /// host-side failures: a scenario that times out because the device
    /// never ran should say so, not just "deadline elapsed".
    pub async fn failure_context(&mut self) -> Option<String> {
        let finished = match &self.state {
            State::Done(r) => r.is_err(),
            State::Running(handle) => handle.is_finished(),
            State::Idle => false,
        };
        if !finished {
            return None;
        }
        self.verdict()
            .await
            .err()
            .map(|f| f.message().unwrap_or("device test failed").to_string())
    }

    /// The device-side result (semihosting exit code via the runner).
    /// Idempotent: awaits the child on first call, cached afterwards, so a
    /// scenario may consult it mid-body and the suite's trailing check is
    /// still valid. Never started => failure, not a pass.
    pub async fn verdict(&mut self) -> Result<(), Failed> {
        let result = match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => {
                return Err(Failed::from(format!(
                    "device test '{}' was never started",
                    self.test_path
                )))
            }
            State::Running(handle) => match handle.await {
                Err(join) => Err(format!("device test task failed: {join}")),
                Ok(Err(spawn)) => Err(format!(
                    "spawning {} in {}: {spawn}",
                    self.suite.program,
                    self.suite.crate_dir.display()
                )),
                Ok(Ok(status)) if status.success() => Ok(()),
                Ok(Ok(status)) => {
                    Err(format!("device-side test '{}' reported failure ({status})", self.test_path))
                }
            },
            State::Done(result) => result,
        };
        self.state = State::Done(result.clone());
        result.map_err(Failed::from)
    }
}

/// Generate a paired-scenario suite: a `harness = false` `main` where every
/// scenario owns a [`DeviceTest`] handle bound to its on-target twin.
///
/// ```ignore
/// banc_host::paired_suite! {
///     device_suite: my_device_suite(),
///     scenario join_ok, device_test: "tests::join_ok", |cx, device| {
///         let mut net = Net::bind(&cx).await?;
///         device.start();
///         net.expect("JoinRequest", secs(120), |e| ...).await?;
///     }
///     // device_test omitted => derived as "tests::<scenario name>"
///     scenario rx2_fallback, |cx, device| { ... }
/// }
/// ```
///
/// The macro wires what today is hand-written per scenario: the
/// `BancTest`/`Box::pin` wrapper, the device handle (named from the scenario
/// unless overridden), and the trailing `device.verdict().await` so no
/// scenario can pass without its device half agreeing.
#[macro_export]
macro_rules! paired_suite {
    (@device_test $name:ident) => { concat!("tests::", stringify!($name)) };
    (@device_test $name:ident $dt:expr) => { $dt };
    (
        device_suite: $suite:expr,
        $(
            scenario $name:ident $(, device_test: $dt:expr)? , |$cx:ident, $dev:ident| $body:block
        )*
    ) => {
        fn main() -> ::std::process::ExitCode {
            let __suite = $suite;
            $crate::run(::std::vec![
                $(
                    {
                        let __suite = __suite.clone();
                        $crate::BancTest::new(
                            stringify!($name),
                            move |$cx: $crate::TestCx| {
                                ::std::boxed::Box::pin(async move {
                                    let mut $dev =
                                        __suite.test($crate::paired_suite!(@device_test $name $($dt)?));
                                    let __body: ::std::result::Result<(), $crate::Failed> =
                                        async { $body Ok(()) }.await;
                                    if let ::std::result::Result::Err(e) = __body {
                                        let msg = e.message().unwrap_or("test failed").to_string();
                                        return ::std::result::Result::Err(
                                            match $dev.failure_context().await {
                                                ::std::option::Option::Some(dev_err) => $crate::Failed::from(
                                                    ::std::format!("{msg}\ndevice side: {dev_err}"),
                                                ),
                                                ::std::option::Option::None => $crate::Failed::from(msg),
                                            },
                                        );
                                    }
                                    $dev.verdict().await
                                })
                            },
                        )
                    }
                ),*
            ])
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(program: &str) -> DeviceSuite {
        let mut s = DeviceSuite::new(std::env::temp_dir(), "join");
        s.program = program.into();
        s
    }

    #[tokio::test]
    async fn verdict_tracks_child_exit() {
        let mut ok = suite("true").test("tests::x");
        ok.start();
        assert!(ok.verdict().await.is_ok());
        // Cached: a second consultation agrees.
        assert!(ok.verdict().await.is_ok());

        let mut bad = suite("false").test("tests::x");
        bad.start();
        assert!(bad.verdict().await.is_err());
        assert!(bad.verdict().await.is_err());
    }

    #[tokio::test]
    async fn missing_crate_dir_fails_at_start_with_context() {
        let mut suite = DeviceSuite::new("/nonexistent/fw-crate", "join");
        suite.program = "true".into();
        let mut dt = suite.test("tests::x");
        dt.start();
        // Available immediately as context, before any verdict await.
        let ctx = dt.failure_context().await.unwrap();
        assert!(ctx.contains("does not exist"), "unexpected context: {ctx}");
        assert!(dt.verdict().await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_failure_context_while_device_still_running() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("banc-slow-{}", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut suite = DeviceSuite::new(std::env::temp_dir(), "join");
        suite.program = script.to_str().unwrap().into();
        let mut dt = suite.test("tests::x");
        dt.start();
        assert!(dt.failure_context().await.is_none());
        // Not consumed: the handle is still awaitable afterwards.
        assert!(matches!(dt.state, State::Running(_)));
        std::fs::remove_file(script).ok();
    }

    #[tokio::test]
    async fn never_started_is_a_failure() {
        let mut dt = suite("true").test("tests::x");
        let err = dt.verdict().await.unwrap_err();
        assert!(err.message().unwrap().contains("never started"));
    }

    #[tokio::test]
    async fn spawn_error_is_reported_not_panicked() {
        let mut dt = suite("/nonexistent/no-such-program").test("tests::x");
        dt.start();
        assert!(dt.verdict().await.is_err());
    }
}
