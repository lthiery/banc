//! Self-skip smoke suite: on a machine without a rig these report as
//! ignored (not passed, not failed); on a rig they exercise acquisition
//! and the assistant identify handshake.

use banc_host::{BancTest, TestCx, run};
use std::process::ExitCode;

fn main() -> ExitCode {
    run(vec![
        BancTest::new("rig_acquired", |cx: TestCx| {
            Box::pin(async move {
                cx.evidence.record(
                    "smoke",
                    format!(
                        "rig '{}' acquired: {} assistant(s), {} instrument(s)",
                        cx.rig.config.rig.name.as_deref().unwrap_or("unnamed"),
                        cx.rig.config.assistants.len(),
                        cx.rig.config.instruments.len(),
                    ),
                );
                Ok(())
            })
        }),
        BancTest::new("assistants_identify", |cx: TestCx| {
            Box::pin(async move {
                for a in cx.rig.config.assistants.clone() {
                    let node = cx
                        .rig
                        .assistant(&a.name)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    cx.evidence.record(
                        "smoke",
                        format!("assistant '{}' identified: {:?}", a.name, node.identity),
                    );
                }
                Ok(())
            })
        }),
    ])
}
