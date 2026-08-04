//! # banc — banc d'essai
//!
//! A host-evaluated hardware-in-the-loop test framework for embedded Rust.
//! The harness is the bench; it is never the thing being tested and never
//! knows what's bolted to it.
//!
//! A std `cargo test` suite on the host orchestrates the device under test
//! (flashed and reset via probe-rs), assistant boards acting as off-target
//! peripherals and observers (speaking [`banc_icd`] over postcard-rpc), and
//! bench instruments. Assertions run host-side against ground truth observed
//! on the wire — never against the target's self-report alone. Timing is
//! asserted from assistant-local timestamps, never host wall-clock over
//! serial.
//!
//! On machines without a rig, suites self-skip (report ignored, not passed
//! or failed), so `cargo test` stays honest everywhere.
//!
//! The litmus test for what belongs here: *would this exist if the device
//! under test were a CAN transceiver?* Domain knowledge (regions, data
//! rates, protocol state machines) lives in downstream suites — called
//! *essais* by convention — which provide their own ICD crates, domain
//! fixtures wrapping banc's, and test suites.
//!
//! - [`host`]: fixtures, rig topology config, runner, evidence
//! - [`probe`]: target lifecycle (flash / reset / RTT) via probe-rs
//! - [`icd`]: node-management + reference-assistant wire types
//! - [`instrument`]: bench equipment in domain-neutral units

pub use banc_host as host;
pub use banc_icd as icd;
pub use banc_instrument as instrument;
pub use banc_probe as probe;

pub mod prelude {
    pub use banc_host::{run, BancTest, DeviceSuite, DeviceTest, Evidence, Failed, Rig, TestCx};
    pub use banc_host::expect::{expect_matching, expect_quiet};
    pub use banc_host::paired_suite;
    pub use banc_instrument::for_each_setpoint;
    pub use banc_probe::{Target, TargetSpec};
}
