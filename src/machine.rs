//! The machine hoster runs on, as its kernel and systemd describe it: total memory, the online
//! CPUs and the cgroup controllers hoster's user manager delegates (ADR-0059 §5).
//!
//! [`Machine`] reads them from `/proc` and `/sys`. The orchestrator asks it whether a stack's
//! resource limits can be enforced ([`Machine::check_limits`]) and whether they exceed the host
//! ([`Machine::limit_warnings`]); the System page shows its memory and CPUs, and `hoster doctor`
//! reads the same [`CgroupControllers`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::stack::{Counted, Limit, Name, Stack};

/// The machine, read from the files under a root (`/`, or a directory standing in for it in
/// tests).
#[derive(Debug, Clone)]
pub struct Machine {
    root: PathBuf,
    /// The user hoster runs as, whose user manager's cgroup the containers land in.
    uid: u32,
}

impl Default for Machine {
    fn default() -> Self {
        Self::local()
    }
}

impl Machine {
    /// This machine, for this process's user.
    pub fn local() -> Self {
        Self {
            root: PathBuf::from("/"),
            uid: rustix::process::getuid().as_raw(),
        }
    }

    /// A machine whose `/proc` and `/sys` are under `root`, for user `uid` (tests).
    #[cfg(test)]
    pub fn under(root: &Path, uid: u32) -> Self {
        Self {
            root: root.to_path_buf(),
            uid,
        }
    }

    /// A file under the root, trimmed; `None` when missing or empty.
    fn file(&self, path: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.root.join(path)).ok()?;
        Some(text.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// `MemTotal` from `/proc/meminfo`, in bytes.
    pub fn memory(&self) -> Option<u64> {
        MemInfo(&self.file("proc/meminfo")?).bytes("MemTotal")
    }

    /// `MemTotal` and `MemAvailable` from `/proc/meminfo`; `None` when either cannot be read.
    #[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
    pub fn memory_usage(&self) -> Option<MemoryUsage> {
        let text = self.file("proc/meminfo")?;
        let info = MemInfo(&text);
        Some(MemoryUsage {
            total: info.bytes("MemTotal")?,
            available: info.bytes("MemAvailable")?,
        })
    }

    /// The CPU time the host has spent since boot, from the aggregate `cpu` line of
    /// `/proc/stat`.
    #[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
    pub fn cpu_times(&self) -> Option<CpuTimes> {
        self.file("proc/stat")?.parse().ok()
    }

    /// The size of the filesystem holding `path`, and what of it an unprivileged user may
    /// still write, from `statvfs`. `path` is a real absolute path, not one under the root.
    #[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
    pub fn filesystem(&self, path: &Path) -> Option<FilesystemUsage> {
        rustix::fs::statvfs(path).ok()?.try_into().ok()
    }

    /// The online CPUs from `/sys/devices/system/cpu/online`; when that cannot be read, the CPUs
    /// hoster may run on (which a cgroup quota or affinity mask can make fewer).
    pub fn cpus(&self) -> Option<usize> {
        self.file("sys/devices/system/cpu/online")
            .and_then(|list| CpuList(&list).count())
            .or_else(|| std::thread::available_parallelism().ok().map(usize::from))
    }

    /// The controllers hoster's user manager delegates, read now.
    pub fn cgroup_controllers(&self) -> CgroupControllers {
        CgroupControllers::read(&self.root.join("sys/fs/cgroup"), self.uid)
    }

    /// Whether every resource limit of `stack` can be enforced: its cgroup controller is
    /// delegated to hoster's user manager (ADR-0059 §5). A stack without limits passes without
    /// the file being read, so only stacks that set one fail when it cannot be.
    pub fn check_limits(&self, stack: &Stack) -> Result<(), Undelegated> {
        let uses = LimitUses::of(stack);
        if uses.0.is_empty() {
            return Ok(());
        }
        self.cgroup_controllers().check(uses)
    }

    /// The limits of `stack` that are larger than this machine, so they cap nothing: `cpus`
    /// above its CPU count, `memory` above `MemTotal`. What cannot be read is not warned about.
    pub fn limit_warnings(&self, stack: &Stack) -> Vec<LimitWarning> {
        let mut warnings = Vec::new();
        let (mut cpus, mut memory) = (None, None);
        for (name, service) in &stack.services {
            let limits = &service.limits;
            if let Some(limit) = limits.cpus {
                let host = *cpus.get_or_insert_with(|| self.cpus());
                #[expect(clippy::cast_precision_loss, reason = "a CPU count")]
                if let Some(host) = host.filter(|host| f64::from(limit) > *host as f64) {
                    warnings.push(LimitWarning {
                        service: name.clone(),
                        limit: Limit::Cpus,
                        message: format!(
                            "`cpus = {limit}` is more than this host's {}, so it caps nothing",
                            Counted(host, "CPU")
                        ),
                    });
                }
            }
            if let Some(limit) = limits.memory {
                let host = *memory.get_or_insert_with(|| self.memory());
                if let Some(host) = host.filter(|host| u64::from(limit) > *host) {
                    warnings.push(LimitWarning {
                        service: name.clone(),
                        limit: Limit::Memory,
                        message: format!(
                            "`memory = \"{limit}\"` is more than this host's {} of memory \
                             (MemTotal), so it caps nothing",
                            Memory(host)
                        ),
                    });
                }
            }
        }
        warnings
    }
}

/// A limit of a stack that is larger than the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitWarning {
    pub service: Name,
    pub limit: Limit,
    pub message: String,
}

