//! hoster: a single-binary deployment orchestrator for one VPS (ADR-0001).
//!
//! `main` is the supervisor: it parses the command line and, for `run`, will own the startup and
//! shutdown order (docs/architecture.md §4). Errors are `anyhow` at this edge; the modules below
//! use their own `thiserror` types.

mod acme;
mod config;
mod orchestrator;
mod proxy;
mod registry;
mod routing;
mod runtime;
mod stack;
mod store;
mod web;

use clap::{Parser, Subcommand};

/// Single-binary deployment orchestrator for one VPS.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// The subcommand to execute; `run` when none was given.
impl From<Cli> for Command {
    fn from(cli: Cli) -> Self {
        cli.command.unwrap_or_default()
    }
}

#[derive(Debug, Default, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run the daemon: proxy, ACME, deploy engine, webhook receiver, poller and admin UI (default).
    #[default]
    Run,
    /// Check that the host is set up correctly for hoster.
    Doctor,
    /// Back up hoster's state.
    #[command(subcommand)]
    Backup(Backup),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Backup {
    /// Write a consistent snapshot of the SQLite state store.
    Db,
}

impl Command {
    async fn execute(self) -> anyhow::Result<()> {
        match self {
            Command::Run => anyhow::bail!("`hoster run` is not implemented yet"),
            Command::Doctor => anyhow::bail!("`hoster doctor` is not implemented yet"),
            Command::Backup(Backup::Db) => {
                anyhow::bail!("`hoster backup db` is not implemented yet")
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::from(Cli::parse());
    tracing::debug!(?command, "starting");
    command.execute().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("hoster").chain(args.iter().copied()))
            .expect("arguments parse")
            .into()
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_is_the_default_command() {
        assert_eq!(parse(&[]), Command::Run);
        assert_eq!(parse(&["run"]), Command::Run);
    }

    #[test]
    fn parses_doctor_and_backup_db() {
        assert_eq!(parse(&["doctor"]), Command::Doctor);
        assert_eq!(parse(&["backup", "db"]), Command::Backup(Backup::Db));
    }

    #[test]
    fn backup_requires_a_target() {
        assert!(Cli::try_parse_from(["hoster", "backup"]).is_err());
    }

    #[tokio::test]
    async fn stub_commands_report_not_implemented() {
        let err = Command::Doctor.execute().await.unwrap_err();
        assert!(err.to_string().contains("not implemented"));
    }
}
