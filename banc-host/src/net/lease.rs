//! Network-backed rig lease: exclusive rig access arbitrated by the rig
//! daemon instead of a local flock, for rigs whose runners do not share a
//! filesystem.
//!
//! Semantics mirror the flock: one holder at a time, held for the life of
//! the `Rig`. The TTL replaces the OS releasing a dead process's lock: a
//! holder that stops renewing (crashed runner, dead CI job) loses the lease
//! after `ttl` and the rig frees itself.
//!
//! The client is deliberately synchronous (`Rig::acquire` must not create
//! runtime-bound resources); renewal runs on a plain thread owned by the
//! [`LeaseClient`], which releases on drop.

use super::{handshake_client_sync, read_frame_sync, write_frame_sync, Role};
use serde::{Deserialize, Serialize};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default lease TTL. Renewals go out every `ttl / 3`, so a couple of missed
/// renewals (GC pause, WAN hiccup) do not lose the rig.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum LeaseRequest {
    Acquire { holder: String, ttl_s: u32 },
    Renew { id: u64 },
    Release { id: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum LeaseReply {
    Granted { id: u64 },
    /// Someone else holds the lease; retry after backoff.
    Busy { holder: String, expires_in_s: u32 },
    /// Renew/Release acknowledged.
    Ok,
    /// The lease id is not current (expired and possibly re-granted).
    Gone,
}

fn roundtrip(stream: &mut TcpStream, req: &LeaseRequest) -> anyhow::Result<LeaseReply> {
    write_frame_sync(stream, &postcard::to_stdvec(req)?)?;
    Ok(postcard::from_bytes(&read_frame_sync(stream)?)?)
}

fn connect(addr: &str, token: &str) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect(addr)
        .map_err(|e| anyhow::anyhow!("connecting to lease server at {addr}: {e}"))?;
    stream.set_nodelay(true)?;
    handshake_client_sync(&mut stream, Role::Lease, token)?;
    Ok(stream)
}

