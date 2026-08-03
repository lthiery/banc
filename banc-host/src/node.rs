//! A banc node: any board speaking banc-icd over postcard-rpc USB.
//!
//! `Node` wraps discovery (nusb enumeration filtered by the rig config),
//! the identify handshake, and access to the underlying `HostClient`.
//! Consumers with their own ICDs call `client()` and use their endpoint
//! types directly — banc never needs to know them.

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
    /// Enumerate USB, match this config entry, connect, and identify.
    pub async fn connect(cfg: &AssistantConfig) -> anyhow::Result<Node> {
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

        let identity = client
            .send_resp::<IdentifyEndpoint>(&())
            .await
            .map_err(|e| anyhow::anyhow!("identify on node '{}': {e:?}", cfg.name))?;
        anyhow::ensure!(
            identity.protocol_version == PROTOCOL_VERSION,
            "node '{}' speaks banc protocol v{}, host expects v{PROTOCOL_VERSION}",
            cfg.name,
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
