//! End-to-end exercise of the network transport against an in-process mini
//! rig daemon: token handshake, postcard-rpc identify over TCP, and the
//! lease protocol including contention.

use banc_host::net::lease::{LeaseClient, LeaseServer};
use banc_host::net::{self, Role};
use banc_icd::node::{Identity, NodeRole};
use banc_icd::{IdentifyEndpoint, PROTOCOL_VERSION};
use postcard_rpc::header::{VarHeader, VarKey};
use postcard_rpc::Endpoint;
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "test-token";

/// Minimal daemon: one listener, handshake per connection, lease or a
/// node that only answers identify. This is the same composition a real
/// rig daemon makes out of these pieces.
async fn spawn_daemon() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let lease = Arc::new(LeaseServer::new());
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => return,
            };
            let lease = lease.clone();
            tokio::spawn(async move {
                let role = match net::handshake_server(&mut stream, TOKEN, "test-rig").await {
                    Ok(role) => role,
                    Err(_) => return,
                };
                match role {
                    Role::Lease => {
                        let _ = lease.serve_conn(&mut stream).await;
                    }
                    Role::Node => {
                        let _ = serve_identify(&mut stream).await;
                    }
                }
            });
        }
    });
    addr
}

async fn serve_identify(stream: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
    loop {
        let frame = net::read_frame(stream).await?;
        let Some((hdr, _body)) = VarHeader::take_from_slice(&frame) else {
            anyhow::bail!("unparseable frame header");
        };
        // VarKey's PartialEq degrades to the narrower width, so this matches
        // whatever key size the client is using.
        anyhow::ensure!(
            hdr.key == VarKey::Key8(IdentifyEndpoint::REQ_KEY),
            "unexpected endpoint key: {:?}",
            hdr.key
        );
        let identity = Identity {
            role: NodeRole::Custom,
            protocol_version: PROTOCOL_VERSION,
            unique_id: 42,
            fw_name: "rigd-test".try_into().unwrap(),
            fw_version: "0".try_into().unwrap(),
        };
        let resp_hdr = VarHeader {
            key: VarKey::Key8(IdentifyEndpoint::RESP_KEY),
            seq_no: hdr.seq_no,
        };
        let mut out = resp_hdr.write_to_vec();
        out.extend_from_slice(&postcard::to_stdvec(&identity)?);
        net::write_frame(stream, &out).await?;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn identify_over_tcp() {
    let addr = spawn_daemon().await;
    let client = net::connect_node(&addr.to_string(), TOKEN).await.unwrap();
    let identity = client.send_resp::<IdentifyEndpoint>(&()).await.unwrap();
    assert_eq!(identity.protocol_version, PROTOCOL_VERSION);
    assert_eq!(identity.unique_id, 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_token_is_denied() {
    let addr = spawn_daemon().await;
    let err = net::connect_node(&addr.to_string(), "wrong").await;
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_contention_over_tcp() {
    let addr = spawn_daemon().await;
    let addr_s = addr.to_string();

    let first = {
        let addr_s = addr_s.clone();
        tokio::task::spawn_blocking(move || {
            LeaseClient::acquire(&addr_s, TOKEN, "first", Duration::from_secs(5)).unwrap()
        })
        .await
        .unwrap()
    };

    // Second holder cannot get it while the first lives.
    let contended = {
        let addr_s = addr_s.clone();
        tokio::task::spawn_blocking(move || {
            LeaseClient::acquire(&addr_s, TOKEN, "second", Duration::from_millis(100))
        })
        .await
        .unwrap()
    };
    assert!(contended.is_err(), "second acquire must fail while first holds the lease");

    // Dropping the first (release on drop) frees it.
    tokio::task::spawn_blocking(move || drop(first)).await.unwrap();
    let after = tokio::task::spawn_blocking(move || {
        LeaseClient::acquire(&addr_s, TOKEN, "second", Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert!(after.is_ok(), "acquire after release must succeed: {:?}", after.err());
}