/// A size in bytes, in GiB with one decimal (MiB below one GiB), as the System page shows it.
pub struct Memory(pub u64);

impl fmt::Display for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MIB: f64 = 1024.0 * 1024.0;
        #[expect(clippy::cast_precision_loss, reason = "shown with one decimal")]
        let mib = self.0 as f64 / MIB;
        if mib >= 1024.0 {
            write!(f, "{:.1} GiB", mib / 1024.0)
        } else {
            write!(f, "{mib:.0} MiB")
        }
    }
}

/// `/proc/meminfo`'s text: `Name:  value kB` lines.
struct MemInfo<'a>(&'a str);

impl MemInfo<'_> {
    /// The field `name`, in bytes; `None` when missing or not a number of bytes or kB.
    fn bytes(&self, name: &str) -> Option<u64> {
        let line = self.0.lines().find(|l| {
            l.split_once(':')
                .is_some_and(|(field, _)| field.trim() == name)
        })?;
        let mut words = line.split_whitespace().skip(1);
        let value: u64 = words.next()?.parse().ok()?;
        match words.next() {
            Some("kB") => value.checked_mul(1024),
            None => Some(value),
            Some(_) => None,
        }
    }
}

/// The host's memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
pub struct MemoryUsage {
    /// `MemTotal`.
    pub total: u64,
    /// `MemAvailable`: what can be allocated without swapping, page cache included.
    pub available: u64,
}

#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
impl MemoryUsage {
    /// The memory in use: `MemTotal` − `MemAvailable` (ADR-0066).
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// The host's CPU time since boot, in jiffies (`USER_HZ`), summed over every CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
pub struct CpuTimes {
    /// Everything but idle and iowait.
    pub busy: u64,
    /// Every state: user, nice, system, idle, iowait, irq, softirq and steal. Guest time is
    /// already counted in user and nice, so it is not added again.
    pub total: u64,
}

#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
impl CpuTimes {
    /// The fraction of CPU time that was busy between `earlier` and this reading, from 0 to 1;
    /// `None` when no time passed or a counter went backwards.
    pub fn busy_fraction(&self, earlier: &CpuTimes) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total).filter(|t| *t > 0)?;
        let busy = self.busy.checked_sub(earlier.busy)?;
        #[expect(clippy::cast_precision_loss, reason = "a fraction")]
        Some((busy as f64 / total as f64).min(1.0))
    }
}

