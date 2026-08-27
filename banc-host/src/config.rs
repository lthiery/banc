//! Rig topology configuration.
//!
//! A rig is described by a `banc-rig.toml`, located via the `BANC_RIG` env
//! var or by searching from the current directory upward. Its absence is the
//! signal that this machine has no hardware attached: suites self-skip.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "banc-rig.toml";
pub const ENV_VAR: &str = "BANC_RIG";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RigConfig {
    #[serde(default)]
    pub rig: RigMeta,
    pub target: Option<TargetConfig>,
    #[serde(default, rename = "assistant")]
    pub assistants: Vec<AssistantConfig>,
    #[serde(default, rename = "instrument")]
    pub instruments: Vec<InstrumentConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RigMeta {
    pub name: Option<String>,
    /// Advisory lock file serializing rig access across processes (nextest
    /// runs one process per test). Default: target/banc.lock next to the
    /// config file.
    pub lock_file: Option<PathBuf>,
    /// Network lease server on the rig daemon. When set, it replaces the
    /// flock entirely: runners on other machines contend for the same rig,
    /// so a local file cannot be the arbiter.
    pub lease: Option<LeaseConfig>,
}

/// Where and how to take the network rig lease.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfig {
    /// host:port of the rig daemon.
    pub addr: String,
    /// File holding the shared token; relative paths resolve from the
    /// rig-config directory.
    pub token_file: PathBuf,
}

/// The device under test, driven via probe-rs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// probe-rs target-database chip name, e.g. "STM32WL55JCIx".
    pub chip: String,
    /// Probe selector `VID:PID[:SERIAL]`. None: the only probe attached.
    pub probe: Option<String>,
    /// probe-rs remote server (`probe-rs serve`), e.g. "https://pi:3000".
    /// None: local USB probe via the probe-rs library.
    pub probe_host: Option<String>,
}

/// An assistant node speaking banc-icd over postcard-rpc, reached over USB
/// (serial/product match) or the network (addr + token).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantConfig {
    /// Name tests use to look the node up, e.g. "a0".
    pub name: String,
    /// USB serial string (assistants surface their unique ID here).
    pub serial: Option<String>,
    /// USB product string to match when serial is not given.
    pub product: Option<String>,
    /// host:port of a network node (a rig daemon). Mutually exclusive with
    /// the USB fields.
    pub addr: Option<String>,
    /// Token file for the network handshake; relative paths resolve from
    /// the rig-config directory.
    pub token_file: Option<PathBuf>,
    /// Expected device unique id as a 16-hex-digit string (the same value the
    /// assistant reports in `Identity` and surfaces as its USB serial). When
    /// set, a connected node whose id differs is rejected: this is how a
    /// network node gets the identity check that USB gets for free via
    /// `serial`. Case-insensitive.
    pub unique_id: Option<String>,
}

impl AssistantConfig {
    /// True when this entry describes a network node (addr set) rather than a
    /// USB one.
    pub fn is_network(&self) -> bool {
        self.addr.is_some()
    }
}

/// A bench instrument. Drivers are matched on `kind` by the suite.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentConfig {
    pub name: String,
    /// Driver key, e.g. "rcdat", "scpi".
    pub kind: String,
    /// Free-form address: host:port, VISA resource, hidraw path...
    pub address: Option<String>,
    /// Driver-specific settings, passed through untouched.
    #[serde(default)]
    pub params: toml::Table,
}

