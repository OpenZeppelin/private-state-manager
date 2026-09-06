use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use miden_multisig_client::{MultisigClient, SecretKey};
use miden_protocol::utils::serde::Serializable;
use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempDir};

use crate::config::parse_miden_endpoint;

const FIXTURE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountFixture {
    pub label: String,
    pub account_id: String,
    pub secret_key_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub version: u32,
    pub guardian_endpoint: String,
    pub miden_endpoint: String,
    pub accounts: Vec<AccountFixture>,
}

impl Fixture {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read account fixture {}", path.display()))?;
        let fixture: Self = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse account fixture {}", path.display()))?;
        if fixture.version != FIXTURE_VERSION {
            bail!(
                "unsupported account fixture version {}; expected {}",
                fixture.version,
                FIXTURE_VERSION
            );
        }
        if fixture.accounts.len() != 2 {
            bail!("account fixture must contain exactly two accounts");
        }
        Ok(fixture)
    }
}

pub async fn prepare(
    guardian_endpoint: String,
    miden_endpoint: String,
    output: &Path,
) -> Result<Fixture> {
    if output.exists() {
        bail!(
            "refusing to overwrite existing account fixture {}; move it explicitly to reprovision",
            output.display()
        );
    }
    let endpoint = parse_miden_endpoint(&miden_endpoint)?;
    let mut fixture = Fixture {
        version: FIXTURE_VERSION,
        guardian_endpoint,
        miden_endpoint,
        accounts: Vec::with_capacity(2),
    };

    for label in ["alice", "bob"] {
        let secret_key = SecretKey::new();
        let secret_key_hex = hex::encode(secret_key.to_bytes());
        let data_dir =
            TempDir::new().context("failed to create temporary Miden client directory")?;
        let mut client = MultisigClient::builder()
            .miden_endpoint(endpoint.clone())
            .guardian_endpoint(fixture.guardian_endpoint.clone())
            .account_dir(data_dir.path())
            .with_secret_key(secret_key)
            .build()
            .await
            .with_context(|| format!("failed to build {label} client"))?;
        let commitment = client.user_commitment();
        let account_id = client
            .create_account(1, vec![commitment])
            .await
            .with_context(|| format!("failed to create {label} account"))?
            .id();
        fixture.accounts.push(AccountFixture {
            label: label.to_string(),
            account_id: account_id.to_string(),
            secret_key_hex,
        });
        persist_fixture(output, &fixture)?;
        client
            .push_account()
            .await
            .with_context(|| format!("failed to register {label} account with Guardian"))?;
    }

    Ok(fixture)
}

fn persist_fixture(output: &Path, fixture: &Fixture) -> Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create fixture directory {}", parent.display()))?;
    }
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to create fixture file in {}", directory.display()))?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), fixture)?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    restrict_permissions(temporary.path())?;
    temporary
        .persist(output)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to write account fixture {}", output.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(path: &Path) -> Result<()> {
    bail!(
        "cannot restrict permissions on {} on this platform; refusing to persist secret keys unprotected",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_partial_fixture_for_account_recovery() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("accounts.json");
        let mut fixture = Fixture {
            version: FIXTURE_VERSION,
            guardian_endpoint: "http://localhost:50051".to_string(),
            miden_endpoint: "https://rpc.testnet.miden.io".to_string(),
            accounts: vec![AccountFixture {
                label: "alice".to_string(),
                account_id: "0xalice".to_string(),
                secret_key_hex: "secret".to_string(),
            }],
        };

        persist_fixture(&path, &fixture).unwrap();

        let persisted: Fixture = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.accounts[0].secret_key_hex, "secret");

        fixture.accounts.push(AccountFixture {
            label: "bob".to_string(),
            account_id: "0xbob".to_string(),
            secret_key_hex: "another-secret".to_string(),
        });
        persist_fixture(&path, &fixture).unwrap();

        let persisted: Fixture = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(persisted.accounts.len(), 2);
    }
}
