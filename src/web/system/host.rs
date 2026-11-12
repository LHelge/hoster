//! The facts about the machine the System page shows: hostname, OS, kernel, architecture and
//! CPUs, memory, the container runtime and its cgroups, hoster's public addresses, the data
//! directory (and its filesystem's usage when that is not podman's, ADR-0066 §5) and the host's
//! uptime, and how many of its open files hoster uses (ADR-0065).
//!
//! [`HostFacts`] reads them when the page renders. [`LocalHost`] reads them from the files the
//! kernel and the distribution publish (`/proc/sys/kernel/hostname`, `/etc/os-release`,
//! `/proc/meminfo`, `/proc/uptime`, …), the runtime's own description and the DNS check's public
//! addresses; tests use a fixed [`Facts`]. Nothing here fails: a fact that cannot be read is
//! `None` and shown as unknown.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::acme::DnsCheck;
use crate::machine::{Machine, Memory};
use crate::proxy::{Admission, FileUse};
use crate::runtime::EngineInfo;
use crate::stats::Usage;
use crate::web::containers::ContainerSource;
use crate::web::ui::{ByteUsage, Uptime};

/// Where the System page's host facts come from.
#[async_trait::async_trait]
pub trait HostFacts: Send + Sync + 'static {
    /// The facts as they are now; the parts that cannot be read are `None`.
    async fn read(&self) -> Facts;
}

/// What is known about the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub hostname: Option<String>,
    /// The distribution's `PRETTY_NAME` (`Debian GNU/Linux 13 (trixie)`).
    pub os: Option<String>,
    /// The kernel release (`6.12.48-amd64`).
    pub kernel: Option<String>,
    /// The CPU architecture (`x86_64`).
    pub arch: Option<String>,
    /// The CPUs the kernel has online.
    pub cpus: Option<usize>,
    /// Total memory, in bytes.
    pub memory: Option<u64>,
    /// The container engine; `None` when it did not answer.
    pub engine: Option<EngineInfo>,
    /// The cgroups version the kernel mounts, when the engine does not say.
    pub cgroups: Option<String>,
    /// hoster's public addresses as the DNS check knows them; empty when unknown.
    pub public: Vec<IpAddr>,
    /// Where the public addresses come from (`acme.public_addresses in hoster.toml`).
    pub public_source: Option<String>,
    /// hoster's state directory.
    pub data_dir: Option<PathBuf>,
    /// The usage of the state directory's filesystem, when it is not the one podman's storage
    /// root is on (whose usage the sidebar shows, ADR-0066 §5); `None` when it is the same one
    /// or cannot be told.
    pub data_disk: Option<Usage>,
    /// How long the host has been up.
    pub uptime: Option<Duration>,
    /// hoster's own descriptors and the client connections refused at the cap.
    pub files: Option<FileUse>,
}

/// One row of the host card: a name and its value, or `None` for "unknown".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub name: &'static str,
    pub value: Option<String>,
    /// Where the value comes from, shown after it.
    pub note: Option<String>,
}

impl Fact {
    fn new(name: &'static str, value: Option<String>) -> Self {
        Self {
            name,
            value,
            note: None,
        }
    }
}

impl Facts {
    /// A Debian VPS with rootless podman (tests and the UI preview).
    #[cfg(test)]
    pub fn sample() -> Self {
        Facts {
            hostname: Some("srv-01".into()),
            os: Some("Debian GNU/Linux 13 (trixie)".into()),
            kernel: Some("6.12.48-amd64".into()),
            arch: Some("x86_64".into()),
            cpus: Some(8),
            memory: Some(32 * 1024 * 1024 * 1024),
            engine: Some(EngineInfo {
                version: Some("5.4.2".into()),
                rootless: Some(true),
                cgroup_version: Some("2".into()),
                cgroup_driver: Some("systemd".into()),
                storage_root: Some("/home/hoster/.local/share/containers/storage".into()),
            }),
            cgroups: Some("2".into()),
            public: vec![
                "203.0.113.42".parse().unwrap(),
                "2001:db8::42".parse().unwrap(),
            ],
            public_source: Some("the A/AAAA records of admin.example.com".into()),
            data_dir: Some("/home/hoster/.local/state/hoster".into()),
            data_disk: None,
            uptime: Some(Duration::from_secs(41 * 86_400 + 7 * 3_600)),
            files: Some(FileUse {
                open: Some(214),
                limit: crate::proxy::FileLimit {
                    soft: Some(16_384),
                    hard: Some(524_288),
                },
                cap: 4096,
                refused: 0,
            }),
        }
    }