impl RigConfig {
    /// Find the rig config for this machine/checkout. `Ok(None)` means "no
    /// rig here" (the self-skip signal); `Err` means a config exists but is
    /// unusable, which is a real failure, not a skip.
    pub fn locate() -> anyhow::Result<Option<PathBuf>> {
        if let Ok(path) = std::env::var(ENV_VAR) {
            let path = PathBuf::from(path);
            anyhow::ensure!(
                path.is_file(),
                "{ENV_VAR} points at {} which does not exist",
                path.display()
            );
            return Ok(Some(path));
        }
        let mut dir = std::env::current_dir()?;
        loop {
            let candidate = dir.join(CONFIG_FILE);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
            if !dir.pop() {
                return Ok(None);
            }
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: RigConfig = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid rig config {}: {e}", path.display()))?;
        Ok(config)
    }

    /// Reject configs that parse but describe nothing runnable, so a typo
    /// fails at load with a clear message instead of a confusing runtime
    /// error (or, worse, silently matching the wrong device).
    pub fn validate(&self) -> anyhow::Result<()> {
        // Names are how tests look nodes up; duplicates make `assistant(name)`
        // and `instrument(name)` ambiguous (first-wins is a silent footgun).
        let mut seen = std::collections::HashSet::new();
        for a in &self.assistants {
            anyhow::ensure!(!a.name.trim().is_empty(), "assistant with an empty name");
            anyhow::ensure!(
                seen.insert(("assistant", a.name.as_str())),
                "duplicate assistant name '{}'",
                a.name
            );
        }
        seen.clear();
        for i in &self.instruments {
            anyhow::ensure!(!i.name.trim().is_empty(), "instrument with an empty name");
            anyhow::ensure!(
                seen.insert(("instrument", i.name.as_str())),
                "duplicate instrument name '{}'",
                i.name
            );
        }

        for a in &self.assistants {
            if a.is_network() {
                // Network node: USB match fields are meaningless and a token is
                // mandatory (the handshake cannot proceed without it).
                anyhow::ensure!(
                    a.serial.is_none() && a.product.is_none(),
                    "assistant '{}' sets addr and USB serial/product; pick one transport",
                    a.name
                );
                anyhow::ensure!(
                    a.token_file.is_some(),
                    "network assistant '{}' has addr but no token_file",
                    a.name
                );
                anyhow::ensure!(
                    !a.addr.as_deref().unwrap_or("").trim().is_empty(),
                    "assistant '{}' has an empty addr",
                    a.name
                );
            } else {
                // USB node: at least one of serial/product must constrain the
                // match, otherwise the predicate matches ANY attached device.
                anyhow::ensure!(
                    a.serial.is_some() || a.product.is_some(),
                    "USB assistant '{}' has neither serial nor product; \
                     that matches any attached device",
                    a.name
                );
                anyhow::ensure!(
                    a.token_file.is_none(),
                    "USB assistant '{}' has a token_file but no addr to use it with",
                    a.name
                );
            }
            if let Some(id) = &a.unique_id {
                anyhow::ensure!(
                    id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit()),
                    "assistant '{}' unique_id '{}' must be 16 hex digits",
                    a.name,
                    id
                );
            }
        }

        if let Some(lease) = &self.rig.lease {
            anyhow::ensure!(!lease.addr.trim().is_empty(), "rig lease has an empty addr");
        }
        Ok(())
    }

    pub fn assistant(&self, name: &str) -> Option<&AssistantConfig> {
        self.assistants.iter().find(|a| a.name == name)
    }

    pub fn instrument(&self, name: &str) -> Option<&InstrumentConfig> {
        self.instruments.iter().find(|i| i.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: RigConfig = toml::from_str(
            r#"
            [rig]
            name = "bench-1"

            [target]
            chip = "RP2350"
            probe = "2e8a:000c"

            [[assistant]]
            name = "a0"
            serial = "0123456789ABCDEF"

            [[instrument]]
            name = "att0"
            kind = "rcdat"
            address = "192.168.1.50:23"
            params = { max_db = 90.0 }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.rig.name.as_deref(), Some("bench-1"));
        assert_eq!(cfg.target.as_ref().unwrap().chip, "RP2350");
        assert_eq!(cfg.assistant("a0").unwrap().serial.as_deref(), Some("0123456789ABCDEF"));
        assert_eq!(cfg.instrument("att0").unwrap().kind, "rcdat");
        assert!(cfg.assistant("nope").is_none());
    }

    #[test]
    fn empty_config_is_valid() {
        let cfg: RigConfig = toml::from_str("").unwrap();
        assert!(cfg.target.is_none());
        assert!(cfg.assistants.is_empty());
        cfg.validate().unwrap();
    }

    fn parse(s: &str) -> RigConfig {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn usb_assistant_without_serial_or_product_is_rejected() {
        let err = parse("[[assistant]]\nname = \"a0\"\n").validate().unwrap_err();
        assert!(err.to_string().contains("any attached device"), "{err}");
    }

    #[test]
    fn network_and_usb_fields_conflict() {
        let err = parse(
            "[[assistant]]\nname = \"a0\"\naddr = \"rig:9000\"\n\
             token_file = \"t\"\nserial = \"ABC\"\n",
        )
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("pick one transport"), "{err}");
    }

    #[test]
    fn network_assistant_needs_a_token() {
        let err = parse("[[assistant]]\nname = \"a0\"\naddr = \"rig:9000\"\n")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("no token_file"), "{err}");
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let err = parse(
            "[[assistant]]\nname = \"a0\"\nserial = \"A\"\n\
             [[assistant]]\nname = \"a0\"\nserial = \"B\"\n",
        )
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("duplicate assistant name"), "{err}");
    }

    #[test]
    fn bad_unique_id_is_rejected() {
        let err = parse("[[assistant]]\nname = \"a0\"\naddr = \"rig:9000\"\ntoken_file = \"t\"\nunique_id = \"xyz\"\n")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("16 hex digits"), "{err}");
    }

    #[test]
    fn well_formed_network_assistant_validates() {
        parse(
            "[[assistant]]\nname = \"a0\"\naddr = \"rig:9000\"\n\
             token_file = \"t\"\nunique_id = \"0123456789ABCDEF\"\n",
        )
        .validate()
        .unwrap();
    }
}