/// `/proc/stat` is not a kernel CPU statistics file with an aggregate `cpu` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no aggregate `cpu` line in /proc/stat")]
#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
pub struct NoCpuLine;

/// Parses `/proc/stat`'s text, of which only the aggregate `cpu` line is read: `cpu  user nice
/// system idle [iowait irq softirq steal guest guest_nice]`. The fields after idle came in
/// later kernels and count as zero when absent.
impl std::str::FromStr for CpuTimes {
    type Err = NoCpuLine;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("cpu "))
            .ok_or(NoCpuLine)?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| NoCpuLine)?;
        if fields.len() < 4 {
            return Err(NoCpuLine);
        }
        let field = |i: usize| fields.get(i).copied().unwrap_or(0);
        let total = (0..8)
            .map(field)
            .try_fold(0u64, u64::checked_add)
            .ok_or(NoCpuLine)?;
        let idle = field(3) + field(4);
        Ok(Self {
            busy: total - idle,
            total,
        })
    }
}

/// A filesystem's size, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
pub struct FilesystemUsage {
    /// The filesystem's size.
    pub total: u64,
    /// What an unprivileged user may still write (`f_bavail`), so less than free space by the
    /// blocks reserved for root.
    pub available: u64,
}

/// `statvfs` reported sizes that overflow a byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("statvfs sizes overflow a byte count")]
#[allow(dead_code, reason = "consumed by the resource sampler (ADR-0066)")]
pub struct Overflow;

impl TryFrom<rustix::fs::StatVfs> for FilesystemUsage {
    type Error = Overflow;

    fn try_from(stat: rustix::fs::StatVfs) -> Result<Self, Self::Error> {
        let bytes = |blocks: u64| blocks.checked_mul(stat.f_frsize).ok_or(Overflow);
        Ok(Self {
            total: bytes(stat.f_blocks)?,
            available: bytes(stat.f_bavail)?,
        })
    }
}

/// A kernel CPU list (`0-7`, `0,2-3`), as in `/sys/devices/system/cpu/online`.
struct CpuList<'a>(&'a str);

impl CpuList<'_> {
    /// How many CPUs the list names; `None` when it is not a CPU list.
    fn count(&self) -> Option<usize> {
        self.0.trim().split(',').try_fold(0usize, |count, range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let (first, last): (usize, usize) =
                (first.trim().parse().ok()?, last.trim().parse().ok()?);
            count.checked_add(last.checked_sub(first)? + 1)
        })
    }
}

/// A cgroup v2 controller a resource limit needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Controller {
    Cpu,
    Memory,
    Pids,
}

impl Controller {
    /// Every controller limits use, in the order `Delegate=` lists them.
    pub const ALL: [Controller; 3] = [Controller::Cpu, Controller::Memory, Controller::Pids];
}

impl From<Limit> for Controller {
    fn from(limit: Limit) -> Self {
        match limit {
            Limit::Memory => Controller::Memory,
            Limit::Pids => Controller::Pids,
            Limit::Cpus => Controller::Cpu,
        }
    }
}

/// As `cgroup.controllers` names it.
impl AsRef<str> for Controller {
    fn as_ref(&self) -> &str {
        match self {
            Controller::Cpu => "cpu",
            Controller::Memory => "memory",
            Controller::Pids => "pids",
        }
    }
}

impl fmt::Display for Controller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

/// The controllers listed in `user@<uid>.service`'s `cgroup.controllers`: what hoster's
/// containers, which podman puts in scopes under that unit, can be limited by (architecture
/// §1.8). libpod's `/info` reports podman's own cgroup instead, so it is not asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupControllers {
    uid: u32,
    path: PathBuf,
    /// The listed controllers, or why the file could not be read.
    listed: Result<BTreeSet<String>, String>,
}

