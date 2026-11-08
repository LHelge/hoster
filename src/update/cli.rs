//! `hoster update` and `hoster rollback` (ADR-0052): the [`Updater`] for an admin in an SSH
//! session as the hoster user. Each command shows what it is about to do, asks for
//! confirmation (unless `--yes`) and reports the outcome; errors carry the updater's message.

use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;

use clap::Args;
use thiserror::Error;

use super::{UpdateError, UpdateOutcome, Updater};
use crate::installation::Version;

/// `hoster update`: installs a newer release and restarts the service on it.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct UpdateCommand {
    /// Update to this release instead of the latest stable one. A pre-release is installed only
    /// when named here.
    #[arg(long, value_name = "vX.Y.Z")]
    pub version: Option<Version>,
    /// Only print the running and the latest version and the release notes; change nothing.
    #[arg(long, conflicts_with = "yes")]
    pub check: bool,
    /// Update without asking for confirmation.
    #[arg(long, short)]
    pub yes: bool,
}

/// `hoster rollback`: returns to the previous installed version, undoing the database schema.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RollbackCommand {
    /// Roll back without asking for confirmation.
    #[arg(long, short)]
    pub yes: bool,
}

/// Why `hoster update` or `hoster rollback` failed.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error(
        "refusing to run as root: the binary layout and the service belong to the hoster user; \
         run this as hoster: `sudo machinectl shell hoster@` or `sudo -iu hoster`"
    )]
    Root,
    #[error("there is no terminal to confirm on (stdin is not a terminal); pass --yes")]
    NotInteractive,
    #[error("cancelled; nothing was changed")]
    Declined,
    #[error(transparent)]
    Update(#[from] UpdateError),
    #[error("cannot talk to the terminal")]
    Io(#[from] io::Error),
}

impl CommandError {
    /// The process exit code: 2 for a usage error, 1 for everything else.
    pub fn exit_code(&self) -> u8 {
        match self {
            CommandError::NotInteractive => 2,
            _ => 1,
        }
    }
}

type Result<T, E = CommandError> = std::result::Result<T, E>;

/// A user id; these commands refuse root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uid(u32);

impl Uid {
    /// This process's effective uid: the owner of `/proc/self` (no libc needed).
    pub fn current() -> io::Result<Self> {
        Ok(Self(std::fs::metadata("/proc/self")?.uid()))
    }

    /// Fails for root, with a hint how to become the hoster user.
    pub fn refuse_root(self) -> Result<()> {
        if self.0 == 0 {
            Err(CommandError::Root)
        } else {
            Ok(())
        }
    }
}

impl From<u32> for Uid {
    fn from(uid: u32) -> Self {
        Self(uid)
    }
}

/// Where the commands talk to the admin: the terminal in production, a script in tests.
pub trait Console {
    /// Prints `text` and a newline on stdout.
    fn print(&mut self, text: &dyn fmt::Display) -> io::Result<()>;
    /// Asks a yes/no `question`; anything but `y`/`yes` is no.
    fn confirm(&mut self, question: &str) -> Result<bool>;
}

/// stdout and stdin. Confirming needs stdin to be a terminal.
#[derive(Debug, Default)]
pub struct Terminal;

impl Console for Terminal {
    fn print(&mut self, text: &dyn fmt::Display) -> io::Result<()> {
        writeln!(io::stdout().lock(), "{text}")
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return Err(CommandError::NotInteractive);
        }
        let mut out = io::stdout().lock();
        write!(out, "{question} [y/N] ")?;
        out.flush()?;
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
}

impl UpdateCommand {
    /// Checks, shows the release notes, confirms, updates and restarts the service.
    pub async fn run(self, updater: &Updater, console: &mut dyn Console) -> Result<()> {
        let check = updater.check_for(self.version.as_ref()).await?;
        let label = if self.version.is_some() {
            "requested"
        } else {
            "latest"
        };
        console.print(&format_args!("running    {}", check.running))?;
        console.print(&format_args!("{label:<10} {}", check.latest))?;
        let up_to_date = match self.version {
            Some(_) => check.latest == check.running,
            None => !check.update_available(),
        };
        let downgrade = !up_to_date && check.latest < check.running;
        if self.check {
            console.print(&format_args!("\n{}", check.notes.trim_end()))?;
            let verdict = if up_to_date {
                format!("\nhoster {} is up to date", check.running)
            } else if downgrade {
                format!(
                    "\n{} is older than the running {}; update only moves forward, `hoster \
                     rollback` returns to the previous version",
                    check.latest, check.running
                )
            } else if self.version.is_some() {
                format!(
                    "\n{0} is available: run `hoster update --version {0}` to install it",
                    check.latest
                )
            } else {
                format!(
                    "\n{} is available: run `hoster update` to install it",
                    check.latest
                )
            };
            return Ok(console.print(&verdict)?);
        }
        if up_to_date {
            return Ok(console.print(&format_args!("hoster {} is up to date", check.running))?);
        }
        if downgrade {
            // Fail before asking: the updater refuses this anyway.
            return Err(UpdateError::Downgrade {
                requested: check.latest,
                running: check.running,
            }
            .into());
        }
        // Fail before asking too; the updater checks again before and after downloading.
        updater.refuse_while_deploying().await?;
        console.print(&format_args!("\n{}\n", check.notes.trim_end()))?;
        let question = format!(
            "Update hoster {} to {} and restart it?",
            check.running, check.latest
        );
        if !self.yes && !console.confirm(&question)? {
            return Err(CommandError::Declined);
        }
        // Install the release that was confirmed, even if a newer one is published meanwhile.
        match updater.update(Some(&check.latest)).await? {
            UpdateOutcome::UpToDate { running } => {
                console.print(&format_args!("hoster {running} is up to date"))?;
            }
            UpdateOutcome::Switched {
                from, to, snapshot, ..
            } => {
                console.print(&format_args!("installed {to} and switched from {from}"))?;
                if let Some(snapshot) = snapshot {
                    console.print(&format_args!(
                        "the pre-update database snapshot is {}",
                        snapshot.display()
                    ))?;
                }
                console.print(&"restarting the hoster service")?;
                updater.restart().await?;
                console.print(&format_args!(
                    "hoster {to} is starting; `hoster rollback` returns to {from}"
                ))?;
            }
        }
        Ok(())
    }
}

impl RollbackCommand {
    /// Shows what the rollback undoes, confirms and rolls back.
    pub async fn run(self, updater: &Updater, console: &mut dyn Console) -> Result<()> {
        let plan = updater.rollback_plan().await?;
        console.print(&format_args!(
            "rolling back hoster {} → {}",
            plan.from, plan.to
        ))?;
        match (plan.database, plan.migrations_undone()) {
            (Some(database), undone @ 1..) => console.print(&format_args!(
                "{undone} database migration(s) will be undone (schema {database} → {}): data \
                 that only {} can hold is lost. The database is snapshotted first.",
                plan.target, plan.from
            ))?,
            _ => console.print(&"no database migrations need to be undone")?,
        }
        console.print(&format_args!(
            "the hoster service is stopped meanwhile and started again on {}",
            plan.to
        ))?;
        let question = format!("Roll back to {}?", plan.to);
        if !self.yes && !console.confirm(&question)? {
            return Err(CommandError::Declined);
        }
        let outcome = updater.rollback().await?;
        console.print(&format_args!(
            "rolled back to {}; the database is at schema {}",
            outcome.to, outcome.schema
        ))?;
        if let Some(snapshot) = outcome.snapshot {
            console.print(&format_args!(
                "the pre-rollback database snapshot is {}",
                snapshot.display()
            ))?;
        }
        Ok(())
    }
}