    /// The rows of the host card, in the design's order.
    pub fn rows(&self) -> Vec<Fact> {
        let arch = match (&self.arch, self.cpus) {
            (Some(arch), Some(cpus)) => Some(format!("{arch} · {cpus} CPU")),
            (Some(arch), None) => Some(arch.clone()),
            (None, Some(cpus)) => Some(format!("{cpus} CPU")),
            (None, None) => None,
        };
        let public = (!self.public.is_empty()).then(|| {
            self.public
                .iter()
                .map(IpAddr::to_string)
                .collect::<Vec<_>>()
                .join(" · ")
        });
        let mut rows = vec![
            Fact::new("hostname", self.hostname.clone()),
            Fact::new("os", self.os.clone()),
            Fact::new("kernel", self.kernel.clone()),
            Fact::new("arch", arch),
            Fact::new("memory", self.memory.map(|b| Memory(b).to_string())),
            Fact::new("runtime", self.engine.as_ref().map(Self::runtime)),
            Fact::new("cgroups", self.cgroups()),
            Fact {
                note: public.as_ref().and(self.public_source.clone()),
                ..Fact::new("public ip", public)
            },
            Fact::new(
                "data dir",
                self.data_dir.as_ref().map(|d| d.display().to_string()),
            ),
        ];
        if let Some(disk) = self.data_disk {
            rows.push(Fact {
                note: Some("its own filesystem; the containers' is in the sidebar".into()),
                ..Fact::new("data disk", Some(format!("{} used", ByteUsage(disk))))
            });
        }
        rows.extend([
            Fact::new("uptime", self.uptime.map(|up| Uptime(up).to_string())),
            Fact {
                note: self.files.map(Self::cap),
                ..Fact::new("open files", self.files.and_then(Self::open_files))
            },
            Fact::new(
                "refused",
                self.files
                    .map(|files| format!("{} client connections since start", files.refused)),
            ),
        ]);
        rows
    }

    /// `214 of 16384`.
    fn open_files(files: FileUse) -> Option<String> {
        let open = files.open?;
        Some(match files.limit.soft {
            Some(limit) => format!("{open} of {limit}"),
            None => open.to_string(),
        })
    }

    /// `new clients refused from 4096`.
    fn cap(files: FileUse) -> String {
        format!("new clients refused from {}", files.cap)
    }

    /// `podman 5.4.2 · rootless`.
    fn runtime(engine: &EngineInfo) -> String {
        let version = engine.version.as_deref().unwrap_or("version unknown");
        match engine.rootless {
            Some(true) => format!("podman {version} · rootless"),
            Some(false) => format!("podman {version} · not rootless"),
            None => format!("podman {version}"),
        }
    }

    /// `v2 · systemd`, from the engine, else from the kernel's mount.
    fn cgroups(&self) -> Option<String> {
        let engine = self.engine.as_ref();
        let version = engine
            .and_then(|e| e.cgroup_version.clone())
            .or_else(|| self.cgroups.clone());
        let driver = engine.and_then(|e| e.cgroup_driver.clone());
        match (version, driver) {
            (Some(version), Some(driver)) => Some(format!("v{version} · {driver}")),
            (Some(version), None) => Some(format!("v{version}")),
            (None, Some(driver)) => Some(driver),
            (None, None) => None,
        }
    }
}

#[async_trait::async_trait]
impl HostFacts for Facts {
    /// Always these facts (tests and the UI preview).
    async fn read(&self) -> Facts {
        self.clone()
    }
}