impl CgroupControllers {
    /// Reads user `uid`'s manager's `cgroup.controllers` under the cgroup v2 mount
    /// `cgroup_root` (normally `/sys/fs/cgroup`).
    pub fn read(cgroup_root: &Path, uid: u32) -> Self {
        let path = cgroup_root.join(format!(
            "user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
        ));
        let listed = std::fs::read_to_string(&path)
            .map(|text| text.split_whitespace().map(str::to_owned).collect())
            .map_err(|e| e.to_string());
        Self { uid, path, listed }
    }

    /// The file read.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The listed controllers, or why the file could not be read.
    pub fn listed(&self) -> Result<&BTreeSet<String>, &str> {
        self.listed.as_ref().map_err(String::as_str)
    }

    /// The controllers limits use that are not listed, or why the file could not be read.
    pub fn missing(&self) -> Result<Vec<Controller>, &str> {
        let listed = self.listed()?;
        Ok(Controller::ALL
            .into_iter()
            .filter(|c| !listed.contains(c.as_ref()))
            .collect())
    }

    /// The one fix for a missing controller.
    pub fn fix(&self) -> DelegationFix {
        DelegationFix { uid: self.uid }
    }

    fn check(&self, uses: LimitUses) -> Result<(), Undelegated> {
        let reason = match self.listed() {
            Err(error) => Reason::Unreadable {
                path: self.path.clone(),
                error: error.to_owned(),
                uses: uses.0,
            },
            Ok(listed) => {
                let mut missing: BTreeMap<Controller, BTreeMap<Limit, Vec<Name>>> = BTreeMap::new();
                for (limit, services) in uses.0 {
                    let controller = Controller::from(limit);
                    if !listed.contains(controller.as_ref()) {
                        missing
                            .entry(controller)
                            .or_default()
                            .insert(limit, services);
                    }
                }
                if missing.is_empty() {
                    return Ok(());
                }
                Reason::Missing(missing)
            }
        };
        Err(Undelegated {
            reason,
            fix: self.fix(),
        })
    }
}

/// The services that set each limit, by limit.
struct LimitUses(BTreeMap<Limit, Vec<Name>>);

impl LimitUses {
    fn of(stack: &Stack) -> Self {
        let mut uses: BTreeMap<Limit, Vec<Name>> = BTreeMap::new();
        for (name, service) in &stack.services {
            for limit in service.limits.set() {
                uses.entry(limit).or_default().push(name.clone());
            }
        }
        Self(uses)
    }

    /// `` `limits.cpus` of api, db `` for each limit.
    fn describe(uses: &BTreeMap<Limit, Vec<Name>>) -> impl Iterator<Item = String> + '_ {
        uses.iter().map(|(limit, services)| {
            let services: Vec<&str> = services.iter().map(AsRef::as_ref).collect();
            format!("`limits.{limit}` of {}", services.join(", "))
        })
    }
}

/// `/etc/systemd/system/user@<uid>.service.d/hoster-delegate.conf` with `[Service]`
/// `Delegate=cpu memory pids`: the drop-in that delegates what limits use to hoster's user only.
/// The error, doctor and install.sh all name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationFix {
    uid: u32,
}

impl DelegationFix {
    /// The drop-in's path.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/etc/systemd/system/user@{}.service.d/hoster-delegate.conf",
            self.uid
        ))
    }
}

impl fmt::Display for DelegationFix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let controllers = Controller::ALL.map(|c| c.as_ref().to_owned()).join(" ");
        write!(
            f,
            "create {} containing `[Service]` and `Delegate={controllers}`, then restart \
             user@{uid}.service (which stops hoster and its containers) or reboot; \
             `systemctl daemon-reload` alone does not apply it",
            self.path().display(),
            uid = self.uid,
        )
    }
}

/// A stack sets a limit whose cgroup controller hoster's user manager does not delegate, or
/// hoster cannot tell (ADR-0059 §5). Apply and a deployment's `Validating` reject it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}; to fix it, {fix}")]
pub struct Undelegated {
    reason: Reason,
    fix: DelegationFix,
}

