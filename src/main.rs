mod ax;
mod browser;
mod cdp;
mod driver;
mod fill;
mod mcp;
mod selftest;
mod vault;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Fill Bitwarden logins into Chrome through cua-driver, without showing
/// them to the agent that asked.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Unlock the vault for agents (asks for the master password).
    Unlock,
    /// Lock the vault and forget the stored session.
    Lock,
    /// Show whether the vault is unlocked for agents.
    Status,
    /// Run the MCP server agents use (stdio).
    Mcp,
    /// Fill a test login on the-internet.herokuapp.com in a throwaway Chrome.
    Selftest,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("cua-login: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Unlock => {
            vault::unlock()?;
            println!("unlocked; run `cua-login lock` when done");
        }
        Command::Lock => {
            vault::lock()?;
            println!("locked");
        }
        Command::Status => println!("{}", vault::status()?),
        Command::Mcp => mcp::serve()?,
        Command::Selftest => selftest::run()?,
    }
    Ok(())
}