/// This machine, as its kernel, distribution and container runtime describe it.
pub struct LocalHost {
    /// `/`, or a directory standing in for it in tests.
    root: PathBuf,
    /// Memory and CPUs, read as the orchestrator reads them for resource limits.
    machine: Machine,
    runtime: Arc<dyn ContainerSource>,
    dns: DnsCheck,
    data_dir: PathBuf,
    /// The proxy's connection cap, whose descriptor use is shown; `None` shows none.
    admission: Option<Admission>,
}

impl LocalHost {
    /// How long the page waits for the runtime to describe itself.
    const ENGINE_TIMEOUT: Duration = Duration::from_secs(2);

    /// The host with its container `runtime`, the DNS check's public addresses and hoster's
    /// state directory `data_dir`.
    pub fn new(runtime: Arc<dyn ContainerSource>, dns: DnsCheck, data_dir: PathBuf) -> Self {
        Self {
            root: PathBuf::from("/"),
            machine: Machine::local(),
            runtime,
            dns,
            data_dir,
            admission: None,
        }
    }

    /// The same host, showing the descriptor use of `admission`.
    pub fn with_admission(mut self, admission: Admission) -> Self {
        self.admission = Some(admission);
        self
    }

    /// The same host with its `/etc`, `/proc` and `/sys` read under `root` (tests).
    #[cfg(test)]
    pub fn under(mut self, root: &std::path::Path) -> Self {
        self.root = root.to_path_buf();
        self.machine = Machine::under(root, 1000);
        self
    }

    /// A file under the root, trimmed; `None` when missing or empty.
    fn file(&self, path: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.root.join(path)).ok()?;
        Some(text.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// `PRETTY_NAME` from os-release(5), falling back to `/usr/lib/os-release`.
    fn os(&self) -> Option<String> {
        ["etc/os-release", "usr/lib/os-release"]
            .iter()
            .find_map(|path| self.file(path))
            .and_then(|text| OsRelease(&text).pretty_name())
    }

    /// The first number of `/proc/uptime`.
    fn uptime(&self) -> Option<Duration> {
        let text = self.file("proc/uptime")?;
        let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
        Duration::try_from_secs_f64(secs).ok()
    }

    /// `2` when the unified hierarchy is mounted, `1` when only the legacy one is.
    fn cgroups(&self) -> Option<String> {
        let cgroup = self.root.join("sys/fs/cgroup");
        if cgroup.join("cgroup.controllers").exists() {
            Some("2".into())
        } else if cgroup.is_dir() {
            Some("1".into())
        } else {
            None
        }
    }

    /// The usage of the state directory's filesystem when it is not the one of `engine`'s
    /// storage root.
    fn data_disk(&self, engine: Option<&EngineInfo>) -> Option<Usage> {
        let storage_root = engine?.storage_root.as_deref()?;
        if self.machine.same_filesystem(storage_root, &self.data_dir)? {
            return None;
        }
        self.machine.filesystem(&self.data_dir).map(Usage::from)
    }

    async fn engine(&self) -> Option<EngineInfo> {
        match tokio::time::timeout(Self::ENGINE_TIMEOUT, self.runtime.engine()).await {
            Ok(Ok(engine)) => Some(engine),
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "reading the container engine's info failed");
                None
            }
            Err(_) => {
                tracing::warn!("the container engine did not describe itself in time");
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl HostFacts for LocalHost {
    async fn read(&self) -> Facts {
        let public: Vec<IpAddr> = self.dns.expected().into_iter().collect();
        let engine = self.engine().await;
        Facts {
            hostname: self.file("proc/sys/kernel/hostname"),
            os: self.os(),
            kernel: self.file("proc/sys/kernel/osrelease"),
            arch: Some(std::env::consts::ARCH.to_owned()),
            cpus: self.machine.cpus(),
            memory: self.machine.memory(),
            data_disk: self.data_disk(engine.as_ref()),
            engine,
            cgroups: self.cgroups(),
            public_source: (!public.is_empty()).then(|| self.dns.source().to_string()),
            public,
            data_dir: Some(self.data_dir.clone()),
            uptime: self.uptime(),
            files: self.admission.as_ref().map(Admission::usage),
        }
    }
}

/// The text of an os-release(5) file.
struct OsRelease<'a>(&'a str);

impl OsRelease<'_> {
    /// `PRETTY_NAME`, unquoted, or `NAME` when there is none.
    fn pretty_name(&self) -> Option<String> {
        self.value("PRETTY_NAME").or_else(|| self.value("NAME"))
    }

    fn value(&self, key: &str) -> Option<String> {
        let line = self
            .0
            .lines()
            .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))?;
        let value = line.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        Some(unquoted.replace("\\\"", "\"")).filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;
    use crate::acme::PublicAddresses;
    use crate::runtime::{FakeRuntime, Operation, RuntimeError};

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn dns(addresses: &[&str]) -> DnsCheck {
        DnsCheck::from(PublicAddresses::Configured(
            addresses.iter().map(|a| a.parse().unwrap()).collect(),
        ))
    }

