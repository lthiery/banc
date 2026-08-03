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
}

/// The device under test, driven via probe-rs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// probe-rs target-database chip name, e.g. "STM32WL55JCIx".
    pub chip: String,
    /// Probe selector "VID:PID[:SERIAL]". None: the only probe attached.
    pub probe: Option<String>,
    /// probe-rs remote server (`probe-rs serve`), e.g. "https://pi:3000".
    /// None: local USB probe via the probe-rs library.
    pub probe_host: Option<String>,
}

/// An assistant node speaking banc-icd over postcard-rpc USB.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantConfig {
    /// Name tests use to look the node up, e.g. "a0".
    pub name: String,
    /// USB serial string (assistants surface their unique ID here).
    pub serial: Option<String>,
    /// USB product string to match when serial is not given.
    pub product: Option<String>,
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
        Ok(config)
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
    }
}
