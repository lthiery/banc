//! Local probe transport: the probe-rs library on a dedicated thread.
//!
//! probe-rs is blocking and `Core` borrows `&mut Session`, so one thread
//! owns the `Session` for the target's lifetime; async callers send commands
//! over a channel and await replies. RTT polling runs on the same thread
//! between commands.

use crate::{CaptureHealth, RttCapture, TargetSpec};
use probe_rs::flashing::{ElfLoader, ElfOptions, download_file};
use probe_rs::probe::DebugProbeSelector;
use probe_rs::probe::list::Lister;
use probe_rs::rtt::{Rtt, ScanRegion};
use probe_rs::{Permissions, Session};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::Duration;

type LineSink = Box<dyn Fn(String) + Send>;

/// Cap on an unterminated RTT line. Past this, `pending` is flushed as one
/// oversized line so a newline-less device cannot grow host memory unbounded.
const MAX_LINE: usize = 64 * 1024;

enum Cmd {
    Flash(
        std::path::PathBuf,
        tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ),
    Reset(tokio::sync::oneshot::Sender<anyhow::Result<()>>),
    StartRtt {
        sink: LineSink,
        stop: tokio::sync::oneshot::Receiver<()>,
        health: CaptureHealth,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
}

pub struct LocalTarget {
    tx: mpsc::Sender<Cmd>,
}

impl LocalTarget {
    pub async fn open(spec: &TargetSpec) -> anyhow::Result<Self> {
        let spec = spec.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("banc-probe".into())
            .spawn(move || worker(spec, ready_tx, rx))?;
        ready_rx.await??;
        Ok(LocalTarget { tx })
    }

    pub async fn flash(&mut self, elf: &std::path::Path) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Flash(elf.to_path_buf(), tx))
            .map_err(|_| anyhow::anyhow!("probe worker thread gone"))?;
        rx.await?
    }

    pub async fn reset(&mut self) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Reset(tx))
            .map_err(|_| anyhow::anyhow!("probe worker thread gone"))?;
        rx.await?
    }

    pub async fn start_rtt(&mut self, sink: LineSink) -> anyhow::Result<RttCapture> {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let health = CaptureHealth::default();
        self.tx
            .send(Cmd::StartRtt {
                sink,
                stop: stop_rx,
                health: health.clone(),
                ready: ready_tx,
            })
            .map_err(|_| anyhow::anyhow!("probe worker thread gone"))?;
        ready_rx.await??;
        Ok(RttCapture {
            stop: Some(stop_tx),
            health,
        })
    }
}

fn open_session(spec: &TargetSpec) -> anyhow::Result<Session> {
    let lister = Lister::new();
    let probe = match &spec.probe {
        Some(sel) => {
            let selector = DebugProbeSelector::from_str(sel)
                .map_err(|e| anyhow::anyhow!("bad probe selector '{sel}': {e}"))?;
            lister
                .open(selector)
                .map_err(|e| anyhow::anyhow!("opening probe '{sel}': {e}"))?
        }
        None => {
            let probes = lister.list_all();
            anyhow::ensure!(
                probes.len() == 1,
                "expected exactly one debug probe, found {} (set target.probe in banc-rig.toml)",
                probes.len()
            );
            lister
                .open(&probes[0])
                .map_err(|e| anyhow::anyhow!("opening probe: {e}"))?
        }
    };
    let session = probe
        .attach(spec.chip.as_str(), Permissions::default())
        .map_err(|e| anyhow::anyhow!("attaching to {}: {e}", spec.chip))?;
    Ok(session)
}

struct RttState {
    rtt: Rtt,
    sink: LineSink,
    stop: tokio::sync::oneshot::Receiver<()>,
    health: CaptureHealth,
    pending: Vec<u8>,
}

fn worker(
    spec: TargetSpec,
    ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    rx: mpsc::Receiver<Cmd>,
) {
    let mut session = match open_session(&spec) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let mut rtt: Option<RttState> = None;
    loop {
        // With RTT active, poll the channel between command checks; without,
        // block until the next command (or shutdown when LocalTarget drops).
        let cmd = if rtt.is_some() {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        };

        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Flash(path, reply) => {
                    rtt = None; // flashing invalidates any RTT attachment
                    let result =
                        download_file(&mut session, &path, ElfLoader(ElfOptions::default()))
                            .map_err(|e| anyhow::anyhow!("flashing {}: {e}", path.display()))
                            .and_then(|_| reset_core(&mut session));
                    let _ = reply.send(result);
                }
                Cmd::Reset(reply) => {
                    let _ = reply.send(reset_core(&mut session));
                }
                Cmd::StartRtt {
                    sink,
                    stop,
                    health,
                    ready,
                } => match attach_rtt(&mut session) {
                    Ok(attached) => {
                        rtt = Some(RttState {
                            rtt: attached,
                            sink,
                            stop,
                            health,
                            pending: Vec::new(),
                        });
                        let _ = ready.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                },
            }
        }

        if let Some(state) = &mut rtt {
            match state.stop.try_recv() {
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    if let Err(e) = poll_rtt(&mut session, state) {
                        // A read fault ends the capture. Record it where the
                        // owner can see it (fail-closed) rather than injecting
                        // it into the line stream as if it were device output:
                        // consumers must be able to tell lost observation from
                        // a healthy quiet channel.
                        state.health.fault(format!("rtt read error: {e}"));
                        rtt = None;
                    }
                }
                // Stopped or capture handle dropped: stop polling.
                _ => rtt = None,
            }
        }
    }
}

fn reset_core(session: &mut Session) -> anyhow::Result<()> {
    let mut core = session
        .core(0)
        .map_err(|e| anyhow::anyhow!("core(0): {e}"))?;
    core.reset().map_err(|e| anyhow::anyhow!("reset: {e}"))?;
    Ok(())
}

fn attach_rtt(session: &mut Session) -> anyhow::Result<Rtt> {
    let mut core = session
        .core(0)
        .map_err(|e| anyhow::anyhow!("core(0): {e}"))?;
    let rtt = Rtt::attach_region(&mut core, &ScanRegion::Ram)
        .map_err(|e| anyhow::anyhow!("attaching RTT: {e}"))?;
    Ok(rtt)
}

fn poll_rtt(session: &mut Session, state: &mut RttState) -> anyhow::Result<()> {
    let mut core = session
        .core(0)
        .map_err(|e| anyhow::anyhow!("core(0): {e}"))?;
    let Some(channel) = state.rtt.up_channels().iter_mut().next() else {
        return Ok(());
    };
    let mut buf = [0u8; 1024];
    let n = channel
        .read(&mut core, &mut buf)
        .map_err(|e| anyhow::anyhow!("rtt read: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    state.pending.extend_from_slice(&buf[..n]);
    // Emit complete lines; keep the trailing partial for the next poll.
    while let Some(pos) = state.pending.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = state.pending.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line);
        (state.sink)(text.trim_end().to_owned());
    }
    // A target that emits without newlines must not grow `pending` without
    // bound: flush an oversized partial as its own line so host memory stays
    // capped regardless of DUT behaviour.
    if state.pending.len() > MAX_LINE {
        let line = std::mem::take(&mut state.pending);
        let text = String::from_utf8_lossy(&line);
        (state.sink)(text.trim_end().to_owned());
    }
    Ok(())
}
