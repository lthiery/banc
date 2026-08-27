//! A banc node: anything speaking banc-icd over postcard-rpc, whether a
//! board on USB or a rig daemon across the network.
//!
//! `Node` wraps discovery (nusb enumeration filtered by the rig config, or
//! an authenticated TCP connect), the identify handshake, and access to the
//! underlying `HostClient`. Consumers with their own ICDs call `client()`
//! and use their endpoint types directly — banc never needs to know them.

use crate::config::AssistantConfig;
use banc_icd::node::{Identity, NodeRole};
use banc_icd::{IdentifyEndpoint, PROTOCOL_VERSION};
use postcard_rpc::header::VarSeqKind;
use postcard_rpc::host_client::HostClient;
use postcard_rpc::standard_icd::{WireError, ERROR_PATH};

pub struct Node {
    client: HostClient<WireError>,
    pub identity: Identity,
}

impl Node {
    /// Connect per the config entry: network when `addr` is set, USB
    /// enumeration otherwise. `token` must be pre-resolved by the caller
    /// (the rig knows the config's base directory; this module does not).
    /// `expected_rig`, when given, is the rig name the caller expects the
    /// daemon to report (from the rig config): a mismatch means we reached
    /// the wrong bench and is a hard error.
    pub async fn connect(
        cfg: &AssistantConfig,
        token: Option<&str>,
        expected_rig: Option<&str>,
    ) -> anyhow::Result<Node> {
        if let Some(addr) = &cfg.addr {
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("node '{}' has addr but no token was resolved", cfg.name)
            })?;
            let (client, rig_name) = crate::net::connect_node(addr, token).await?;
            // Catch a typo/DNS/reused-token that landed us on another rig
            // before we run a single test against the wrong hardware.
            if let Some(want) = expected_rig {
                anyhow::ensure!(
                    rig_name == want,
                    "node '{}' at {addr} belongs to rig '{rig_name}', expected '{want}'",
                    cfg.name,
                );
            }
            return Self::identify(client, cfg).await;
        }
        let serial = cfg.serial.clone();
        let product = cfg.product.clone();
        let name = cfg.name.clone();
        let client = HostClient::try_new_raw_nusb(
            move |d| {
                let serial_ok = serial
                    .as_deref()
                    .is_none_or(|want| d.serial_number() == Some(want));
                let product_ok = product
                    .as_deref()
                    .is_none_or(|want| d.product_string() == Some(want));
                serial_ok && product_ok
            },
            ERROR_PATH,
            8,
            VarSeqKind::Seq2,
        )
        .map_err(|e| anyhow::anyhow!("connecting to node '{name}': {e}"))?;
        Self::identify(client, cfg).await
    }

    async fn identify(client: HostClient<WireError>, cfg: &AssistantConfig) -> anyhow::Result<Node> {
        let name = &cfg.name;
        let identity = client
            .send_resp::<IdentifyEndpoint>(&())
            .await
            .map_err(|e| anyhow::anyhow!("identify on node '{name}': {e:?}"))?;
        anyhow::ensure!(
            identity.protocol_version == PROTOCOL_VERSION,
            "node '{name}' speaks banc protocol v{}, host expects v{PROTOCOL_VERSION}",
            identity.protocol_version,
        );
        // Pin to a specific device when the config asks. USB gets this via the
        // serial match; a network node has no such filter, so this is its only
        // device-identity check.
        if let Some(want) = &cfg.unique_id {
            let got = format!("{:016X}", identity.unique_id);
            anyhow::ensure!(
                got.eq_ignore_ascii_case(want),
                "node '{name}' reports unique id {got}, config expects {want}",
            );
        }
        // A node configured as an assistant that reports a custom role is a
        // wiring mistake worth flagging, but consumers do run custom-ICD nodes
        // through this path, so warn rather than fail.
        if identity.role != NodeRole::Assistant {
            eprintln!(
                "banc: node '{name}' reports role {:?}, not Assistant (check the rig config)",
                identity.role
            );
        }
        Ok(Node { client, identity })
    }

    /// The raw postcard-rpc client, for consumer-defined endpoints/topics.
    pub fn client(&self) -> &HostClient<WireError> {
        &self.client
    }

    /// Ask the node to reset itself (firmware replies first, then reboots).
    pub async fn reset(&self) -> anyhow::Result<()> {
        self.client
            .send_resp::<banc_icd::ResetEndpoint>(&())
            .await
            .map_err(|e| anyhow::anyhow!("reset: {e:?}"))?;
        Ok(())
    }
}