impl Undelegated {
    /// The (service, limit) pairs that cannot be enforced, in service order: those on a
    /// missing controller, or every limit when the file could not be read.
    pub fn uses(&self) -> Vec<(Name, Limit)> {
        let by_limit: Vec<&BTreeMap<Limit, Vec<Name>>> = match &self.reason {
            Reason::Missing(missing) => missing.values().collect(),
            Reason::Unreadable { uses, .. } => vec![uses],
        };
        let mut uses: Vec<(Name, Limit)> = by_limit
            .into_iter()
            .flatten()
            .flat_map(|(limit, services)| services.iter().map(|s| (s.clone(), *limit)))
            .collect();
        uses.sort();
        uses
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Reason {
    /// The controllers not listed, each with the limits that need it and the services that set
    /// them.
    Missing(BTreeMap<Controller, BTreeMap<Limit, Vec<Name>>>),
    /// `cgroup.controllers` could not be read; the services that set each limit.
    Unreadable {
        path: PathBuf,
        error: String,
        uses: BTreeMap<Limit, Vec<Name>>,
    },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Missing(missing) => {
                let names: Vec<&str> = missing.keys().map(AsRef::as_ref).collect();
                let needs: Vec<String> = missing.values().flat_map(LimitUses::describe).collect();
                let (noun, pronoun) = if names.len() == 1 {
                    ("controller", "it")
                } else {
                    ("controllers", "them")
                };
                write!(
                    f,
                    "hoster's user manager does not delegate the cgroup {noun} {}, so {} cannot \
                     be enforced without {pronoun}",
                    names.join(" and "),
                    needs.join(" and ")
                )
            }
            Reason::Unreadable { path, error, .. } => write!(
                f,
                "resource limits need the cgroup controllers of hoster's user manager, but {} \
                 cannot be read ({error}); limits need cgroup v2 and a running user manager \
                 (linger enabled)",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn controllers(root: &Path, listed: &str) {
        write(
            root,
            "sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/cgroup.controllers",
            listed,
        );
    }

    fn stack(limits: &[(&str, &str)]) -> Stack {
        let mut toml = String::from("version = 1\nname = \"notes\"\n");
        for (service, table) in limits {
            toml.push_str(&format!(
                "\n[services.{service}]\nimage = \"ghcr.io/acme/{service}\"\nlimits = {{ {table} }}\n"
            ));
        }
        toml.parse().unwrap()
    }

    #[test]
    fn cpu_lists_are_counted() {
        assert_eq!(CpuList("0-7\n").count(), Some(8));
        assert_eq!(CpuList("0").count(), Some(1));
        assert_eq!(CpuList("0,2-3,8-11").count(), Some(7));
        assert_eq!(CpuList("").count(), None);
        assert_eq!(CpuList("3-1").count(), None);
    }

    #[test]
    fn memory_and_cpus_come_from_proc_and_sys() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "proc/meminfo",
            "MemTotal:  32768000 kB\nMemFree: 1 kB\n",
        );
        write(root.path(), "sys/devices/system/cpu/online", "0-7\n");
        let machine = Machine::under(root.path(), 1000);
        assert_eq!(machine.memory(), Some(32_768_000 * 1024));
        assert_eq!(machine.cpus(), Some(8));
        assert_eq!(Machine::under(&root.path().join("x"), 1000).memory(), None);
    }

    #[test]
    fn memory_usage_comes_from_meminfo() {
        let root = tempfile::tempdir().unwrap();
        let machine = Machine::under(root.path(), 1000);
        assert_eq!(machine.memory_usage(), None);
        write(
            root.path(),
            "proc/meminfo",
            "MemTotal:  4194304 kB\nMemFree:  1048576 kB\nMemAvailable:  3145728 kB\n",
        );
        let usage = machine.memory_usage().unwrap();
        assert_eq!(usage.total, 4_194_304 * 1024);
        assert_eq!(usage.available, 3_145_728 * 1024);
        assert_eq!(usage.used(), 1024 * 1024 * 1024);
        assert_eq!(machine.memory(), Some(4_194_304 * 1024));

        // Without MemAvailable (before Linux 3.14) or with a malformed value: unknown.
        write(root.path(), "proc/meminfo", "MemTotal:  4194304 kB\n");
        assert_eq!(machine.memory_usage(), None);
        write(
            root.path(),
            "proc/meminfo",
            "MemTotal:  4194304 kB\nMemAvailable:  lots kB\n",
        );
        assert_eq!(machine.memory_usage(), None);
        assert_eq!(machine.memory(), Some(4_194_304 * 1024));
        write(root.path(), "proc/meminfo", "MemTotal: 1 MB\n");
        assert_eq!(machine.memory(), None);
        // More available than total (never, but not an underflow).
        let odd = MemoryUsage {
            total: 1,
            available: 2,
        };
        assert_eq!(odd.used(), 0);
    }

    #[test]
    fn cpu_times_come_from_the_aggregate_cpu_line() {
        let root = tempfile::tempdir().unwrap();
        let machine = Machine::under(root.path(), 1000);
        assert_eq!(machine.cpu_times(), None);
        write(
            root.path(),
            "proc/stat",
            "cpu  100 20 30 800 50 5 5 10 7 3\ncpu0 50 10 15 400 25 2 3 5 0 0\nintr 1 2 3\n",
        );
        let times = machine.cpu_times().unwrap();
        // user + nice + system + irq + softirq + steal; guest is in user and nice already.
        assert_eq!(times.busy, 170);
        assert_eq!(times.total, 1020);

        // An old kernel's four fields.
        assert_eq!(
            "cpu 1 2 3 4\n".parse::<CpuTimes>(),
            Ok(CpuTimes { busy: 6, total: 10 })
        );
        for malformed in ["", "cpu0 1 2 3 4\n", "cpu 1 2 3\n", "cpu 1 2 x 4\n"] {
            assert_eq!(
                malformed.parse::<CpuTimes>(),
                Err(NoCpuLine),
                "{malformed:?}"
            );
        }
        write(root.path(), "proc/stat", "cpu 1 2\n");
        assert_eq!(machine.cpu_times(), None);
    }

    #[test]
    fn busy_fraction_is_between_two_readings() {
        let earlier = CpuTimes {
            busy: 100,
            total: 1000,
        };
        let later = CpuTimes {
            busy: 150,
            total: 1200,
        };
        assert_eq!(later.busy_fraction(&earlier), Some(0.25));
        assert_eq!(earlier.busy_fraction(&earlier), None);
        assert_eq!(earlier.busy_fraction(&later), None);
        let busy_backwards = CpuTimes {
            busy: 50,
            total: 1200,
        };
        assert_eq!(busy_backwards.busy_fraction(&earlier), None);
    }

    #[test]
    fn filesystem_usage_comes_from_statvfs() {
        let dir = tempfile::tempdir().unwrap();
        let machine = Machine::under(&dir.path().join("unused"), 1000);
        let usage = machine.filesystem(dir.path()).unwrap();
        assert!(usage.total > 0);
        assert!(usage.available <= usage.total);
        assert_eq!(machine.filesystem(&dir.path().join("missing")), None);
    }

    #[test]
    fn controllers_are_read_from_the_user_managers_cgroup() {
        let root = tempfile::tempdir().unwrap();
        let machine = Machine::under(root.path(), 1000);
        let unreadable = machine.cgroup_controllers();
        assert!(unreadable.listed().is_err());
        assert!(unreadable.missing().is_err());
        assert!(
            unreadable
                .path()
                .ends_with("user.slice/user-1000.slice/user@1000.service/cgroup.controllers")
        );

        // Ubuntu 22.04's systemd delegates `pids memory` only.
        controllers(root.path(), "memory pids\n");
        let partial = machine.cgroup_controllers();
        assert_eq!(partial.missing().unwrap(), [Controller::Cpu]);
        controllers(root.path(), "cpuset cpu io memory pids\n");
        assert!(machine.cgroup_controllers().missing().unwrap().is_empty());
    }

    #[test]
    fn a_limit_on_an_undelegated_controller_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let machine = Machine::under(root.path(), 1000);
        controllers(root.path(), "memory pids\n");
        assert_eq!(
            machine.check_limits(&stack(&[("api", "memory = \"64MiB\"")])),
            Ok(())
        );
        let limited = stack(&[
            ("api", "cpus = 1.5, memory = \"64MiB\""),
            ("db", "cpus = 2"),
        ]);
        let err = machine.check_limits(&limited).unwrap_err();
        assert_eq!(
            err.to_string(),
            "hoster's user manager does not delegate the cgroup controller cpu, so \
             `limits.cpus` of api, db cannot be enforced without it; to fix it, create \
             /etc/systemd/system/user@1000.service.d/hoster-delegate.conf containing \
             `[Service]` and `Delegate=cpu memory pids`, then restart user@1000.service (which \
             stops hoster and its containers) or reboot; `systemctl daemon-reload` alone does \
             not apply it"
        );
        let name = |s: &str| s.parse::<Name>().unwrap();
        assert_eq!(
            err.uses(),
            [(name("api"), Limit::Cpus), (name("db"), Limit::Cpus)]
        );

        controllers(root.path(), "cpu\n");
        let err = machine.check_limits(&limited).unwrap_err().to_string();
        assert!(
            err.starts_with(
                "hoster's user manager does not delegate the cgroup controller memory, so \
                 `limits.memory` of api cannot"
            ),
            "{err}"
        );
        let both = stack(&[("api", "pids = 100, memory = \"64MiB\"")]);
        let err = machine.check_limits(&both).unwrap_err().to_string();
        assert!(
            err.contains("controllers memory and pids, so `limits.memory` of api and `limits.pids` of api cannot be enforced without them"),
            "{err}"
        );
    }

