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

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Single-binary deployment orchestrator for one VPS.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file [default: $XDG_CONFIG_HOME/hoster/hoster.toml]. Every key can be
    /// overridden by a HOSTER_* environment variable (`__` separates nested keys).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// Runs the selected command, loading the configuration where it needs one.
    async fn execute(self) -> anyhow::Result<()> {
        let config_path = self.config.clone();
        let command = Command::from(self);
        tracing::debug!(?command, "starting");
        let load = || Config::load(config_path.as_deref());
        match command {
            Command::Run => {
                let config = load()?;
                tracing::info!(
                    admin_host = %config.admin_host,
                    state_dir = %config.state_dir.display(),
                    upstream_ports = %config.upstream_ports,
                    "configuration loaded"
                );
                command.execute().await
            }
            Command::Backup(_) => {
                load()?;
                command.execute().await
            }
            Command::Doctor => command.execute().await,
        }
    }
}

/// Logging to stderr (journald under systemd): compact text, filtered by `RUST_LOG`
/// (default `info`).
struct Tracing;

impl Tracing {
    fn init() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
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
async fn main() -> ExitCode {
    let cli = Cli::parse();
    Tracing::init();
    match cli.execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` prints the cause chain on one line and never a backtrace.
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
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
    fn config_flag_is_global() {
        let cli = Cli::try_parse_from(["hoster", "run", "--config", "/etc/h.toml"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/etc/h.toml"))
        );
        let cli = Cli::try_parse_from(["hoster", "--config", "/etc/h.toml"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/etc/h.toml"))
        );
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
