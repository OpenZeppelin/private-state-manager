mod canonicalization;
mod config;
mod fixture;
mod runner;
mod runtime;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use miden_multisig_client::AccountId;
use miden_protocol::address::NetworkId;

use config::RunConfig;

#[derive(Debug, Parser)]
#[command(about = "Real multisig proposal benchmark for Guardian")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Prepare {
        #[arg(long, default_value = "http://localhost:50051")]
        guardian_endpoint: String,
        #[arg(long, default_value = "https://rpc.devnet.miden.io")]
        miden_endpoint: String,
        #[arg(long, default_value = ".guardian/bench/multisig-e2e-accounts.json")]
        accounts_file: PathBuf,
    },
    Preflight {
        #[arg(long)]
        config: PathBuf,
    },
    Bootstrap {
        #[arg(long)]
        config: PathBuf,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    Summarize {
        #[arg(long)]
        report: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Prepare {
            guardian_endpoint,
            miden_endpoint,
            accounts_file,
        } => {
            let fixture =
                fixture::prepare(guardian_endpoint, miden_endpoint, &accounts_file).await?;
            println!("wrote {}", accounts_file.display());
            let network_id = if fixture.miden_endpoint.contains("testnet") {
                NetworkId::Testnet
            } else {
                NetworkId::Devnet
            };
            for account in fixture.accounts {
                let account_id = AccountId::from_hex(&account.account_id)?;
                println!(
                    "{}: {} ({})",
                    account.label,
                    account.account_id,
                    account_id.to_bech32(network_id.clone())
                );
            }
        }
        Command::Preflight { config } => runner::preflight(&RunConfig::load(&config)?).await?,
        Command::Bootstrap { config } => runner::bootstrap(&RunConfig::load(&config)?).await?,
        Command::Run { config } => {
            let path = runner::run(&RunConfig::load(&config)?).await?;
            println!("wrote {}", path.display());
        }
        Command::Summarize { report } => {
            runner::summarize_report(&report)?;
        }
    }
    Ok(())
}
