//! Remote probe transport: shell out to a `probe-rs` binary built with
//! `--features remote`, pointed at a `probe-rs serve` instance.
//!
//! This exists because probe-rs's remote RPC client lives only in the
//! `probe-rs-tools` binary (not exported as a library). The CLI surface we
//! use is deliberately minimal: `download`, `reset`. Auth: the CLI reads its
//! own remote token config (~/.config/probe-rs/).

use crate::TargetSpec;
use std::path::PathBuf;
use tokio::process::Command;

pub struct RemoteCliTarget {
    bin: PathBuf,
    host: String,
    chip: String,
    probe: Option<String>,
}

impl RemoteCliTarget {
    pub fn new(spec: &TargetSpec, host: &str) -> Self {
        RemoteCliTarget {
            bin: spec
                .probe_rs_bin
                .clone()
                .unwrap_or_else(|| PathBuf::from("probe-rs")),
            host: host.to_owned(),
            chip: spec.chip.clone(),
            probe: spec.probe.clone(),
        }
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(subcommand)
            .arg("--host")
            .arg(&self.host)
            .arg("--chip")
            .arg(&self.chip)
            .kill_on_drop(true);
        if let Some(probe) = &self.probe {
            cmd.arg("--probe").arg(probe);
        }
        cmd
    }

    async fn run(&self, mut cmd: Command, what: &str) -> anyhow::Result<()> {
        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("spawning {} for {what}: {e}", self.bin.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "{what} failed ({}):\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Ok(())
    }

    pub async fn flash(&mut self, elf: &std::path::Path) -> anyhow::Result<()> {
        let mut cmd = self.command("download");
        cmd.arg(elf);
        self.run(cmd, "remote flash").await?;
        // `download` leaves the core halted; bring it up like the local path.
        self.reset().await
    }

    pub async fn reset(&mut self) -> anyhow::Result<()> {
        let cmd = self.command("reset");
        self.run(cmd, "remote reset").await
    }
}
