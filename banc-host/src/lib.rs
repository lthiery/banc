//! Host-side fixtures and runner for the banc HIL test framework.
//!
//! The shape of a banc test suite:
//!
//! ```ignore
//! use banc_host::{run, BancTest, TestCx};
//!
//! fn main() -> std::process::ExitCode {
//!     run(vec![BancTest::new("blink_observed", |cx| {
//!         Box::pin(async move {
//!             let assistant = cx.rig.assistant("a0").await?;
//!             // drive the target, assert on assistant-observed ground truth
//!             Ok(())
//!         })
//!     })])
//! }
//! ```
//!
//! On a machine without a rig (no `banc-rig.toml`, no `BANC_RIG`), every test
//! reports **ignored** with a reason — never passed, never failed — so
//! `cargo test` stays green off-rig and honest on-rig.

pub mod config;
pub mod device;
pub mod evidence;
pub mod expect;
pub mod node;
pub mod rig;
pub mod runner;

pub use config::RigConfig;
pub use device::{DeviceSuite, DeviceTest};
pub use evidence::Evidence;
pub use libtest_mimic::Failed;
pub use node::Node;
pub use rig::{Acquire, Rig};
pub use runner::{run, BancTest, TestCx};
