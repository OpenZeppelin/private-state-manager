# Guardian multisig end-to-end benchmark

This benchmark measures a real multisig proposal lifecycle. Two persistent 1-of-1 accounts
alternate P2ID transfers through Guardian while the Miden transactions are proved and submitted.
A deterministic fraction of received notes is consumed.

It is intentionally separate from `benchmarks/prod-server`: that harness measures Guardian API
capacity with synthetic deltas, while this one measures the full Rust SDK, Guardian, prover, and
Miden network path.

## Local Guardian workflow

Start a local Guardian server configured for the same Miden network as the benchmark. Create the
two persistent accounts:

```bash
cargo run -p guardian-multisig-e2e-benchmark -- prepare \
  --miden-endpoint https://rpc.testnet.miden.io
```

`prepare` registers Alice and Bob with Guardian and writes their account IDs and Falcon secret keys
to `.guardian/bench/multisig-e2e-accounts.json`. The file is mode `0600` on Unix and is ignored by
git. It is not overwritten automatically because the account and secret-key binding must remain
stable.

Each generated account is persisted before it is registered with Guardian. If preparation stops
partway through, the partial fixture retains every key that may have been registered. Preserve or
move that file before deciding whether to reprovision.

Fund both printed account IDs from the faucet selected for the run. Update `testnet.local.toml`
with the faucet's hex or bech32 address, then turn the funding notes into spendable vault assets:

```bash
cargo run -p guardian-multisig-e2e-benchmark -- bootstrap \
  --config benchmarks/multisig-e2e/testnet.local.toml
```

Check balances and connectivity without mutating either account:

```bash
cargo run -p guardian-multisig-e2e-benchmark -- preflight \
  --config benchmarks/multisig-e2e/testnet.local.toml
```

Run the benchmark:

```bash
cargo run --release -p guardian-multisig-e2e-benchmark -- run \
  --config benchmarks/multisig-e2e/testnet.local.toml
```

## Measurements and artifacts

Each completed send is flushed as one JSONL record under `reports/`. A companion manifest captures
the endpoints, public account IDs, workload parameters, and random seed without copying account
secrets. If a run stops early, a failure sidecar captures the operation and full error chain.

The primary timings are:

- `send_proposal_ms`: end-to-end proposal time, including retry attempts and retry sleep.
- `send_proposal_retry_wait_ms`: time spent sleeping between proposal attempts.
- active proposal time: derived as proposal time minus retry sleep.
- `send_execution_ms`: send execution, proving, and submission.
- `note_visibility_ms`: time until the receiver can discover the new note.
- `total_ms`: the complete operation, including optional note consumption.

The summary compares both end-to-end and active proposal medians in the first and last workload
quintiles. The active comparison separates Guardian/client work from configured retry sleep.

Canonicalization observations are queued during the run and collected after foreground account
operations stop. The separate `canonicalization.json` artifact uses Guardian's `canonical_at`
timestamp. A timestamp at or before the local observation start is recorded as zero rather than
falling back to the deferred wall clock. All observations share one `timeout_seconds` drain
deadline, so final collection is bounded independently of observation count.

If Guardian reports that a prior delta is still pending, the benchmark waits
`proposal_retry_interval_ms`, then reruns the proposal workflow against the latest account state.
Transient Guardian connection failures use the same retry deadline for proposal creation and
execution. Miden's transient `block_to`-ahead-of-chain-tip sync race is also retried regardless of
which sync endpoint reports it. Other errors fail immediately.

`max_duration_seconds` stops scheduling new operations at an operation boundary. The shared
canonicalization drain runs afterward before the final summary is written.

Summarize a completed or manually interrupted JSONL report:

```bash
cargo run -p guardian-multisig-e2e-benchmark -- summarize \
  --report benchmarks/multisig-e2e/reports/multisig-e2e-<timestamp>.jsonl
```

## Funding calculation

The runner requires a conservative starting vault balance of
`amount * ceil(operations / 2)` for each account. This guarantees that deterministic consume
choices cannot make a run fail for lack of spendable funds. Funding notes do not count until
`bootstrap` has consumed them.

For a quick smoke, lower `operations` before attempting a long run.
