//! Target lifecycle for banc: flash an ELF, reset, capture RTT — generic
//! over chips via probe-rs's target database (a name string is the entire
//! chip-specific surface).
//!
//! Two transports:
//!
//! - [`local`]: the probe-rs *library* against a locally attached USB probe.
//!   The library is blocking and `Session` is `Send`, so a dedicated thread
//!   owns the session and async callers talk to it over channels.
//! - [`remote`]: probe-rs's `remote` feature is CLI-only (the RPC client is
//!   not exported as a library), so a remote `probe-rs serve` is driven by
//!   shelling out to the `probe-rs` binary with `--host`. This seam is
//!   deliberately thin and should be replaced if upstream ever exports the
//!   RPC client. RTT capture over the remote CLI is not wired yet (Phase 2).

pub mod local;
pub mod remote;

use std::path::{Path, PathBuf};

/// Which probe and chip to use; mirrors `banc_host::config::TargetConfig`.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    /// probe-rs target-database name, e.g. "STM32WL55JCIx", "RP2350".
    pub chip: String,
    /// "VID:PID[:SERIAL]"; None = the only probe attached.
    pub probe: Option<String>,
    /// `probe-rs serve` URL; None = local USB probe via the library.
    pub probe_host: Option<String>,
    /// Path to the probe-rs binary for the remote path. Default: "probe-rs"
    /// from PATH. (The stock `cargo install probe-rs-tools` binary lacks the
    /// remote feature; point this at one built with `--features remote`.)
    pub probe_rs_bin: Option<PathBuf>,
}

/// A handle on the device under test.
pub enum Target {
    Local(local::LocalTarget),
    Remote(remote::RemoteCliTarget),
}

impl Target {
    pub async fn open(spec: &TargetSpec) -> anyhow::Result<Target> {
        match &spec.probe_host {
            None => Ok(Target::Local(local::LocalTarget::open(spec).await?)),
            Some(host) => Ok(Target::Remote(remote::RemoteCliTarget::new(spec, host))),
        }
    }

    /// Flash an ELF and leave the core running it.
    pub async fn flash(&mut self, elf: &Path) -> anyhow::Result<()> {
        match self {
            Target::Local(t) => t.flash(elf).await,
            Target::Remote(t) => t.flash(elf).await,
        }
    }

    pub async fn reset(&mut self) -> anyhow::Result<()> {
        match self {
            Target::Local(t) => t.reset().await,
            Target::Remote(t) => t.reset().await,
        }
    }

    /// Start streaming RTT up-channel 0 as lines into `sink` (one call per
    /// line, from a background thread). Caller keeps the returned handle
    /// alive for the duration of the capture.
    pub async fn start_rtt(
        &mut self,
        sink: impl Fn(String) + Send + 'static,
    ) -> anyhow::Result<RttCapture> {
        match self {
            Target::Local(t) => t.start_rtt(Box::new(sink)).await,
            Target::Remote(_) => anyhow::bail!(
                "RTT capture over the remote probe-rs CLI is not wired yet (Phase 2)"
            ),
        }
    }
}

/// Live RTT capture; dropping it stops the reader.
pub struct RttCapture {
    pub(crate) stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RttCapture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
