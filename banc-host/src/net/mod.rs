//! Network transport for remote rigs: postcard-rpc nodes and the rig lease
//! over token-authenticated TCP.
//!
//! A rig daemon (built by the essai from these library pieces) listens on one
//! port; every connection starts with a `Hello` frame declaring a role:
//!
//! - [`Role::Node`]: the rest of the stream is postcard-rpc, same frames as
//!   USB — the daemon is a banc node that happens to live across the network.
//!   Hosts get an ordinary `HostClient` via [`connect_node`].
//! - [`Role::Lease`]: the rest of the stream is the [`lease`] protocol,
//!   arbitrating exclusive rig access between runners that may not share a
//!   filesystem (the flock in `Rig::acquire` cannot reach across machines).
//!
//! Framing is a 4-byte little-endian length prefix per frame, both
//! directions, capped at [`MAX_FRAME`]. The token is a shared secret; the
//! daemon should sit behind an authenticated tunnel or a firewalled port, the
//! token is the second factor, not the perimeter.

pub mod lease;

use postcard_rpc::header::VarSeqKind;
use postcard_rpc::host_client::{HostClient, WireRx, WireSpawn, WireTx};
use postcard_rpc::standard_icd::{WireError, ERROR_PATH};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Bumped on any breaking change to the handshake or framing.
pub const NET_VERSION: u8 = 0;

/// Upper bound on a single frame, both directions. Generous for RPC frames
/// and small chunked payloads; a peer exceeding it is broken or hostile.
pub const MAX_FRAME: usize = 256 * 1024;

/// What a connection wants to be after the handshake.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Node,
    Lease,
}

#[derive(Serialize, Deserialize, Debug)]
struct Hello {
    version: u8,
    role: Role,
    token: String,
}

#[derive(Serialize, Deserialize, Debug)]
enum HelloReply {
    Ok { rig_name: String },
    Denied,
}

// --- framing ---

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    debug_assert!(data.len() <= MAX_FRAME);
    w.write_all(&(data.len() as u32).to_le_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("frame of {len} bytes exceeds MAX_FRAME")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub fn write_frame_sync<W: Write>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    debug_assert!(data.len() <= MAX_FRAME);
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)?;
    w.flush()
}

pub fn read_frame_sync<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("frame of {len} bytes exceeds MAX_FRAME")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Length-then-content comparison without early exit on content, so a wrong
/// token costs the same as a right one.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// --- client side ---

async fn handshake_client(stream: &mut TcpStream, role: Role, token: &str) -> anyhow::Result<String> {
    let hello = Hello { version: NET_VERSION, role, token: token.to_owned() };
    write_frame(stream, &postcard::to_stdvec(&hello)?).await?;
    let reply: HelloReply = postcard::from_bytes(&read_frame(stream).await?)?;
    match reply {
        HelloReply::Ok { rig_name } => Ok(rig_name),
        HelloReply::Denied => anyhow::bail!("rig daemon denied the handshake (bad token?)"),
    }
}

/// Connect to a remote banc node and return the postcard-rpc client plus the
/// rig name the daemon reported, exactly as `Node::connect` does over USB.
/// Must run on a tokio runtime (the wire workers are spawned onto it).
pub async fn connect_node(
    addr: &str,
    token: &str,
) -> anyhow::Result<(HostClient<WireError>, String)> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to node at {addr}: {e}"))?;
    stream.set_nodelay(true)?;
    let rig_name = handshake_client(&mut stream, Role::Node, token)
        .await
        .map_err(|e| anyhow::anyhow!("node handshake with {addr}: {e}"))?;
    let (rx, tx) = stream.into_split();
    let client = HostClient::new_with_wire(
        TcpWireTx(tx),
        TcpWireRx(rx),
        TokioSpawn,
        VarSeqKind::Seq2,
        ERROR_PATH,
        8,
    );
    Ok((client, rig_name))
}

struct TcpWireTx(tokio::net::tcp::OwnedWriteHalf);

impl WireTx for TcpWireTx {
    type Error = std::io::Error;
    async fn send(&mut self, data: Vec<u8>) -> Result<(), Self::Error> {
        write_frame(&mut self.0, &data).await
    }
}

struct TcpWireRx(tokio::net::tcp::OwnedReadHalf);

impl WireRx for TcpWireRx {
    type Error = std::io::Error;
    async fn receive(&mut self) -> Result<Vec<u8>, Self::Error> {
        read_frame(&mut self.0).await
    }
}

struct TokioSpawn;

impl WireSpawn for TokioSpawn {
    fn spawn(&mut self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        tokio::spawn(fut);
    }
}

// --- server side (for rig daemons composing these pieces) ---

/// Run the server half of the handshake on a fresh connection. On success
/// the stream is positioned at the first post-handshake frame and the caller
/// dispatches on the role; on failure the peer got `Denied` and the
/// connection should be dropped.
pub async fn handshake_server(
    stream: &mut TcpStream,
    token: &str,
    rig_name: &str,
) -> anyhow::Result<Role> {
    stream.set_nodelay(true)?;
    let hello: Hello = postcard::from_bytes(&read_frame(stream).await?)?;
    if hello.version != NET_VERSION || !token_eq(&hello.token, token) {
        write_frame(stream, &postcard::to_stdvec(&HelloReply::Denied)?).await?;
        anyhow::bail!(
            "handshake denied: version {} (want {NET_VERSION}), token {}",
            hello.version,
            if token_eq(&hello.token, token) { "ok" } else { "mismatch" },
        );
    }
    let reply = HelloReply::Ok { rig_name: rig_name.to_owned() };
    write_frame(stream, &postcard::to_stdvec(&reply)?).await?;
    Ok(hello.role)
}

/// Sync client half of the handshake, for the lease client's std stream.
pub(crate) fn handshake_client_sync(
    stream: &mut std::net::TcpStream,
    role: Role,
    token: &str,
) -> anyhow::Result<()> {
    let hello = Hello { version: NET_VERSION, role, token: token.to_owned() };
    write_frame_sync(stream, &postcard::to_stdvec(&hello)?)?;
    let reply: HelloReply = postcard::from_bytes(&read_frame_sync(stream)?)?;
    match reply {
        HelloReply::Ok { .. } => Ok(()),
        HelloReply::Denied => anyhow::bail!("rig daemon denied the handshake (bad token?)"),
    }
}
