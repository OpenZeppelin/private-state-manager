use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use guardian_client::GuardianClient;
use miden_multisig_client::AccountId;
use serde::Serialize;

use crate::fixture::Fixture;
use crate::runtime::load_observer;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Send,
    Consume,
}

#[derive(Debug)]
struct Observation {
    operation: u64,
    kind: ProposalKind,
    nonce: u64,
    started_at: DateTime<Utc>,
    started: Instant,
}

#[derive(Debug)]
struct QueuedObservation {
    account: String,
    observation: Observation,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservationStatus {
    Canonical,
    Discarded,
    TimedOut,
    ObservationFailed,
}

#[derive(Debug, Serialize)]
struct CanonicalizationRecord {
    operation: u64,
    account: String,
    account_id: String,
    proposal_kind: ProposalKind,
    nonce: u64,
    started_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    elapsed_ms: u64,
    polls: u64,
    status: ObservationStatus,
    canonical_at: Option<String>,
    error: Option<String>,
}

pub struct CanonicalizationTracker {
    fixture: Fixture,
    observations: RefCell<Vec<QueuedObservation>>,
    poll_interval: Duration,
    timeout: Duration,
}

impl CanonicalizationTracker {
    pub async fn start(
        fixture: &Fixture,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<Self> {
        for account in &fixture.accounts {
            AccountId::from_hex(&account.account_id)
                .with_context(|| format!("invalid account ID for {}", account.label))?;
        }

        Ok(Self {
            fixture: fixture.clone(),
            observations: RefCell::new(Vec::new()),
            poll_interval,
            timeout,
        })
    }

    pub fn observe(
        &self,
        account: &str,
        operation: u64,
        kind: ProposalKind,
        nonce: u64,
    ) -> Result<()> {
        if !self
            .fixture
            .accounts
            .iter()
            .any(|fixture_account| fixture_account.label == account)
        {
            return Err(anyhow!(
                "no canonicalization observer for account {account}"
            ));
        }
        self.observations.borrow_mut().push(QueuedObservation {
            account: account.to_string(),
            observation: Observation {
                operation,
                kind,
                nonce,
                started_at: Utc::now(),
                started: Instant::now(),
            },
        });
        Ok(())
    }

    pub async fn finish(self, path: &Path) -> Result<()> {
        let mut records = Vec::new();
        let observations = self.observations.into_inner();
        let deadline = Instant::now() + self.timeout;
        println!(
            "collecting {} canonicalization observations with a shared {}s deadline",
            observations.len(),
            self.timeout.as_secs()
        );
        for account in &self.fixture.accounts {
            let mut observer = load_observer(&self.fixture, account).await?;
            let account_id = AccountId::from_hex(&account.account_id)
                .with_context(|| format!("invalid account ID for {}", account.label))?;
            for queued in observations
                .iter()
                .filter(|queued| queued.account == account.label)
            {
                records.push(
                    observe_delta(
                        &mut observer,
                        &account.label,
                        account_id,
                        &queued.observation,
                        self.poll_interval,
                        deadline,
                    )
                    .await,
                );
            }
        }
        records.sort_by_key(|record| (record.operation, kind_order(record.proposal_kind)));
        fs::write(path, serde_json::to_vec_pretty(&records)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

async fn observe_delta(
    observer: &mut GuardianClient,
    label: &str,
    account_id: AccountId,
    observation: &Observation,
    poll_interval: Duration,
    deadline: Instant,
) -> CanonicalizationRecord {
    let mut polls = 0;

    loop {
        polls += 1;
        let poll_error = match observer.get_delta(&account_id, observation.nonce).await {
            Ok(response) => {
                if let Some(delta) = response.delta {
                    if let Some(canonical_at) = delta.canonical_at {
                        return record(
                            label,
                            account_id,
                            observation,
                            polls,
                            ObservationStatus::Canonical,
                            Some(canonical_at),
                            None,
                        );
                    }
                    if delta.discarded_at.is_some() {
                        return record(
                            label,
                            account_id,
                            observation,
                            polls,
                            ObservationStatus::Discarded,
                            None,
                            None,
                        );
                    }
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };

        let now = Instant::now();
        if now >= deadline {
            let status = if poll_error.is_some() {
                ObservationStatus::ObservationFailed
            } else {
                ObservationStatus::TimedOut
            };
            return record(
                label,
                account_id,
                observation,
                polls,
                status,
                None,
                poll_error,
            );
        }
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
    }
}

fn record(
    label: &str,
    account_id: AccountId,
    observation: &Observation,
    polls: u64,
    status: ObservationStatus,
    canonical_at: Option<String>,
    error: Option<String>,
) -> CanonicalizationRecord {
    let elapsed_ms = canonical_at
        .as_deref()
        .and_then(|value| canonical_elapsed_ms(observation.started_at, value))
        .unwrap_or_else(|| wall_elapsed_ms(observation.started));
    CanonicalizationRecord {
        operation: observation.operation,
        account: label.to_string(),
        account_id: account_id.to_string(),
        proposal_kind: observation.kind,
        nonce: observation.nonce,
        started_at: observation.started_at,
        observed_at: Utc::now(),
        elapsed_ms,
        polls,
        status,
        canonical_at,
        error,
    }
}

fn kind_order(kind: ProposalKind) -> u8 {
    match kind {
        ProposalKind::Send => 0,
        ProposalKind::Consume => 1,
    }
}

fn canonical_elapsed_ms(started_at: DateTime<Utc>, canonical_at: &str) -> Option<u64> {
    let canonical_at = DateTime::parse_from_rfc3339(canonical_at)
        .ok()?
        .with_timezone(&Utc);
    Some(u64::try_from((canonical_at - started_at).num_milliseconds()).unwrap_or(0))
}

fn wall_elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_elapsed_clamps_timestamp_before_observation_to_zero() {
        let started_at = DateTime::parse_from_rfc3339("2026-07-23T08:00:01Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            canonical_elapsed_ms(started_at, "2026-07-23T08:00:00Z"),
            Some(0)
        );
    }

    #[test]
    fn canonical_elapsed_rejects_invalid_timestamp() {
        assert_eq!(canonical_elapsed_ms(Utc::now(), "not-a-timestamp"), None);
    }
}
