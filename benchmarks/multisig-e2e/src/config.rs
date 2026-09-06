use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use miden_multisig_client::Endpoint;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RunConfig {
    pub accounts_file: PathBuf,
    pub faucet_id: String,
    #[serde(default = "default_operations")]
    pub operations: u64,
    #[serde(default = "default_amount")]
    pub amount: u64,
    #[serde(default = "default_consume_probability")]
    pub consume_probability: f64,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_proposal_retry_interval_ms")]
    pub proposal_retry_interval_ms: u64,
    #[serde(default = "default_proposal_retry_timeout_seconds")]
    pub proposal_retry_timeout_seconds: u64,
    #[serde(default)]
    pub max_duration_seconds: Option<u64>,
    #[serde(default = "default_artifacts_dir")]
    pub artifacts_dir: PathBuf,
}

impl RunConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read benchmark config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse benchmark config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.operations == 0 {
            bail!("operations must be greater than zero");
        }
        if self.amount == 0 {
            bail!("amount must be greater than zero");
        }
        if !(0.0..=1.0).contains(&self.consume_probability) {
            bail!("consume_probability must be between 0 and 1");
        }
        if self.poll_interval_ms == 0 || self.timeout_seconds == 0 {
            bail!("poll_interval_ms and timeout_seconds must be greater than zero");
        }
        if self.proposal_retry_interval_ms == 0 || self.proposal_retry_timeout_seconds == 0 {
            bail!(
                "proposal_retry_interval_ms and proposal_retry_timeout_seconds must be greater than zero"
            );
        }
        if self.max_duration_seconds == Some(0) {
            bail!("max_duration_seconds must be greater than zero when set");
        }
        Ok(())
    }
}

pub fn parse_miden_endpoint(input: &str) -> Result<Endpoint> {
    let (protocol, authority) = input
        .split_once("://")
        .ok_or_else(|| anyhow!("Miden endpoint must start with http:// or https://"))?;
    if protocol != "http" && protocol != "https" {
        bail!("unsupported Miden endpoint protocol '{protocol}'");
    }
    if authority.is_empty() || authority.contains('/') {
        bail!("Miden endpoint must contain only a host and optional port");
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port
                .parse::<u16>()
                .with_context(|| format!("invalid Miden endpoint port '{port}'"))?;
            (host.to_string(), Some(port))
        }
        _ => (authority.to_string(), None),
    };
    Ok(Endpoint::new(protocol.to_string(), host, port))
}

fn default_operations() -> u64 {
    300
}

fn default_amount() -> u64 {
    1
}

fn default_consume_probability() -> f64 {
    0.5
}

fn default_seed() -> u64 {
    42
}

fn default_poll_interval_ms() -> u64 {
    1_000
}

fn default_timeout_seconds() -> u64 {
    180
}

fn default_proposal_retry_interval_ms() -> u64 {
    1_000
}

fn default_proposal_retry_timeout_seconds() -> u64 {
    180
}

fn default_artifacts_dir() -> PathBuf {
    PathBuf::from("benchmarks/multisig-e2e/reports")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_with_port() {
        let endpoint = parse_miden_endpoint("http://localhost:57291").unwrap();
        assert_eq!(endpoint.protocol(), "http");
        assert_eq!(endpoint.host(), "localhost");
        assert_eq!(endpoint.port(), Some(57291));
    }

    #[test]
    fn parses_endpoint_without_port() {
        let endpoint = parse_miden_endpoint("https://rpc.devnet.miden.io").unwrap();
        assert_eq!(endpoint.protocol(), "https");
        assert_eq!(endpoint.host(), "rpc.devnet.miden.io");
        assert_eq!(endpoint.port(), None);
    }

    #[test]
    fn rejects_endpoint_path() {
        assert!(parse_miden_endpoint("https://rpc.devnet.miden.io/path").is_err());
    }
}
