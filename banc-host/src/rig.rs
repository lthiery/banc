//! Rig acquisition: locate the topology config, take the cross-process lock,
//! hand out fixtures. One `Rig` per process; access to the hardware is
//! exclusive for as long as it lives (the pattern is embedded-test-stand's
//! lock-owned-by-fixture, extended with a file lock because nextest runs one
//! process per test).

use crate::config::{AssistantConfig, RigConfig};
use crate::node::Node;
use fs4::fs_std::FileExt;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// Acquisition is deliberately synchronous: the `Rig` outlives every
// per-trial runtime, so nothing created here may be tied to one. Keeping
// async out of this path means a runtime-bound resource (socket, client,
// spawned task) cannot be added to `Rig` without changing this signature —
// connections belong to per-test fixtures, created on the trial's runtime.

/// How long a process waits for another test process to release the rig
/// before giving up. Override with BANC_LOCK_TIMEOUT_SECS.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(300);

/// Why `Rig::acquire` did not return a rig.
pub enum Acquire {
    /// No rig on this machine — report the test as ignored, with this reason.
    Skip(String),
    /// A rig is configured but broken/contended — report a failure.
    Fail(anyhow::Error),
}

impl From<anyhow::Error> for Acquire {
    fn from(e: anyhow::Error) -> Self {
        Acquire::Fail(e)
    }
}

pub struct Rig {
    pub config: RigConfig,
    /// Directory the config file lives in; relative paths resolve from here.
    pub base_dir: PathBuf,
    _lock: Exclusive,
}

/// How this process's exclusive hold on the rig is enforced: an flock for
/// single-machine rigs, a network lease when the rig config names a lease
/// server (runners on other machines cannot see our lock file).
enum Exclusive {
    // Both variants exist only for their Drop (release on Rig teardown).
    Flock(#[allow(dead_code)] RigLock),
    Lease(#[allow(dead_code)] crate::net::lease::LeaseClient),
}

impl Rig {
    /// Locate + parse the config and take the cross-process lock.
    ///
    /// Missing config => `Acquire::Skip` (honest self-skip). Malformed
    /// config or lock timeout => `Acquire::Fail` (a rig machine that cannot
    /// run its suite is a failure, not a skip).
    pub fn acquire() -> Result<Rig, Acquire> {
        let Some(path) = RigConfig::locate()? else {
            return Err(Acquire::Skip(format!(
                "no rig: {} not found (set {} or create one to run on hardware)",
                crate::config::CONFIG_FILE,
                crate::config::ENV_VAR,
            )));
        };
        let config = RigConfig::load(&path)?;
        let base_dir = path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();

        let timeout = std::env::var("BANC_LOCK_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LOCK_TIMEOUT);

        let lock = if let Some(lease) = &config.rig.lease {
            let token = read_token(&lease.token_file, &base_dir)?;
            let holder = std::env::var("BANC_LEASE_HOLDER").unwrap_or_else(|_| {
                let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "?".into());
                format!("{host}:{}", std::process::id())
            });
            Exclusive::Lease(
                crate::net::lease::LeaseClient::acquire(&lease.addr, &token, &holder, timeout)
                    .map_err(|e| anyhow::anyhow!("acquiring rig lease: {e}"))?,
            )
        } else {
            let lock_path = config
                .rig
                .lock_file
                .clone()
                .map(|p| if p.is_absolute() { p } else { base_dir.join(p) })
                .unwrap_or_else(|| base_dir.join("target").join("banc.lock"));
            Exclusive::Flock(RigLock::take(lock_path, timeout)?)
        };

        Ok(Rig { config, base_dir, _lock: lock })
    }

    /// Connect to a configured assistant by name.
    pub async fn assistant(&self, name: &str) -> anyhow::Result<Node> {
        let cfg: &AssistantConfig = self
            .config
            .assistant(name)
            .ok_or_else(|| anyhow::anyhow!("no assistant '{name}' in rig config"))?;
        let token = cfg
            .token_file
            .as_ref()
            .map(|p| read_token(p, &self.base_dir))
            .transpose()?;
        Node::connect(cfg, token.as_deref(), self.config.rig.name.as_deref()).await
    }
}

fn read_token(path: &std::path::Path, base_dir: &std::path::Path) -> anyhow::Result<String> {
    let path = if path.is_absolute() { path.to_path_buf() } else { base_dir.join(path) };
    let token = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading token file {}: {e}", path.display()))?;
    let token = token.trim().to_owned();
    // An empty token authenticates nothing; fail loudly rather than handshake
    // with a blank secret.
    anyhow::ensure!(!token.is_empty(), "token file {} is empty", path.display());
    Ok(token)
}

/// Advisory file lock serializing rig access across processes. Held for the
/// life of the `Rig`; a poisoned/stale holder is handled by the OS releasing
/// the lock when that process dies.
struct RigLock {
    file: File,
}

impl RigLock {
    fn take(path: PathBuf, timeout: Duration) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(&path)
            .map_err(|e| anyhow::anyhow!("creating rig lock {}: {e}", path.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            if file.try_lock_exclusive()? {
                return Ok(RigLock { file });
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "rig lock {} held by another process for over {timeout:?}",
                    path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for RigLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_second_taker_until_dropped() {
        let path = std::env::temp_dir()
            .join(format!("banc-rig-lock-test-{}", std::process::id()));
        let held = RigLock::take(path.clone(), Duration::ZERO).unwrap();
        let contended = RigLock::take(path.clone(), Duration::ZERO);
        assert!(contended.is_err(), "second take must fail while lock is held");
        drop(held);
        RigLock::take(path.clone(), Duration::ZERO).unwrap();
        std::fs::remove_file(path).ok();
    }
}
