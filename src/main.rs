//! hoster: a single-binary deployment orchestrator for one VPS (ADR-0001).
//!
//! `main` parses the command line, builds the orchestrator runtime and hands `run` to the
//! [`Supervisor`](supervisor::Supervisor), which owns the startup and shutdown order
//! (docs/architecture.md §4). Errors are `anyhow` at this edge; the modules below use their own
//! `thiserror` types.

mod acme;
mod config;
mod doctor;
mod orchestrator;
mod ports;
mod proxy;
mod registry;
mod routing;
mod runtime;
mod stack;
mod store;
mod supervisor;
mod web;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, ConfigError};
use crate::doctor::Doctor;
use crate::supervisor::Supervisor;

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
    /// Runs the selected command with the configuration it names.
    async fn execute(self) -> anyhow::Result<()> {
        let config_path = self.config.clone();
        let command = Command::from(self);
        tracing::debug!(?command, "starting");
        command.execute(Config::load(config_path.as_deref())).await
    }
}

/// Logging to stderr (journald under systemd): compact text, filtered by `RUST_LOG`
/// (default `info`), coloured only when stderr is a terminal.
struct Tracing;

impl Tracing {
    fn init() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(filter)
            .with_ansi(std::io::stderr().is_terminal())
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
    /// Check that the host is set up correctly for hoster (run as the hoster user, not root).
    /// Exits non-zero when a required check fails.
    Doctor {
        /// Print the report as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
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
    /// Runs the command. `config` is the outcome of loading the configuration: `doctor` reports
    /// a failure as one of its findings, every other command needs it to have loaded.
    async fn execute(self, config: Result<Config, ConfigError>) -> anyhow::Result<()> {
        match self {
            Command::Run => Supervisor::new(config?).run().await,
            Command::Doctor { json } => {
                let report = Doctor::from_env()?.diagnose(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!("{report}");
                }
                if !report.ok() {
                    anyhow::bail!(
                        "host is not ready for hoster: {} required check(s) failed",
                        report.failures()
                    );
                }
                Ok(())
            }
            Command::Backup(Backup::Db) => {
                config?;
                anyhow::bail!("`hoster backup db` is not implemented yet")
            }
        }
    }
}

/// The orchestrator runtime (architecture §3.1): multi-threaded, shared by the supervisor,
/// orchestrator, deploy tasks, ACME, poller and web app. Pingora gets its own thread and runtimes.
struct OrchestratorRuntime;

impl OrchestratorRuntime {
    fn build() -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("hoster-orch")
            .enable_all()
            .build()
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    Tracing::init();
    let result = OrchestratorRuntime::build()
        .map_err(|err| anyhow::Error::new(err).context("building the tokio runtime"))
        .and_then(|runtime| runtime.block_on(cli.execute()));
    match result {
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
        assert_eq!(parse(&["doctor"]), Command::Doctor { json: false });
        assert_eq!(parse(&["doctor", "--json"]), Command::Doctor { json: true });
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
    async fn daemon_commands_need_a_loaded_config() {
        for command in [Command::Run, Command::Backup(Backup::Db)] {
            let err = command
                .execute(Err(ConfigError::NoConfigDir))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("hoster.toml"), "{err}");
        }
    }
}