/// A held rig lease. Renewed in the background; released on drop.
pub struct LeaseClient {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LeaseClient {
    /// Acquire the lease, retrying while it is busy until `timeout`.
    pub fn acquire(
        addr: &str,
        token: &str,
        holder: &str,
        timeout: Duration,
    ) -> anyhow::Result<LeaseClient> {
        let ttl = DEFAULT_TTL;
        let deadline = Instant::now() + timeout;
        let mut stream = connect(addr, token)?;
        let id = loop {
            let req = LeaseRequest::Acquire {
                holder: holder.to_owned(),
                ttl_s: ttl.as_secs() as u32,
            };
            match roundtrip(&mut stream, &req)? {
                LeaseReply::Granted { id } => break id,
                LeaseReply::Busy { holder: other, expires_in_s } => {
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "rig lease held by '{other}' (expires in {expires_in_s}s), \
                             gave up after {timeout:?}"
                        );
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                other => anyhow::bail!("unexpected reply to Acquire: {other:?}"),
            }
        };

        let (stop, stopped) = mpsc::channel::<()>();
        let (addr, token, holder) = (addr.to_owned(), token.to_owned(), holder.to_owned());
        let thread = std::thread::Builder::new()
            .name("banc-lease-renew".into())
            .spawn(move || renew_loop(stream, id, ttl, &addr, &token, &holder, stopped))?;

        Ok(LeaseClient { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for LeaseClient {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Renew every ttl/3 until the stop channel closes, then release. A broken
/// connection is reconnected and the lease re-acquired.
///
/// Losing the lease to another holder mid-run is currently only reported
/// here, not enforced: node connections are separate authenticated sessions
/// that carry no lease id, so the daemon cannot refuse traffic from a
/// displaced holder, and two runners can both believe they hold the rig after
/// a partition or long pause. Binding hardware sessions to the active lease
/// (so exclusivity is provable, not advisory) is tracked as follow-up; until
/// then this is fail-open and the eprintln is the only signal.
fn renew_loop(
    mut stream: TcpStream,
    mut id: u64,
    ttl: Duration,
    addr: &str,
    token: &str,
    holder: &str,
    stopped: mpsc::Receiver<()>,
) {
    let period = ttl / 3;
    loop {
        match stopped.recv_timeout(period) {
            // Sender dropped: the Rig is going away, release and exit.
            Err(mpsc::RecvTimeoutError::Disconnected) | Ok(()) => {
                let _ = roundtrip(&mut stream, &LeaseRequest::Release { id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let outcome = roundtrip(&mut stream, &LeaseRequest::Renew { id });
        match outcome {
            Ok(LeaseReply::Ok) => {}
            Ok(LeaseReply::Gone) => {
                eprintln!("banc: rig lease expired mid-run; re-acquiring");
                match reacquire(addr, token, holder, ttl) {
                    Ok((s, new_id)) => {
                        stream = s;
                        id = new_id;
                    }
                    Err(e) => eprintln!("banc: rig lease lost and re-acquire failed: {e}"),
                }
            }
            Ok(other) => eprintln!("banc: unexpected reply to Renew: {other:?}"),
            Err(e) => {
                eprintln!("banc: lease renew failed ({e}); reconnecting");
                match reacquire(addr, token, holder, ttl) {
                    Ok((s, new_id)) => {
                        stream = s;
                        id = new_id;
                    }
                    Err(e) => eprintln!("banc: lease reconnect failed: {e}"),
                }
            }
        }
    }
}

fn reacquire(
    addr: &str,
    token: &str,
    holder: &str,
    ttl: Duration,
) -> anyhow::Result<(TcpStream, u64)> {
    let mut stream = connect(addr, token)?;
    let req = LeaseRequest::Acquire { holder: holder.to_owned(), ttl_s: ttl.as_secs() as u32 };
    match roundtrip(&mut stream, &req)? {
        LeaseReply::Granted { id } => Ok((stream, id)),
        LeaseReply::Busy { holder: other, .. } => {
            anyhow::bail!("lease now held by '{other}'")
        }
        other => anyhow::bail!("unexpected reply to Acquire: {other:?}"),
    }
}

// --- server side ---

/// Single-slot lease state for a rig daemon. Expiry is evaluated lazily on
/// each request; a holder that stops renewing is displaced by the next
/// Acquire after its TTL passes.
pub struct LeaseServer {
    state: std::sync::Mutex<Option<Held>>,
    next_id: std::sync::atomic::AtomicU64,
}

struct Held {
    id: u64,
    holder: String,
    expires_at: Instant,
    ttl: Duration,
}

impl Default for LeaseServer {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseServer {
    pub fn new() -> Self {
        LeaseServer {
            state: std::sync::Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Who currently holds the lease, if anyone.
    pub fn holder(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        state
            .as_ref()
            .filter(|h| h.expires_at > Instant::now())
            .map(|h| h.holder.clone())
    }

    pub fn handle(&self, req: LeaseRequest) -> LeaseReply {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if state.as_ref().is_some_and(|h| h.expires_at <= now) {
            *state = None;
        }
        match req {
            LeaseRequest::Acquire { holder, ttl_s } => match &*state {
                Some(held) => LeaseReply::Busy {
                    holder: held.holder.clone(),
                    expires_in_s: held.expires_at.saturating_duration_since(now).as_secs() as u32,
                },
                None => {
                    let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let ttl = Duration::from_secs(ttl_s.clamp(5, 3600) as u64);
                    *state = Some(Held { id, holder, expires_at: now + ttl, ttl });
                    LeaseReply::Granted { id }
                }
            },
            LeaseRequest::Renew { id } => match state.as_mut() {
                Some(held) if held.id == id => {
                    held.expires_at = now + held.ttl;
                    LeaseReply::Ok
                }
                _ => LeaseReply::Gone,
            },
            LeaseRequest::Release { id } => {
                if state.as_ref().is_some_and(|h| h.id == id) {
                    *state = None;
                }
                LeaseReply::Ok
            }
        }
    }

    /// Serve the lease protocol on one authenticated connection until the
    /// peer disconnects.
    pub async fn serve_conn(&self, stream: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
        loop {
            let frame = match super::read_frame(stream).await {
                Ok(f) => f,
                // Peer hung up: normal end of a lease session.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let req: LeaseRequest = postcard::from_bytes(&frame)?;
            let reply = self.handle(req);
            super::write_frame(stream, &postcard::to_stdvec(&reply)?).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_excludes_second_holder_until_released() {
        let srv = LeaseServer::new();
        let LeaseReply::Granted { id } =
            srv.handle(LeaseRequest::Acquire { holder: "a".into(), ttl_s: 60 })
        else {
            panic!("first acquire must be granted");
        };
        assert!(matches!(
            srv.handle(LeaseRequest::Acquire { holder: "b".into(), ttl_s: 60 }),
            LeaseReply::Busy { .. }
        ));
        assert!(matches!(srv.handle(LeaseRequest::Renew { id }), LeaseReply::Ok));
        srv.handle(LeaseRequest::Release { id });
        assert!(matches!(
            srv.handle(LeaseRequest::Acquire { holder: "b".into(), ttl_s: 60 }),
            LeaseReply::Granted { .. }
        ));
    }

    #[test]
    fn expired_lease_is_displaced_and_stale_renew_refused() {
        let srv = LeaseServer::new();
        let LeaseReply::Granted { id: stale } =
            srv.handle(LeaseRequest::Acquire { holder: "a".into(), ttl_s: 5 })
        else {
            panic!("first acquire must be granted");
        };
        // Force expiry rather than sleeping: rewind the deadline.
        srv.state.lock().unwrap().as_mut().unwrap().expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(matches!(
            srv.handle(LeaseRequest::Acquire { holder: "b".into(), ttl_s: 60 }),
            LeaseReply::Granted { .. }
        ));
        assert!(matches!(srv.handle(LeaseRequest::Renew { id: stale }), LeaseReply::Gone));
    }
}
