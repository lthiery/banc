//! Rig acquisition: locate the topology config, take the cross-process lock,
//! hand out fixtures. One `Rig` per process; access to the hardware is
//! exclusive for as long as it lives (the pattern is embedded-test-stand's
//! lock-owned-by-fixture, extended with a file lock because nextest runs one
//! process per test).

use crate::config::{AssistantConfig, RigConfig};
use crate::node::Node;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};
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
        let base_dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();

        let timeout = std::env::var("BANC_LOCK_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LOCK_TIMEOUT);

        let lock = if let Some(lease) = &config.rig.lease {
            let token = read_token(&lease.token_file, &base_dir)?;
            Exclusive::Lease(
                crate::net::lease::LeaseClient::acquire(
                    &lease.addr,
                    &token,
                    &holder_identity(),
                    timeout,
                )
                .map_err(|e| anyhow::anyhow!("acquiring rig lease: {e}"))?,
            )
        } else {
            Exclusive::Flock(RigLock::take(lock_path(&config, &base_dir), timeout)?)
        };

        Ok(Rig {
            config,
            base_dir,
            _lock: lock,
        })
    }

    /// Where test artifacts (evidence logs, captures the suite writes) go:
    /// `BANC_ARTIFACTS` or `target/banc-artifacts` next to the rig config.
    /// The runner persists evidence here; suites use it for their own file
    /// artifacts so everything from a run lands in one place.
    pub fn artifacts_dir(&self) -> PathBuf {
        std::env::var_os("BANC_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.base_dir.join("target").join("banc-artifacts"))
    }

    /// The token for the target's probe-rs remote server, when the rig config
    /// sets `target.token_file`. `Ok(None)` when no target or no token file.
    pub fn target_token(&self) -> anyhow::Result<Option<String>> {
        let Some(target) = &self.config.target else {
            return Ok(None);
        };
        target
            .token_file
            .as_ref()
            .map(|p| read_token(p, &self.base_dir))
            .transpose()
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

/// Who this process is when contending for the rig, for both the network
/// lease and the flock's holder note: `BANC_LEASE_HOLDER`, else host:pid.
pub fn holder_identity() -> String {
    std::env::var("BANC_LEASE_HOLDER").unwrap_or_else(|_| {
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "?".into());
        format!("{host}:{}", std::process::id())
    })
}

/// The advisory lock path for a flock-arbitrated rig: `rig.lock_file`
/// resolved against the config dir, defaulting to `target/banc.lock`.
pub fn lock_path(config: &RigConfig, base_dir: &Path) -> PathBuf {
    config
        .rig
        .lock_file
        .clone()
        .map(|p| if p.is_absolute() { p } else { base_dir.join(p) })
        .unwrap_or_else(|| base_dir.join("target").join("banc.lock"))
}

/// Read and trim a token file, resolving relative paths against the
/// rig-config directory (the same resolution every config token field gets).
pub fn read_token(path: &std::path::Path, base_dir: &std::path::Path) -> anyhow::Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let token = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading token file {}: {e}", path.display()))?;
    let token = token.trim().to_owned();
    // An empty token authenticates nothing; fail loudly rather than handshake
    // with a blank secret.
    anyhow::ensure!(!token.is_empty(), "token file {} is empty", path.display());
    Ok(token)
}

/// The holder note a lock-taker writes into the lock file: the flock itself
/// carries no identity, so without this a contender staring at a timeout
/// cannot tell a legitimate long run from a wedged process — and the
/// observed failure mode is deleting the lock file, which frees nothing (the
/// flock lives on the wedged holder's open inode) while letting a second
/// runner "acquire" a fresh one.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockHolder {
    pub holder: String,
    pub pid: u32,
    /// Unix seconds when the lock was taken.
    pub since: u64,
}

/// Read the holder note from a lock file, if one is legible. Advisory: the
/// note may be stale (the OS frees a dead holder's flock but nothing erases
/// its note), so it only means something while the flock is actually held.
pub fn read_lock_holder(path: &Path) -> Option<LockHolder> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a flock-arbitrated rig is currently held, without contending for
/// it (the probe lock is taken non-blocking and released immediately).
pub enum FlockStatus {
    Free,
    /// Held; the note is None when the holder left nothing legible.
    Held(Option<LockHolder>),
}

pub fn flock_status(path: &Path) -> anyhow::Result<FlockStatus> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        // Never created: nothing has ever locked this rig.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FlockStatus::Free),
        Err(e) => return Err(anyhow::anyhow!("opening rig lock {}: {e}", path.display())),
    };
    if file.try_lock_exclusive()? {
        let _ = FileExt::unlock(&file);
        Ok(FlockStatus::Free)
    } else {
        Ok(FlockStatus::Held(read_lock_holder(path)))
    }
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
        // Open without truncating: until the flock is ours, the file's
        // contents are the current holder's note, not ours to clobber.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("creating rig lock {}: {e}", path.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            if file.try_lock_exclusive()? {
                let note = LockHolder {
                    holder: holder_identity(),
                    pid: std::process::id(),
                    since: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                };
                // Best-effort: an unwritable note degrades to the old
                // anonymous lock, never to a failed acquisition.
                let _ = file.set_len(0);
                let _ = file.seek(std::io::SeekFrom::Start(0));
                if let Ok(json) = serde_json::to_string(&note) {
                    let _ = file.write_all(json.as_bytes());
                    let _ = file.flush();
                }
                return Ok(RigLock { file });
            }
            if Instant::now() >= deadline {
                let holder = match read_lock_holder(&path) {
                    Some(h) => format!("'{}' (pid {}, since unix {})", h.holder, h.pid, h.since),
                    None => "an unidentified process".to_owned(),
                };
                anyhow::bail!(
                    "rig lock {} held by {holder} for over {timeout:?}; if that pid is \
                     wedged, end it (killing the process releases the flock) — deleting \
                     the lock file frees nothing",
                    path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for RigLock {
    fn drop(&mut self) {
        // Erase our note before releasing so a freed lock does not advertise
        // a dead holder. A killed process skips this; readers must treat the
        // note as meaningful only while the flock is held.
        let _ = self.file.set_len(0);
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_second_taker_until_dropped() {
        let path = std::env::temp_dir().join(format!("banc-rig-lock-test-{}", std::process::id()));
        let held = RigLock::take(path.clone(), Duration::ZERO).unwrap();
        let contended = RigLock::take(path.clone(), Duration::ZERO);
        assert!(
            contended.is_err(),
            "second take must fail while lock is held"
        );
        drop(held);
        RigLock::take(path.clone(), Duration::ZERO).unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn contender_timeout_names_the_holder() {
        let path =
            std::env::temp_dir().join(format!("banc-rig-holder-test-{}", std::process::id()));
        let held = RigLock::take(path.clone(), Duration::ZERO).unwrap();
        let note = read_lock_holder(&path).expect("holder note written on acquisition");
        assert_eq!(note.pid, std::process::id());
        let err = match RigLock::take(path.clone(), Duration::ZERO) {
            Ok(_) => panic!("second take must fail while lock is held"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains(&note.holder),
            "timeout must name holder: {err}"
        );
        assert!(
            err.contains("deleting the lock file frees nothing"),
            "timeout must warn against rm: {err}"
        );
        drop(held);
        // The note dies with the hold; a freed lock must not advertise one.
        assert!(
            read_lock_holder(&path).is_none(),
            "note must be erased on release"
        );
        std::fs::remove_file(path).ok();
    }
}
