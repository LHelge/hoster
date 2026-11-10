//! hoster: a single-binary deployment orchestrator for one VPS (ADR-0001).
//!
//! `main` parses the command line, builds the orchestrator runtime and hands `run` to the
//! [`Supervisor`](supervisor::Supervisor), which owns the startup and shutdown order
//! (docs/architecture.md §4). Errors are `anyhow` at this edge; the modules below use their own
//! `thiserror` types.

mod acme;
mod config;
mod doctor;
mod health;
mod installation;
mod logging;
mod orchestrator;
mod ports;
mod proxy;
mod registry;
mod routing;
mod runtime;
mod secrets;
mod stack;
mod stats;
mod store;
mod supervisor;
mod update;
mod web;

#[cfg(test)]
mod e2e_tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{Config, ConfigError};
use crate::doctor::Doctor;
use crate::installation::BuildInfo;
use crate::logging::{LogFormat, Logging};
use crate::stack::{StackCheck, StackSchema};
use crate::store::Store;
use crate::supervisor::Supervisor;
use crate::update::{CommandError, RollbackCommand, Terminal, Uid, UpdateCommand, Updater};

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
    /// Runs the selected command with the configuration it names. Logging starts once the
    /// configuration is loaded, in its `log.format` (text when it did not load).
    async fn execute(self) -> anyhow::Result<()> {
        let config_path = self.config.clone();
        let command = Command::from(self);
        let config = Config::load(config_path.as_deref());
        let format = config
            .as_ref()
            .map_or(LogFormat::default(), |config| config.log.format);
        Logging::from(format).init();
        tracing::debug!(?command, "starting");
        command.execute(config, config_path.as_deref()).await
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
    /// Print the version, target triple and database schema of this binary. Needs no
    /// configuration.
    Version {
        /// Print `{"version", "target", "schema"}` as JSON on stdout (read by `hoster rollback`).
        #[arg(long)]
        json: bool,
    },
    /// Update hoster to the latest stable release (or the one named) and restart the service.
    /// Run as the hoster user; refused while a deployment is in progress.
    Update(UpdateCommand),
    /// Return to the previous installed version, undoing the database migrations it does not
    /// know (their data is lost). Run as the hoster user.
    Rollback(RollbackCommand),
    /// Work with stack files. Needs no configuration.
    #[command(subcommand)]
    Stack(StackCommand),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum StackCommand {
    /// Print the JSON Schema of the stack file format, for editors with TOML schema support
    /// (published at https://docs.hoster.lhelge.se/stack.schema.json).
    Schema,
    /// Parse and validate stack files offline, e.g. in an app repository's CI. Prints every
    /// error and warning with file:line:column and field path, and exits non-zero on errors.
    /// Checks that need the server (routes of other stacks, the admin host, secrets, registry
    /// tags, cgroups) are skipped.
    Check {
        /// The stack files to check.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: CheckFormat,
        /// Exit non-zero on warnings too.
        #[arg(long)]
        deny_warnings: bool,
    },
}

/// How `hoster stack check` prints its result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum CheckFormat {
    /// `file:line:column: severity: path: message` lines and a summary per file.
    #[default]
    Text,
    /// One JSON object on stdout: `{ok, errors, warnings, files, skipped}`.
    Json,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Backup {
    /// Write a consistent snapshot of the SQLite state store, safe while hoster runs
    /// (`VACUUM INTO`). Back up the secret-store key separately; see https://docs.hoster.lhelge.se/wiki/backup/.
    Db {
        /// The snapshot file to create; it must not exist yet.
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

impl Command {
    /// Runs the command. `config` is the outcome of loading the configuration from
    /// `config_path`: `doctor` reports a failure as one of its findings, `update` and `rollback`
    /// read only `state_dir` from `config_path` (they run outside the service's environment),
    /// every other command needs it to have loaded.
    async fn execute(
        self,
        config: Result<Config, ConfigError>,
        config_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        match self {
            Command::Version { json } => {
                println!("{}", Self::version(json)?);
                Ok(())
            }
            Command::Stack(StackCommand::Schema) => Ok(Self::print(StackSchema::get().as_ref())?),
            Command::Stack(StackCommand::Check {
                files,
                format,
                deny_warnings,
            }) => {
                let check = StackCheck::files(&files).deny_warnings(deny_warnings);
                Self::print(&match format {
                    CheckFormat::Text => check.to_string(),
                    CheckFormat::Json => serde_json::to_string_pretty(&check)? + "\n",
                })?;
                Ok(check.verdict()?)
            }
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
            Command::Backup(Backup::Db { file }) => {
                let store = Store::open_read_only(&config?.state_dir).await?;
                let result = store.snapshot(&file).await;
                store.close().await;
                result?;
                println!(
                    "wrote a snapshot of {} to {}",
                    store.path().display(),
                    file.display()
                );
                Ok(())
            }
            Command::Update(command) => {
                let updater = Self::updater(config_path)?;
                Ok(command.run(&updater, &mut Terminal).await?)
            }
            Command::Rollback(command) => {
                let updater = Self::updater(config_path)?;
                Ok(command.run(&updater, &mut Terminal).await?)
            }
        }
    }

    /// Writes `text` to stdout. Unlike `print!`, it does not panic when the reader goes away
    /// early (`… | head`): a closed pipe is not an error, so the caller's verdict still decides
    /// the exit code.
    fn print(text: &str) -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        match stdout
            .write_all(text.as_bytes())
            .and_then(|()| stdout.flush())
        {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e),
            _ => Ok(()),
        }
    }

    /// The updater for `hoster update` / `hoster rollback`, refusing root first. Only
    /// `state_dir` is read from the configuration: an SSH session as hoster has no
    /// `HOSTER_ADMIN_GITHUB` (the unit's drop-in sets it), and these commands do not need it.
    fn updater(config_path: Option<&Path>) -> anyhow::Result<Updater> {
        Uid::current()?.refuse_root()?;
        Ok(Updater::for_host(&Config::load_state_dir(config_path)?)?)
    }

    /// What `hoster version [--json]` prints.
    fn version(json: bool) -> serde_json::Result<String> {
        let info = BuildInfo::running();
        if json {
            serde_json::to_string(&info)
        } else {
            Ok(info.to_string())
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
    let result = OrchestratorRuntime::build()
        .map_err(|err| anyhow::Error::new(err).context("building the tokio runtime"))
        .and_then(|runtime| runtime.block_on(cli.execute()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` prints the cause chain on one line and never a backtrace.
            eprintln!("error: {err:#}");
            err.downcast_ref::<CommandError>()
                .map_or(ExitCode::FAILURE, |err| err.exit_code().into())
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
        assert_eq!(
            parse(&["backup", "db", "/tmp/snap.db"]),
            Command::Backup(Backup::Db {
                file: "/tmp/snap.db".into()
            })
        );
        assert!(Cli::try_parse_from(["hoster", "backup", "db"]).is_err());
    }

    #[test]
    fn parses_version_and_keeps_the_version_flag() {
        assert_eq!(parse(&["version"]), Command::Version { json: false });
        assert_eq!(
            parse(&["version", "--json"]),
            Command::Version { json: true }
        );
        for flag in ["--version", "-V"] {
            let err = Cli::try_parse_from(["hoster", flag]).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
            assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")), "{err}");
        }
    }

    #[test]
    fn version_json_reports_version_target_and_schema() {
        let json: serde_json::Value =
            serde_json::from_str(&Command::version(true).unwrap()).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            ["schema", "target", "version"]
        );
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["target"], env!("BUILD_TARGET_TRIPLE"));
        assert_eq!(json["schema"], i64::from(Store::expected_schema_version()));
        let text = Command::version(false).unwrap();
        assert!(
            text.starts_with(concat!("hoster ", env!("CARGO_PKG_VERSION"))),
            "{text}"
        );
    }

    #[tokio::test]
    async fn version_needs_no_config() {
        Command::Version { json: true }
            .execute(Err(ConfigError::NoConfigDir), None)
            .await
            .unwrap();
    }

    #[test]
    fn parses_update_and_rollback() {
        let update = |version: Option<&str>, check, yes| {
            Command::Update(UpdateCommand {
                version: version.map(|v| v.parse().unwrap()),
                check,
                yes,
            })
        };
        assert_eq!(parse(&["update"]), update(None, false, false));
        assert_eq!(parse(&["update", "--check"]), update(None, true, false));
        assert_eq!(parse(&["update", "-y"]), update(None, false, true));
        assert_eq!(
            parse(&["update", "--version", "v0.2.0", "--yes"]),
            update(Some("0.2.0"), false, true)
        );
        assert_eq!(
            parse(&["update", "--version", "0.3.0-rc.1", "--check"]),
            update(Some("v0.3.0-rc.1"), true, false)
        );
        assert_eq!(
            parse(&["rollback"]),
            Command::Rollback(RollbackCommand { yes: false })
        );
        assert_eq!(
            parse(&["rollback", "--yes"]),
            Command::Rollback(RollbackCommand { yes: true })
        );
    }

    #[test]
    fn update_and_rollback_usage_errors_exit_2() {
        for args in [
            &["update", "--version", "latest"][..],
            &["update", "--version"],
            &["update", "--check", "--yes"],
            &["rollback", "--version", "v0.1.0"],
        ] {
            let err = Cli::try_parse_from(std::iter::once("hoster").chain(args.iter().copied()))
                .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}: {err}");
        }
    }

    #[tokio::test]
    async fn update_and_rollback_need_a_config_file() {
        if Uid::current().unwrap().refuse_root().is_err() {
            return; // Running the suite as root: the root check rightly comes first.
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("hoster.toml");
        let update = Command::Update(UpdateCommand {
            version: None,
            check: true,
            yes: false,
        });
        for command in [update, Command::Rollback(RollbackCommand { yes: true })] {
            // A config that failed to load (no HOSTER_ADMIN_GITHUB in an SSH session) is not
            // what stops them; only a missing file is.
            let err = command
                .execute(Err(ConfigError::NoConfigDir), Some(&missing))
                .await
                .unwrap_err();
            assert!(
                matches!(err.downcast_ref(), Some(ConfigError::NotFound(path)) if *path == missing),
                "{err}"
            );
        }
    }

    #[test]
    fn parses_stack_schema() {
        assert_eq!(
            parse(&["stack", "schema"]),
            Command::Stack(StackCommand::Schema)
        );
        assert!(Cli::try_parse_from(["hoster", "stack"]).is_err());
    }

    #[test]
    fn parses_stack_check() {
        assert_eq!(
            parse(&["stack", "check", "a.toml", "b.toml"]),
            Command::Stack(StackCommand::Check {
                files: vec!["a.toml".into(), "b.toml".into()],
                format: CheckFormat::Text,
                deny_warnings: false,
            })
        );
        assert_eq!(
            parse(&[
                "stack",
                "check",
                "--format",
                "json",
                "--deny-warnings",
                "a.toml"
            ]),
            Command::Stack(StackCommand::Check {
                files: vec!["a.toml".into()],
                format: CheckFormat::Json,
                deny_warnings: true,
            })
        );
        for args in [
            &["stack", "check"][..],
            &["stack", "check", "--format", "yaml", "a.toml"],
        ] {
            let err = Cli::try_parse_from(std::iter::once("hoster").chain(args.iter().copied()))
                .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}: {err}");
        }
    }

    #[tokio::test]
    async fn stack_check_needs_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo.stack.toml");
        std::fs::write(
            &file,
            "version = 1\nname = \"demo\"\n[services.app]\nimage = \"ghcr.io/acme/app\"\n",
        )
        .unwrap();
        Command::Stack(StackCommand::Check {
            files: vec![file],
            format: CheckFormat::Json,
            deny_warnings: true,
        })
        .execute(Err(ConfigError::NoConfigDir), None)
        .await
        .expect("checks a valid file without a config");
    }

    #[tokio::test]
    async fn stack_schema_needs_no_config() {
        Command::Stack(StackCommand::Schema)
            .execute(Err(ConfigError::NoConfigDir), None)
            .await
            .expect("prints the schema without a config");
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
        let backup = Command::Backup(Backup::Db {
            file: "snap.db".into(),
        });
        for command in [Command::Run, backup] {
            let err = command
                .execute(Err(ConfigError::NoConfigDir), None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("hoster.toml"), "{err}");
        }
    }
}
