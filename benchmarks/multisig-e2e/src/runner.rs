use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use guardian_client::GuardianClient;
use miden_multisig_client::{
    AccountId, ConsumableNote, MultisigError, NoteFilter, Proposal, TransactionType,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::canonicalization::{CanonicalizationTracker, ProposalKind};
use crate::config::RunConfig;
use crate::fixture::Fixture;
use crate::runtime::{BenchClient, is_miden_sync_tip_ahead, load_clients, load_observer};

#[derive(Debug, Deserialize, Serialize)]
pub struct OperationRecord {
    pub operation: u64,
    pub started_at: DateTime<Utc>,
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub consumed: bool,
    pub send_proposal_id: String,
    pub send_nonce: u64,
    #[serde(default)]
    pub send_proposal_retries: u64,
    #[serde(default)]
    pub send_proposal_retry_wait_ms: u64,
    pub send_proposal_ms: u64,
    pub send_execution_ms: u64,
    pub send_canonicalization_ms: Option<u64>,
    pub note_visibility_ms: u64,
    pub note_id: String,
    pub consume_proposal_id: Option<String>,
    pub consume_nonce: Option<u64>,
    #[serde(default)]
    pub consume_proposal_retries: Option<u64>,
    #[serde(default)]
    pub consume_proposal_retry_wait_ms: Option<u64>,
    pub consume_proposal_ms: Option<u64>,
    pub consume_execution_ms: Option<u64>,
    pub consume_canonicalization_ms: Option<u64>,
    pub total_ms: u64,
}

#[derive(Debug, Serialize)]
struct RunManifest {
    schema_version: u32,
    started_at: DateTime<Utc>,
    guardian_endpoint: String,
    miden_endpoint: String,
    account_ids: Vec<String>,
    faucet_id: String,
    operations: u64,
    amount: u64,
    consume_probability: f64,
    seed: u64,
    poll_interval_ms: u64,
    timeout_seconds: u64,
    proposal_retry_interval_ms: u64,
    proposal_retry_timeout_seconds: u64,
    max_duration_seconds: Option<u64>,
    records_file: String,
    canonicalization_file: String,
}

#[derive(Debug, Serialize)]
struct FailureRecord {
    failed_at: DateTime<Utc>,
    operation: u64,
    sender: String,
    receiver: String,
    error: String,
}

#[derive(Debug, Clone, Copy)]
struct OperationSpec {
    index: u64,
    faucet_id: AccountId,
    amount: u64,
    consumed: bool,
}

struct ProposalAttempt {
    proposal: Proposal,
    retries: u64,
    retry_wait_ms: u64,
}

pub async fn preflight(config: &RunConfig) -> Result<()> {
    let fixture = Fixture::load(&config.accounts_file)?;
    let faucet_id = parse_faucet_id(config)?;
    let mut clients = load_clients(&fixture, config).await?;

    for client in &mut clients {
        sync_network_with_retry(client, config)
            .await
            .with_context(|| {
                format!(
                    "failed to refresh notes for {} during preflight",
                    client.label
                )
            })?;
        let notes = client
            .client
            .list_consumable_notes_filtered(NoteFilter::by_faucet(faucet_id))
            .await?;
        let note_balance: u64 = notes
            .iter()
            .map(|note| note.amount_for_faucet(faucet_id))
            .sum();
        let nonce = client
            .client
            .account()
            .ok_or_else(|| anyhow!("{} account is not loaded", client.label))?
            .nonce();
        println!(
            "{}: account={} nonce={} vault_balance={} consumable_note_balance={} notes={}",
            client.label,
            client.account_id,
            nonce,
            client.balance(faucet_id),
            note_balance,
            notes.len()
        );
    }

    let required = required_balance(config);
    println!(
        "worst-case starting vault balance per account for this profile: {}",
        required
    );
    Ok(())
}

pub async fn bootstrap(config: &RunConfig) -> Result<()> {
    let fixture = Fixture::load(&config.accounts_file)?;
    let faucet_id = parse_faucet_id(config)?;
    let mut clients = load_clients(&fixture, config).await?;

    for (client, fixture_account) in clients.iter_mut().zip(&fixture.accounts) {
        let mut observer = load_observer(&fixture, fixture_account).await?;
        sync_network_with_retry(client, config).await?;
        let notes = client
            .client
            .list_consumable_notes_filtered(NoteFilter::by_faucet(faucet_id))
            .await?;
        if notes.is_empty() {
            println!("{}: no consumable faucet notes", client.label);
            continue;
        }
        for note in notes {
            let amount = note.amount_for_faucet(faucet_id);
            let proposal = propose_with_retry(
                client,
                TransactionType::consume_notes(vec![note.id]),
                config,
            )
            .await
            .with_context(|| format!("failed to create {} bootstrap proposal", client.label))?
            .proposal;
            ensure_ready(&proposal.status, &proposal.id)?;
            execute_with_retry(client, &proposal.id, config)
                .await
                .with_context(|| {
                    format!("failed to execute {} bootstrap proposal", client.label)
                })?;
            await_canonical(
                &mut observer,
                &client.label,
                client.account_id,
                proposal.nonce,
                config,
            )
            .await?;
            println!(
                "{}: consumed note {} ({} units), nonce {}",
                client.label, note.id, amount, proposal.nonce
            );
        }
    }
    Ok(())
}

pub async fn run(config: &RunConfig) -> Result<PathBuf> {
    let fixture = Fixture::load(&config.accounts_file)?;
    let faucet_id = parse_faucet_id(config)?;
    let mut clients = load_clients(&fixture, config).await?;
    ensure_starting_balances(&clients, faucet_id, config)?;
    fs::create_dir_all(&config.artifacts_dir).with_context(|| {
        format!(
            "failed to create artifact directory {}",
            config.artifacts_dir.display()
        )
    })?;
    let started_at = Utc::now();
    let run_name = format!("multisig-e2e-{}", started_at.format("%Y%m%dT%H%M%S%.3fZ"));
    let path = config.artifacts_dir.join(format!("{run_name}.jsonl"));
    let canonicalization_path = config
        .artifacts_dir
        .join(format!("{run_name}.canonicalization.json"));
    write_manifest(
        config,
        &fixture,
        started_at,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("artifact path is not valid UTF-8"))?,
        canonicalization_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("canonicalization path is not valid UTF-8"))?,
        config
            .artifacts_dir
            .join(format!("{run_name}.manifest.json")),
    )?;
    let file = File::create(&path)
        .with_context(|| format!("failed to create artifact {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let mut records = Vec::with_capacity(config.operations as usize);
    let run_started = Instant::now();
    let tracker = CanonicalizationTracker::start(
        &fixture,
        Duration::from_millis(config.poll_interval_ms),
        Duration::from_secs(config.timeout_seconds),
    )
    .await?;

    for operation in 0..config.operations {
        if duration_limit_reached(run_started, config.max_duration_seconds) {
            println!(
                "duration limit reached after {} completed operations",
                records.len()
            );
            break;
        }
        let consumed = rng.random_bool(config.consume_probability);
        let (sender, receiver) = client_pair(&mut clients, operation);
        let spec = OperationSpec {
            index: operation,
            faucet_id,
            amount: config.amount,
            consumed,
        };
        let result = execute_operation(spec, sender, receiver, config, &tracker).await;
        let record = match result {
            Ok(record) => record,
            Err(error) => {
                writer.flush()?;
                let failure = FailureRecord {
                    failed_at: Utc::now(),
                    operation: operation + 1,
                    sender: sender.label.clone(),
                    receiver: receiver.label.clone(),
                    error: format!("{error:#}"),
                };
                let failure_path = config
                    .artifacts_dir
                    .join(format!("{run_name}.failure.json"));
                fs::write(&failure_path, serde_json::to_vec_pretty(&failure)?)?;
                tracker.finish(&canonicalization_path).await?;
                return Err(error).with_context(|| {
                    format!(
                        "operation {} failed; details written to {}",
                        operation + 1,
                        failure_path.display()
                    )
                });
            }
        };
        serde_json::to_writer(&mut writer, &record)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        println!(
            "{}/{} {} -> {} proposal={}ms retries={} retry_wait={}ms execute={}ms canonical=deferred note={}ms consumed={}",
            operation + 1,
            config.operations,
            record.sender,
            record.receiver,
            record.send_proposal_ms,
            record.send_proposal_retries,
            record.send_proposal_retry_wait_ms,
            record.send_execution_ms,
            record.note_visibility_ms,
            record.consumed
        );
        records.push(record);
    }

    writer.flush()?;
    drop(writer);
    tracker.finish(&canonicalization_path).await?;
    write_and_print_summary(&path)?;
    Ok(path)
}

fn write_manifest(
    config: &RunConfig,
    fixture: &Fixture,
    started_at: DateTime<Utc>,
    records_file: &str,
    canonicalization_file: &str,
    path: PathBuf,
) -> Result<()> {
    let manifest = RunManifest {
        schema_version: 4,
        started_at,
        guardian_endpoint: fixture.guardian_endpoint.clone(),
        miden_endpoint: fixture.miden_endpoint.clone(),
        account_ids: fixture
            .accounts
            .iter()
            .map(|account| account.account_id.clone())
            .collect(),
        faucet_id: config.faucet_id.clone(),
        operations: config.operations,
        amount: config.amount,
        consume_probability: config.consume_probability,
        seed: config.seed,
        poll_interval_ms: config.poll_interval_ms,
        timeout_seconds: config.timeout_seconds,
        proposal_retry_interval_ms: config.proposal_retry_interval_ms,
        proposal_retry_timeout_seconds: config.proposal_retry_timeout_seconds,
        max_duration_seconds: config.max_duration_seconds,
        records_file: records_file.to_string(),
        canonicalization_file: canonicalization_file.to_string(),
    };
    fs::write(&path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write run manifest {}", path.display()))
}

async fn execute_operation(
    spec: OperationSpec,
    sender: &mut BenchClient,
    receiver: &mut BenchClient,
    config: &RunConfig,
    tracker: &CanonicalizationTracker,
) -> Result<OperationRecord> {
    let started_at = Utc::now();
    let started = Instant::now();
    let existing_notes = consumable_note_ids(receiver, spec.faucet_id, config).await?;
    let proposal_started = Instant::now();
    let send_attempt = propose_with_retry(
        sender,
        TransactionType::transfer(receiver.account_id, spec.faucet_id, spec.amount),
        config,
    )
    .await?;
    let send_proposal_ms = elapsed_ms(proposal_started);
    let send = send_attempt.proposal;
    ensure_ready(&send.status, &send.id)?;

    let execution_started = Instant::now();
    execute_with_retry(sender, &send.id, config).await?;
    let send_execution_ms = elapsed_ms(execution_started);
    tracker.observe(
        &sender.label,
        spec.index + 1,
        ProposalKind::Send,
        send.nonce,
    )?;
    let note_started = Instant::now();
    let note = await_new_note(
        receiver,
        spec.faucet_id,
        spec.amount,
        &existing_notes,
        config,
    )
    .await?;
    let note_visibility_ms = elapsed_ms(note_started);

    let mut consume_proposal_id = None;
    let mut consume_nonce = None;
    let mut consume_proposal_retries = None;
    let mut consume_proposal_retry_wait_ms = None;
    let mut consume_proposal_ms = None;
    let mut consume_execution_ms = None;
    if spec.consumed {
        let proposal_started = Instant::now();
        let consume_attempt = propose_with_retry(
            receiver,
            TransactionType::consume_notes(vec![note.id]),
            config,
        )
        .await?;
        consume_proposal_ms = Some(elapsed_ms(proposal_started));
        consume_proposal_retries = Some(consume_attempt.retries);
        consume_proposal_retry_wait_ms = Some(consume_attempt.retry_wait_ms);
        let consume = consume_attempt.proposal;
        ensure_ready(&consume.status, &consume.id)?;
        let execution_started = Instant::now();
        execute_with_retry(receiver, &consume.id, config).await?;
        consume_execution_ms = Some(elapsed_ms(execution_started));
        tracker.observe(
            &receiver.label,
            spec.index + 1,
            ProposalKind::Consume,
            consume.nonce,
        )?;
        consume_proposal_id = Some(consume.id);
        consume_nonce = Some(consume.nonce);
    }

    Ok(OperationRecord {
        operation: spec.index + 1,
        started_at,
        sender: sender.label.clone(),
        receiver: receiver.label.clone(),
        amount: spec.amount,
        consumed: spec.consumed,
        send_proposal_id: send.id,
        send_nonce: send.nonce,
        send_proposal_retries: send_attempt.retries,
        send_proposal_retry_wait_ms: send_attempt.retry_wait_ms,
        send_proposal_ms,
        send_execution_ms,
        send_canonicalization_ms: None,
        note_visibility_ms,
        note_id: note.id.to_string(),
        consume_proposal_id,
        consume_nonce,
        consume_proposal_retries,
        consume_proposal_retry_wait_ms,
        consume_proposal_ms,
        consume_execution_ms,
        consume_canonicalization_ms: None,
        total_ms: elapsed_ms(started),
    })
}

async fn propose_with_retry(
    client: &mut BenchClient,
    transaction_type: TransactionType,
    config: &RunConfig,
) -> Result<ProposalAttempt> {
    let deadline = Instant::now() + Duration::from_secs(config.proposal_retry_timeout_seconds);
    let retry_interval = Duration::from_millis(config.proposal_retry_interval_ms);
    let mut retries = 0;
    let mut retry_wait_ms = 0;

    loop {
        match client
            .client
            .propose_transaction(transaction_type.clone())
            .await
        {
            Ok(proposal) => {
                return Ok(ProposalAttempt {
                    proposal,
                    retries,
                    retry_wait_ms,
                });
            }
            Err(error) if is_retryable_proposal_error(&error) => {
                let now = Instant::now();
                if now >= deadline {
                    bail!(
                        "timed out retrying proposal for {} after transient error: {}",
                        client.label,
                        error
                    );
                }
                retries += 1;
                let wait_started = Instant::now();
                tokio::time::sleep(retry_interval.min(deadline - now)).await;
                retry_wait_ms += elapsed_ms(wait_started);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn execute_with_retry(
    client: &mut BenchClient,
    proposal_id: &str,
    config: &RunConfig,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(config.proposal_retry_timeout_seconds);
    let retry_interval = Duration::from_millis(config.proposal_retry_interval_ms);
    loop {
        match client.client.execute_proposal(proposal_id).await {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_execution_error(&error) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error.into());
                }
                tokio::time::sleep(retry_interval.min(deadline - now)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn is_retryable_proposal_error(error: &MultisigError) -> bool {
    match error {
        MultisigError::GuardianConnection(_) => true,
        MultisigError::GuardianServer(message) => {
            message.contains("conflict_pending_delta")
                || message.contains("There's already a pending change for this account")
        }
        MultisigError::MidenClient(_) => is_miden_sync_tip_ahead(error),
        _ => false,
    }
}

fn is_retryable_execution_error(error: &MultisigError) -> bool {
    matches!(error, MultisigError::GuardianConnection(_)) || is_miden_sync_tip_ahead(error)
}

fn client_pair(
    clients: &mut [BenchClient],
    operation: u64,
) -> (&mut BenchClient, &mut BenchClient) {
    let (alice, bob) = clients.split_at_mut(1);
    if operation.is_multiple_of(2) {
        (&mut alice[0], &mut bob[0])
    } else {
        (&mut bob[0], &mut alice[0])
    }
}

async fn consumable_note_ids(
    client: &mut BenchClient,
    faucet_id: AccountId,
    config: &RunConfig,
) -> Result<HashSet<String>> {
    sync_network_with_retry(client, config).await?;
    Ok(client
        .client
        .list_consumable_notes_filtered(NoteFilter::by_faucet(faucet_id))
        .await?
        .into_iter()
        .map(|note| note.id.to_string())
        .collect())
}

async fn await_new_note(
    client: &mut BenchClient,
    faucet_id: AccountId,
    amount: u64,
    existing_notes: &HashSet<String>,
    config: &RunConfig,
) -> Result<ConsumableNote> {
    let deadline = Instant::now() + Duration::from_secs(config.timeout_seconds);
    let retry_interval = Duration::from_millis(config.proposal_retry_interval_ms);
    loop {
        sync_network_until(client, deadline, retry_interval).await?;
        let notes = client
            .client
            .list_consumable_notes_filtered(NoteFilter::by_faucet(faucet_id))
            .await?;
        if let Some(note) = notes.into_iter().find(|note| {
            !existing_notes.contains(&note.id.to_string())
                && note.amount_for_faucet(faucet_id) == amount
        }) {
            return Ok(note);
        }
        let now = Instant::now();
        if now >= deadline {
            bail!("timed out waiting for the P2ID note to become consumable");
        }
        tokio::time::sleep(Duration::from_millis(config.poll_interval_ms).min(deadline - now))
            .await;
    }
}

async fn sync_network_with_retry(client: &mut BenchClient, config: &RunConfig) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(config.proposal_retry_timeout_seconds);
    let retry_interval = Duration::from_millis(config.proposal_retry_interval_ms);
    sync_network_until(client, deadline, retry_interval).await
}

async fn sync_network_until(
    client: &mut BenchClient,
    deadline: Instant,
    retry_interval: Duration,
) -> Result<()> {
    loop {
        match client.client.sync_network_only().await {
            Ok(()) => return Ok(()),
            Err(error) if is_miden_sync_tip_ahead(&error) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error.into());
                }
                tokio::time::sleep(retry_interval.min(deadline - now)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn await_canonical(
    observer: &mut GuardianClient,
    label: &str,
    account_id: AccountId,
    nonce: u64,
    config: &RunConfig,
) -> Result<u64> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(config.timeout_seconds);
    loop {
        let poll_error = match observer.get_delta(&account_id, nonce).await {
            Ok(response) => {
                if let Some(delta) = response.delta {
                    if delta.canonical_at.is_some() {
                        return Ok(elapsed_ms(started));
                    }
                    if delta.discarded_at.is_some() {
                        bail!("Guardian discarded {} nonce {}", label, nonce);
                    }
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        if Instant::now() >= deadline {
            if let Some(error) = poll_error {
                bail!(
                    "timed out observing {} nonce {} after polling error: {}",
                    label,
                    nonce,
                    error
                );
            }
            bail!(
                "timed out waiting for Guardian to canonicalize {} nonce {}",
                label,
                nonce
            );
        }
        tokio::time::sleep(Duration::from_millis(config.poll_interval_ms)).await;
    }
}

fn required_balance(config: &RunConfig) -> u64 {
    config.amount.saturating_mul(config.operations.div_ceil(2))
}

fn ensure_starting_balances(
    clients: &[BenchClient],
    faucet_id: AccountId,
    config: &RunConfig,
) -> Result<()> {
    let required = required_balance(config);
    for client in clients {
        let balance = client.balance(faucet_id);
        if balance < required {
            bail!(
                "{} vault balance is {}; profile requires {} in the worst case. Fund the account and run bootstrap",
                client.label,
                balance,
                required
            );
        }
    }
    Ok(())
}

fn ensure_ready(status: &miden_multisig_client::ProposalStatus, id: &str) -> Result<()> {
    if !status.is_ready() {
        bail!("1-of-1 proposal {id} was not ready after creation");
    }
    Ok(())
}

fn parse_faucet_id(config: &RunConfig) -> Result<AccountId> {
    AccountId::parse(&config.faucet_id)
        .map(|(account_id, _network_id)| account_id)
        .context("faucet_id is not a valid Miden hex or bech32 account ID")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn duration_limit_reached(started: Instant, max_duration_seconds: Option<u64>) -> bool {
    max_duration_seconds
        .map(|seconds| started.elapsed() >= Duration::from_secs(seconds))
        .unwrap_or(false)
}

#[derive(Debug, Serialize)]
struct LatencyStats {
    samples: usize,
    min_ms: u64,
    p50_ms: u64,
    p95_ms: u64,
    max_ms: u64,
    mean_ms: f64,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    completed_operations: usize,
    consumed_operations: usize,
    unconsumed_operations: usize,
    measured_runtime_ms: u64,
    send_proposal_retries: u64,
    send_proposal_retry_wait: LatencyStats,
    send_proposal: LatencyStats,
    send_execution: LatencyStats,
    operation_total: LatencyStats,
    first_quintile_send_proposal: LatencyStats,
    last_quintile_send_proposal: LatencyStats,
    send_proposal_p50_growth_percent: f64,
    send_proposal_active: LatencyStats,
    first_quintile_send_proposal_active: LatencyStats,
    last_quintile_send_proposal_active: LatencyStats,
    send_proposal_active_p50_growth_percent: f64,
}

pub fn summarize_report(path: &Path) -> Result<PathBuf> {
    write_and_print_summary(path)
}

fn write_and_print_summary(path: &Path) -> Result<PathBuf> {
    let records = read_records(path)?;
    let summary = RunSummary::from_records(&records)?;
    let summary_path = path.with_extension("summary.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)
        .with_context(|| format!("failed to write {}", summary_path.display()))?;
    println!(
        "completed={} consumed={} runtime={:.1}min proposal_p50={}ms proposal_p95={}ms retries={} first_q_p50={}ms last_q_p50={}ms growth={:+.1}% active_first_q_p50={}ms active_last_q_p50={}ms active_growth={:+.1}%",
        summary.completed_operations,
        summary.consumed_operations,
        summary.measured_runtime_ms as f64 / 60_000.0,
        summary.send_proposal.p50_ms,
        summary.send_proposal.p95_ms,
        summary.send_proposal_retries,
        summary.first_quintile_send_proposal.p50_ms,
        summary.last_quintile_send_proposal.p50_ms,
        summary.send_proposal_p50_growth_percent,
        summary.first_quintile_send_proposal_active.p50_ms,
        summary.last_quintile_send_proposal_active.p50_ms,
        summary.send_proposal_active_p50_growth_percent
    );
    println!("wrote {}", summary_path.display());
    Ok(summary_path)
}

impl RunSummary {
    fn from_records(records: &[OperationRecord]) -> Result<Self> {
        if records.is_empty() {
            bail!("report contains no completed operations");
        }
        let quintile_size = records.len().div_ceil(5).max(1);
        let first = &records[..quintile_size];
        let last = &records[records.len() - quintile_size..];
        let first_stats = latency_stats(first.iter().map(|record| record.send_proposal_ms))?;
        let last_stats = latency_stats(last.iter().map(|record| record.send_proposal_ms))?;
        let growth = percent_growth(first_stats.p50_ms, last_stats.p50_ms);
        let active_ms = |record: &OperationRecord| {
            active_proposal_ms(record.send_proposal_ms, record.send_proposal_retry_wait_ms)
        };
        let first_active_stats = latency_stats(first.iter().map(active_ms))?;
        let last_active_stats = latency_stats(last.iter().map(active_ms))?;
        let active_growth = percent_growth(first_active_stats.p50_ms, last_active_stats.p50_ms);

        Ok(Self {
            completed_operations: records.len(),
            consumed_operations: records.iter().filter(|record| record.consumed).count(),
            unconsumed_operations: records.iter().filter(|record| !record.consumed).count(),
            measured_runtime_ms: records.iter().map(|record| record.total_ms).sum(),
            send_proposal_retries: records
                .iter()
                .map(|record| record.send_proposal_retries)
                .sum(),
            send_proposal_retry_wait: latency_stats(
                records
                    .iter()
                    .map(|record| record.send_proposal_retry_wait_ms),
            )?,
            send_proposal: latency_stats(records.iter().map(|record| record.send_proposal_ms))?,
            send_execution: latency_stats(records.iter().map(|record| record.send_execution_ms))?,
            operation_total: latency_stats(records.iter().map(|record| record.total_ms))?,
            first_quintile_send_proposal: first_stats,
            last_quintile_send_proposal: last_stats,
            send_proposal_p50_growth_percent: growth,
            send_proposal_active: latency_stats(records.iter().map(active_ms))?,
            first_quintile_send_proposal_active: first_active_stats,
            last_quintile_send_proposal_active: last_active_stats,
            send_proposal_active_p50_growth_percent: active_growth,
        })
    }
}

fn read_records(path: &Path) -> Result<Vec<OperationRecord>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open benchmark report {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            result => Some((index, result)),
        })
        .map(|(index, line)| {
            let line = line.with_context(|| format!("failed to read line {}", index + 1))?;
            serde_json::from_str(&line)
                .with_context(|| format!("failed to parse report line {}", index + 1))
        })
        .collect()
}

fn latency_stats(values: impl IntoIterator<Item = u64>) -> Result<LatencyStats> {
    let mut values: Vec<u64> = values.into_iter().collect();
    if values.is_empty() {
        bail!("cannot summarize an empty latency series");
    }
    values.sort_unstable();
    let sum: u128 = values.iter().map(|value| u128::from(*value)).sum();
    let p50_index = (values.len() - 1) / 2;
    let p95_index = (values.len() * 95).div_ceil(100).saturating_sub(1);
    Ok(LatencyStats {
        samples: values.len(),
        min_ms: values[0],
        p50_ms: values[p50_index],
        p95_ms: values[p95_index],
        max_ms: values[values.len() - 1],
        mean_ms: sum as f64 / values.len() as f64,
    })
}

fn percent_growth(first: u64, last: u64) -> f64 {
    if first == 0 {
        return 0.0;
    }
    (last as f64 - first as f64) * 100.0 / first as f64
}

fn active_proposal_ms(total_ms: u64, retry_wait_ms: u64) -> u64 {
    total_ms.saturating_sub(retry_wait_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::address::NetworkId;

    #[test]
    fn latency_stats_uses_nearest_rank_percentiles() {
        let stats = latency_stats([30, 10, 20]).unwrap();
        assert_eq!(stats.p50_ms, 20);
        assert_eq!(stats.p95_ms, 30);
    }

    #[test]
    fn parses_testnet_faucet_address() {
        let account_id = AccountId::from_hex("0x023e462be6c55661144b5440f07c2c").unwrap();
        let config = RunConfig {
            accounts_file: PathBuf::new(),
            faucet_id: account_id.to_bech32(NetworkId::Testnet),
            operations: 1,
            amount: 1,
            consume_probability: 0.5,
            seed: 42,
            poll_interval_ms: 1_000,
            timeout_seconds: 180,
            proposal_retry_interval_ms: 1_000,
            proposal_retry_timeout_seconds: 180,
            max_duration_seconds: None,
            artifacts_dir: PathBuf::new(),
        };

        assert_eq!(parse_faucet_id(&config).unwrap(), account_id);
    }

    #[test]
    fn recognizes_only_known_transient_proposal_errors() {
        let pending = MultisigError::GuardianServer(
            "There's already a pending change for this account. Finish it first.".to_string(),
        );
        let unrelated = MultisigError::GuardianServer("invalid signature".to_string());

        assert!(is_retryable_proposal_error(&pending));
        assert!(!is_retryable_proposal_error(&unrelated));
    }

    #[test]
    fn retries_guardian_connection_errors_for_proposal_and_execution() {
        let connection = MultisigError::GuardianConnection("temporarily unavailable".to_string());

        assert!(is_retryable_proposal_error(&connection));
        assert!(is_retryable_execution_error(&connection));
    }

    #[test]
    fn recognizes_sync_tip_race_from_nullifier_endpoint() {
        let error = MultisigError::MidenClient(
            "endpoint: SyncNullifiers, message: block_to (857206) is greater than chain tip (857205)"
                .to_string(),
        );

        assert!(is_miden_sync_tip_ahead(&error));
    }

    #[test]
    fn active_proposal_time_excludes_retry_sleep() {
        assert_eq!(active_proposal_ms(4_500, 3_000), 1_500);
    }
}