    #[test]
    fn os_release_values_are_unquoted() {
        let text = "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";
        assert_eq!(
            OsRelease(text).pretty_name().as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(
            OsRelease("NAME='Arch Linux'\n").pretty_name().as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(OsRelease("PRETTY_NAME=\n").pretty_name(), None);
    }

    #[tokio::test]
    async fn reads_the_host_from_its_files_and_the_runtime() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "proc/sys/kernel/hostname", "srv-01\n");
        write(root, "proc/sys/kernel/osrelease", "6.12.48-amd64\n");
        write(
            root,
            "etc/os-release",
            "ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n",
        );
        write(
            root,
            "proc/meminfo",
            "MemTotal:       32768000 kB\nMemFree:  1 kB\n",
        );
        write(root, "proc/uptime", "3567310.52 7000000.00\n");
        write(root, "sys/fs/cgroup/cgroup.controllers", "cpu memory\n");
        write(root, "sys/devices/system/cpu/online", "0-7\n");
        let host = LocalHost::new(
            Arc::new(FakeRuntime::new()),
            dns(&["203.0.113.42", "2001:db8::42"]),
            "/home/hoster/.local/state/hoster".into(),
        )
        .with_admission(Admission::new(4096))
        .under(root);
        let facts = host.read().await;
        assert_eq!(facts.hostname.as_deref(), Some("srv-01"));
        assert_eq!(facts.memory, Some(32_768_000 * 1024));
        assert_eq!(facts.uptime, Some(Duration::from_secs_f64(3_567_310.52)));
        let rows: Vec<(&str, Option<String>)> = facts
            .rows()
            .into_iter()
            .map(|f| (f.name, f.value))
            .collect();
        let value = |name: &str| {
            rows.iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{name}: {rows:?}"))
        };
        assert_eq!(value("os"), "Debian GNU/Linux 13 (trixie)");
        assert_eq!(value("kernel"), "6.12.48-amd64");
        assert_eq!(value("arch"), format!("{} · 8 CPU", std::env::consts::ARCH));
        assert_eq!(value("memory"), "31.2 GiB");
        assert_eq!(value("runtime"), "podman 5.4.2 · rootless");
        assert_eq!(value("cgroups"), "v2 · systemd");
        assert_eq!(value("public ip"), "203.0.113.42 · 2001:db8::42");
        assert_eq!(value("data dir"), "/home/hoster/.local/state/hoster");
        assert_eq!(value("uptime"), "41d 6h");
        // hoster's own descriptors, from the real process.
        let soft = crate::proxy::FileLimit::current().soft.unwrap();
        assert!(
            value("open files").ends_with(&format!(" of {soft}")),
            "{rows:?}"
        );
        assert_eq!(value("refused"), "0 client connections since start");
        let files = facts.rows().into_iter().find(|f| f.name == "open files");
        assert_eq!(
            files.unwrap().note.as_deref(),
            Some("new clients refused from 4096")
        );
        let public = facts.rows().into_iter().find(|f| f.name == "public ip");
        assert_eq!(
            public.unwrap().note.as_deref(),
            Some("acme.public_addresses in hoster.toml")
        );
    }

