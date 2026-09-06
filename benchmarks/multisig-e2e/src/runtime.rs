use anyhow::{Context, Result};
use guardian_client::{Auth, FalconRpoSigner, GuardianClient};
use miden_multisig_client::{AccountId, MultisigClient, MultisigError, SecretKey};
use miden_protocol::asset::Asset;
use miden_protocol::utils::serde::Deserializable;
use tempfile::TempDir;

use crate::config::RunConfig;
use crate::config::parse_miden_endpoint;
use crate::fixture::{AccountFixture, Fixture};

pub struct BenchClient {
    pub label: String,
    pub account_id: AccountId,
    pub client: MultisigClient,
    _data_dir: TempDir,
}

impl BenchClient {
    pub fn balance(&self, faucet_id: AccountId) -> u64 {
        self.client
            .account()
            .into_iter()
            .flat_map(|account| account.inner().vault().assets())
            .filter_map(|asset| match asset {
                Asset::Fungible(asset) if asset.faucet_id() == faucet_id => {
                    Some(asset.amount().as_u64())
                }
                _ => None,
            })
            .sum()
    }
}

pub async fn load_clients(fixture: &Fixture, config: &RunConfig) -> Result<Vec<BenchClient>> {
    let mut clients = Vec::with_capacity(fixture.accounts.len());
    for account in &fixture.accounts {
        clients.push(load_client(fixture, account, config).await?);
    }
    Ok(clients)
}

async fn load_client(
    fixture: &Fixture,
    account: &AccountFixture,
    config: &RunConfig,
) -> Result<BenchClient> {
    let endpoint = parse_miden_endpoint(&fixture.miden_endpoint)?;
    let account_id = AccountId::from_hex(&account.account_id)
        .with_context(|| format!("invalid account ID for {}", account.label))?;
    let data_dir = TempDir::new().context("failed to create temporary Miden client directory")?;
    let mut client = MultisigClient::builder()
        .miden_endpoint(endpoint)
        .guardian_endpoint(fixture.guardian_endpoint.clone())
        .account_dir(data_dir.path())
        .with_secret_key(parse_secret_key(account)?)
        .build()
        .await
        .with_context(|| format!("failed to build {} client", account.label))?;
    client
        .pull_account(account_id)
        .await
        .with_context(|| format!("failed to pull {} account from Guardian", account.label))?;
    sync_with_retry(&mut client, config)
        .await
        .with_context(|| format!("failed to sync {} account", account.label))?;

    Ok(BenchClient {
        label: account.label.clone(),
        account_id,
        client,
        _data_dir: data_dir,
    })
}

async fn sync_with_retry(client: &mut MultisigClient, config: &RunConfig) -> Result<()> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(config.proposal_retry_timeout_seconds);
    let retry_interval = std::time::Duration::from_millis(config.proposal_retry_interval_ms);
    loop {
        match client.sync().await {
            Ok(()) => return Ok(()),
            Err(error) if is_miden_sync_tip_ahead(&error) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(error.into());
                }
                tokio::time::sleep(retry_interval.min(deadline - now)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) fn is_miden_sync_tip_ahead(error: &MultisigError) -> bool {
    matches!(
        error,
        MultisigError::MidenClient(message)
            if message.contains("block_to (")
                && message.contains("is greater than chain tip (")
    )
}

pub async fn load_observer(fixture: &Fixture, account: &AccountFixture) -> Result<GuardianClient> {
    let observer = GuardianClient::connect(fixture.guardian_endpoint.clone())
        .await
        .with_context(|| {
            format!(
                "failed to connect {} canonicalization observer",
                account.label
            )
        })?
        .with_auth(Auth::FalconRpoSigner(FalconRpoSigner::new(
            parse_secret_key(account)?,
        )));
    Ok(observer)
}

fn parse_secret_key(account: &AccountFixture) -> Result<SecretKey> {
    let bytes = hex::decode(&account.secret_key_hex)
        .with_context(|| format!("invalid secret key hex for {}", account.label))?;
    SecretKey::read_from_bytes(&bytes)
        .with_context(|| format!("invalid Falcon secret key for {}", account.label))
}
