//! A banc node: anything speaking banc-icd over postcard-rpc, whether a
//! board on USB or a rig daemon across the network.
//!
//! `Node` wraps discovery (nusb enumeration filtered by the rig config, or
//! an authenticated TCP connect), the identify handshake, and access to the
//! underlying `HostClient`. Consumers with their own ICDs call `client()`
//! and use their endpoint types directly — banc never needs to know them.

use crate::config::AssistantConfig;
use banc_icd::node::Identity;
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
    pub async fn connect(cfg: &AssistantConfig, token: Option<&str>) -> anyhow::Result<Node> {
        if let Some(addr) = &cfg.addr {
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("node '{}' has addr but no token was resolved", cfg.name)
            })?;
            let client = crate::net::connect_node(addr, token).await?;
            return Self::identify(client, &cfg.name).await;
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
        Self::identify(client, &cfg.name).await
    }

    async fn identify(client: HostClient<WireError>, name: &str) -> anyhow::Result<Node> {
        let identity = client
            .send_resp::<IdentifyEndpoint>(&())
            .await
            .map_err(|e| anyhow::anyhow!("identify on node '{name}': {e:?}"))?;
        anyhow::ensure!(
            identity.protocol_version == PROTOCOL_VERSION,
            "node '{name}' speaks banc protocol v{}, host expects v{PROTOCOL_VERSION}",
            identity.protocol_version,
        );
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