    #[test]
    fn an_unreadable_file_rejects_only_stacks_with_limits() {
        let root = tempfile::tempdir().unwrap();
        let machine = Machine::under(root.path(), 1000);
        assert_eq!(machine.check_limits(&stack(&[("api", "")])), Ok(()));
        let err = machine
            .check_limits(&stack(&[("api", "pids = 64")]))
            .unwrap_err();
        assert_eq!(err.uses(), [("api".parse().unwrap(), Limit::Pids)]);
        let err = err.to_string();
        assert!(
            err.starts_with(
                "resource limits need the cgroup controllers of hoster's user manager, but "
            ),
            "{err}"
        );
        assert!(
            err.contains("user@1000.service/cgroup.controllers cannot be read"),
            "{err}"
        );
        assert!(err.contains("hoster-delegate.conf"), "{err}");
    }

    #[test]
    fn limits_larger_than_the_machine_are_warned_about() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "proc/meminfo", "MemTotal:  4194304 kB\n");
        write(root.path(), "sys/devices/system/cpu/online", "0-3\n");
        let machine = Machine::under(root.path(), 1000);
        let fits = stack(&[("api", "cpus = 4, memory = \"4GiB\", pids = 100000")]);
        assert!(machine.limit_warnings(&fits).is_empty());
        let larger = stack(&[("api", "cpus = 4.5, memory = \"5GiB\"")]);
        let messages: Vec<(Limit, String)> = machine
            .limit_warnings(&larger)
            .into_iter()
            .map(|w| (w.limit, w.message))
            .collect();
        assert_eq!(
            messages,
            [
                (
                    Limit::Cpus,
                    "`cpus = 4.5` is more than this host's 4 CPUs, so it caps nothing".to_owned()
                ),
                (
                    Limit::Memory,
                    "`memory = \"5GiB\"` is more than this host's 4.0 GiB of memory (MemTotal), \
                     so it caps nothing"
                        .to_owned()
                ),
            ]
        );
        // Nothing to compare with: no warning.
        let unknown = Machine::under(&root.path().join("missing"), 1000);
        assert!(
            unknown
                .limit_warnings(&stack(&[("api", "memory = \"5GiB\"")]))
                .is_empty()
        );
    }
}