    #[tokio::test]
    async fn what_cannot_be_read_is_unknown_and_nothing_fails() {
        let root = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::new();
        runtime.fail_always(
            Operation::Engine,
            RuntimeError::Unreachable {
                socket: "podman.sock".into(),
                message: "gone".into(),
            },
        );
        let host = LocalHost::new(
            Arc::new(runtime),
            DnsCheck::from(PublicAddresses::OfHost("admin.example.com".into())),
            "/state".into(),
        )
        .under(root.path());
        let facts = host.read().await;
        assert_eq!(facts.engine, None);
        let unknown: BTreeSet<&str> = facts
            .rows()
            .into_iter()
            .filter(|f| f.value.is_none())
            .map(|f| f.name)
            .collect();
        assert_eq!(
            unknown,
            BTreeSet::from([
                "hostname",
                "os",
                "kernel",
                "memory",
                "runtime",
                "cgroups",
                "public ip",
                "uptime",
                // No proxy to report on.
                "open files",
                "refused",
            ])
        );
    }

    #[tokio::test]
    async fn the_data_dirs_filesystem_is_shown_only_when_it_is_not_podmans() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let runtime = Arc::new(FakeRuntime::new());
        let host = |runtime: &Arc<FakeRuntime>| {
            LocalHost::new(
                runtime.clone(),
                dns(&["203.0.113.42"]),
                data.path().to_path_buf(),
            )
            .under(root.path())
        };
        let disk = |facts: &Facts| facts.rows().into_iter().find(|f| f.name == "data disk");

        // The usual VPS: the storage root and the state dir share a filesystem.
        runtime.set_storage_root(data.path().join("containers"));
        std::fs::create_dir(data.path().join("containers")).unwrap();
        let facts = host(&runtime).read().await;
        assert_eq!(facts.data_disk, None);
        assert_eq!(disk(&facts), None);

        // On another filesystem (procfs stands in for one).
        runtime.set_storage_root("/proc");
        let facts = host(&runtime).read().await;
        let usage = facts.data_disk.expect("the state dir's filesystem");
        assert!(usage.total > 0 && usage.used <= usage.total, "{usage:?}");
        let fact = disk(&facts).unwrap();
        assert!(fact.value.unwrap().ends_with(" used"));
        assert_eq!(
            fact.note.as_deref(),
            Some("its own filesystem; the containers' is in the sidebar")
        );

        // A storage root that cannot be read says nothing either way.
        runtime.set_storage_root(root.path().join("missing"));
        assert_eq!(host(&runtime).read().await.data_disk, None);
    }

    #[test]
    fn a_data_disk_row_follows_the_data_dir() {
        let facts = Facts {
            data_disk: Some(Usage {
                used: 3 * 1024 * 1024 * 1024,
                total: 40 * 1024 * 1024 * 1024,
            }),
            ..Facts::sample()
        };
        let names: Vec<&str> = facts.rows().iter().map(|f| f.name).collect();
        let at = names.iter().position(|n| *n == "data dir").unwrap();
        assert_eq!(names[at + 1], "data disk", "{names:?}");
        assert_eq!(
            facts.rows()[at + 1].value.as_deref(),
            Some("3.0 / 40.0 GiB used")
        );
    }

    #[test]
    fn the_engine_cgroups_win_over_the_mount() {
        let facts = Facts {
            engine: Some(EngineInfo {
                rootless: Some(false),
                cgroup_driver: Some("cgroupfs".into()),
                ..EngineInfo::default()
            }),
            cgroups: Some("1".into()),
            ..Facts::default()
        };
        let rows = facts.rows();
        let value = |name| rows.iter().find(|f| f.name == name).unwrap().value.clone();
        assert_eq!(
            value("runtime").as_deref(),
            Some("podman version unknown · not rootless")
        );
        assert_eq!(value("cgroups").as_deref(), Some("v1 · cgroupfs"));
        assert_eq!(Memory(512 * 1024 * 1024).to_string(), "512 MiB");
    }
}
