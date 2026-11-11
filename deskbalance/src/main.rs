//! deskbalance: keeps programs that start or wait on the disk ahead of
//! long-running work, inside the user manager's cgroup tree.
//!
//! - A program starting (a unit appearing in `app.slice`) caps
//!   `background.slice`, and `app.slice` units burning more than
//!   [`heavy_cpus`], to [`boost_cpus`] in all for [`BOOST_TIME`] after the last
//!   start, [`BOOST_LIMIT`] at most, so it keeps the turbo and whole cores.
//!   How busy the new unit is says nothing: it mostly waits on the disk.
//! - At that moment, and whenever `app.slice` waits on the disk (PSI trigger),
//!   a program scope that has run for [`MIN_AGE`] and moves the most bytes is
//!   moved into a scope of its own in `background.slice`, which the session's
//!   io.latency target throttles. That scope may hold what the job allocates
//!   plus [`CACHE_ROOM`], so the page cache of a copy stays there instead of
//!   pushing programs out of RAM. Only the busiest job moves: the processes
//!   sharing the process group of the one with the most disk bytes. A copy or
//!   build started from a terminal is a job of its own, so the terminal stays;
//!   a program's own I/O shares its group, and the whole program moves.
//! - A moved job goes back to `app.slice`, in a scope named after its unit,
//!   after [`QUIET_TIME`] without I/O.
//!
//! It blocks in inotify, PSI and a signalfd: no wakeups while nothing starts,
//! nobody waits on the disk and nothing is moved. On a kernel without
//! unprivileged PSI triggers it moves jobs only when a program starts. It
//! runs as a service of the user manager whose tree it balances, and takes
//! over the caps and moved jobs an instance that was killed left behind.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};

/// How long the CPU caps hold after the last program start.
const BOOST_TIME: Duration = Duration::from_secs(10);
/// The longest a boost lasts from its first start. One that reaches it is
/// followed by [`BOOST_TIME`] without one: units appearing in a loop cannot
/// hold the caps.
const BOOST_LIMIT: Duration = Duration::from_secs(30);
/// Bytes per second, and [`SHARE`] of `app.slice`'s, for a unit to move.
const HEAVY_IO: f64 = (5 << 20) as f64;
/// A program younger than this may still be loading: it is never moved.
const MIN_AGE: Duration = Duration::from_secs(60);
/// The span of `app.slice`'s bytes looked at under pressure.
const WINDOW: Duration = Duration::from_secs(3);
/// The share of `app.slice`'s bytes that moves a unit.
const SHARE: f64 = 0.7;
/// A moved job slower than this many bytes per second, and not blocked, is quiet.
const QUIET_RATE: f64 = (1 << 20) as f64;
const QUIET_TIME: Duration = Duration::from_secs(10);
/// 256 MB: a copy keeps its speed and the programs stay cached.
const CACHE_ROOM: u64 = 256 << 20;
/// Unprivileged PSI triggers need a window that is a multiple of 2 s.
const PSI_TRIGGER: &[u8] = b"some 150000 2000000\0";

/// A quarter of the CPUs, at least one.
fn boost_cpus() -> f64 {
    (cpu_count() / 4).max(1) as f64
}

/// A unit above an eighth of the CPUs, at least one, is capped too: a fixed
/// count would let a `make -j2` escape on a 4-thread CPU.
fn heavy_cpus() -> f64 {
    (cpu_count() as f64 / 8.0).max(1.0)
}

fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// The contents of a cgroup or proc file, empty when it is gone.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Process (`procs`) or thread (`threads`) ids of a cgroup.
fn ids(cgroup: &Path, kind: &str) -> Vec<u32> {
    read(&cgroup.join(format!("cgroup.{kind}")))
        .split_whitespace()
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// Fields of `/proc/<pid>/stat` after the command name, which may hold spaces
/// and parentheses: index 0 is the state, 2 the process group, 19 the start time.
fn stat_fields(stat: &str) -> Vec<&str> {
    stat.rsplit_once(')')
        .map_or_else(Vec::new, |(_, rest)| rest.split_whitespace().collect())
}

/// Disk bytes read and written, from the `rbytes=` and `wbytes=` of `io.stat`.
fn io_stat_bytes(io_stat: &str) -> u64 {
    io_stat
        .lines()
        .flat_map(|line| line.split_whitespace().skip(1))
        .filter_map(|kv| {
            kv.strip_prefix("rbytes=")
                .or_else(|| kv.strip_prefix("wbytes="))
        })
        .filter_map(|n| n.parse::<u64>().ok())
        .sum()
}

/// CPU microseconds and disk bytes of a cgroup so far.
fn usage(cgroup: &Path) -> (u64, u64) {
    let cpu = read(&cgroup.join("cpu.stat"))
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    (cpu, io_stat_bytes(&read(&cgroup.join("io.stat"))))
}

type Snapshot = HashMap<String, (u64, u64)>;

/// [`usage`] of every unit in a slice.
fn units(slice: &Path) -> Snapshot {
    let Ok(entries) = fs::read_dir(slice) else {
        return Snapshot::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                usage(&e.path()),
            )
        })
        .collect()
}

/// Bytes per second each unit moved between two snapshots `secs` apart.
fn io_rates(before: &Snapshot, after: &Snapshot, secs: f64) -> HashMap<String, f64> {
    after
        .iter()
        .filter_map(|(unit, &(_, bytes))| {
            let (_, old) = before.get(unit)?;
            Some((unit.clone(), bytes.saturating_sub(*old) as f64 / secs))
        })
        .collect()
}

/// A scope of a program the user started, as systemd names it.
fn movable(unit: &str) -> bool {
    unit.starts_with("app-") && unit.ends_with(".scope")
}

/// Seconds since the oldest process of a cgroup started.
fn age(cgroup: &Path) -> f64 {
    let ticks = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .unwrap_or(100) as f64;
    let Some(oldest) = ids(cgroup, "procs")
        .iter()
        .filter_map(|pid| {
            let stat = read(Path::new(&format!("/proc/{pid}/stat")));
            stat_fields(&stat).get(19)?.parse::<u64>().ok()
        })
        .min()
    else {
        return 0.0;
    };
    let Ok(now) = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME) else {
        return 0.0;
    };
    now.tv_sec() as f64 + now.tv_nsec() as f64 / 1e9 - oldest as f64 / ticks
}

/// Bytes each process moved through read()/write(), and whether a thread
/// sleeps in D state. A copy held back by io.latency or dirty throttling
/// shows little in `io.stat` or `io.pressure`, but its thread sleeps
/// uninterruptibly.
fn activity(cgroup: &Path) -> (HashMap<u32, u64>, bool) {
    let chars = ids(cgroup, "procs")
        .into_iter()
        .map(|pid| (pid, proc_io(pid, "rchar", "wchar")))
        .collect();
    let blocked = ids(cgroup, "threads").iter().any(|tid| {
        let stat = read(Path::new(&format!("/proc/{tid}/stat")));
        stat_fields(&stat).first() == Some(&"D")
    });
    (chars, blocked)
}

/// The sum of two fields of `/proc/<pid>/io`.
fn proc_io(pid: u32, first: &str, second: &str) -> u64 {
    read(Path::new(&format!("/proc/{pid}/io")))
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(": ")?;
            if key == first || key == second {
                value.parse::<u64>().ok()
            } else {
                None
            }
        })
        .sum()
}

fn disk_bytes(pid: u32) -> u64 {
    proc_io(pid, "read_bytes", "write_bytes")
}

fn group(pid: u32) -> Option<u32> {
    let stat = read(Path::new(&format!("/proc/{pid}/stat")));
    stat_fields(&stat).get(2)?.parse().ok()
}

/// The process group whose processes moved the most disk bytes in `delta`,
/// when they moved at least half of [`HEAVY_IO`]. A job of short-lived
/// processes (a `cp` per file) is counted as a whole.
fn busiest_group(delta: &HashMap<u32, u64>, groups: &HashMap<u32, u32>) -> Option<u32> {
    let mut per_group: HashMap<u32, u64> = HashMap::new();
    for (pid, &bytes) in delta {
        if let Some(&group) = groups.get(pid) {
            *per_group.entry(group).or_default() += bytes;
        }
    }
    let (group, bytes) = per_group.into_iter().max_by_key(|&(_, bytes)| bytes)?;
    ((bytes as f64) >= HEAVY_IO / 2.0).then_some(group)
}

/// Process group of the busiest job in a cgroup over 0.5 s. The unit's
/// `io.stat` keeps counting a moved job's writeback for ~30 s, so a unit
/// whose processes write nothing themselves stays.
fn job(cgroup: &Path) -> Option<u32> {
    let before: HashMap<u32, u64> = ids(cgroup, "procs")
        .into_iter()
        .map(|pid| (pid, disk_bytes(pid)))
        .collect();
    sleep(Duration::from_millis(500));
    let after: Vec<u32> = ids(cgroup, "procs");
    let delta: HashMap<u32, u64> = after
        .iter()
        .map(|&pid| {
            let now = disk_bytes(pid);
            // A process that started meanwhile moved all its bytes in the window.
            (
                pid,
                now.saturating_sub(before.get(&pid).copied().unwrap_or(0)),
            )
        })
        .collect();
    let groups: HashMap<u32, u32> = after
        .iter()
        .filter_map(|&pid| Some((pid, group(pid)?)))
        .collect();
    busiest_group(&delta, &groups)
}

/// A call to the user manager; the error is what busctl printed. busctl
/// takes options anywhere in its argv, and unit names may start with `-`.
/// It inherits this daemon's blocked SIGTERM, so its own timeout bounds a
/// stop while a call hangs.
fn manager_call(method: &str, signature: &str, args: &[String]) -> Result<(), String> {
    let out = Command::new("busctl")
        .args([
            "--user",
            "--timeout=5",
            "call",
            "--",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            method,
            signature,
        ])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// A property of a unit of the user manager, as systemctl prints it.
fn unit_property(unit: &str, property: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property",
            property,
            "--value",
            "--",
            unit,
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn attach(unit: &str, procs: &[u32]) -> Result<(), String> {
    let mut args = vec![unit.to_owned(), String::new(), procs.len().to_string()];
    args.extend(procs.iter().map(u32::to_string));
    manager_call("AttachProcessesToUnit", "ssau", &args)
}

/// Starts a scope in `slice` holding `procs`, described by the unit they
/// come from. It is delegated: systemd attaches processes only to delegated
/// units.
fn start_scope(
    procs: &[u32],
    scope: &str,
    slice: &str,
    origin: &str,
    memory_high: Option<u64>,
) -> Result<(), String> {
    let mut props: Vec<String> = vec!["PIDs".into(), "au".into(), procs.len().to_string()];
    props.extend(procs.iter().map(u32::to_string));
    props.extend(["Slice".into(), "s".into(), slice.into()]);
    props.extend(["Description".into(), "s".into(), origin.into()]);
    props.extend(["Delegate".into(), "b".into(), "true".into()]);
    let mut count = 4;
    if let Some(high) = memory_high {
        props.extend(["MemoryHigh".into(), "t".into(), high.to_string()]);
        count += 1;
    }
    let mut args = vec![scope.to_owned(), "fail".to_owned(), count.to_string()];
    args.extend(props);
    args.push("0".into());
    manager_call("StartTransientUnit", "ssa(sv)a(sa(sv))", &args)
}

/// Moves the processes of cgroup `from` that `pick` selects into a new scope
/// in the slice at `to`, and returns how many moved. systemd answers before
/// the scope's cgroup exists, and a job forks while it moves: the cgroup is
/// awaited, then the children left behind follow.
fn move_job(
    from: &Path,
    pick: impl Fn(u32) -> bool,
    to: &Path,
    scope: &str,
    origin: &str,
    memory_high: Option<u64>,
) -> Result<usize, String> {
    let slice = to.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let picked = || -> Vec<u32> {
        ids(from, "procs")
            .into_iter()
            .filter(|&pid| pick(pid))
            .collect()
    };
    let mut procs = picked();
    if start_scope(&procs, scope, slice, origin, memory_high).is_err() {
        // A process exited between reading cgroup.procs and the call (a cp
        // between two files): systemd refuses the whole list. Once more.
        procs = picked();
        if procs.is_empty() {
            return Err("its processes are gone".into());
        }
        start_scope(&procs, scope, slice, origin, memory_high)?;
    }
    let cgroup = to.join(scope);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cgroup.is_dir() {
        if Instant::now() >= deadline {
            println!("{scope}: not there after 2 s, late children stay behind");
            return Ok(procs.len());
        }
        sleep(Duration::from_millis(10));
    }
    let late = picked();
    if !late.is_empty()
        && let Err(e) = attach(scope, &late)
    {
        println!("{scope}: {} processes stay behind: {e}", late.len());
    }
    Ok(ids(&cgroup, "procs").len())
}

/// The scope in `app.slice` a moved job returns to: its unit's name, without
/// what an earlier return added, and the scope it leaves.
fn return_scope(unit: &str, scope: &str) -> String {
    let base = unit.trim_end_matches(".scope");
    let base = base.find("-deskbalance-").map_or(base, |i| &base[..i]);
    format!("{base}-{}.scope", scope.trim_end_matches(".scope"))
}

struct Moved {
    unit: String,
    /// Bytes through read()/write() per process when last looked at, and when.
    chars: HashMap<u32, u64>,
    sampled: Instant,
    quiet_since: Option<Instant>,
    /// Whether its cgroup has existed: systemd may create it after answering.
    seen: bool,
}

impl Moved {
    fn new(unit: &str, cgroup: &Path) -> Self {
        Moved {
            unit: unit.to_owned(),
            chars: activity(cgroup).0,
            sampled: Instant::now(),
            quiet_since: None,
            seen: cgroup.is_dir(),
        }
    }
}

struct Balancer {
    app: PathBuf,
    background: PathBuf,
    /// Scope in `background.slice` -> where it came from.
    moved: HashMap<String, Moved>,
    scopes_started: u32,
    /// Scopes this daemon created in `app.slice`: not a program starting.
    own: HashSet<String>,
    /// Cgroup -> the `cpu.max` it had.
    capped: Vec<(PathBuf, String)>,
}

impl Balancer {
    /// Caps a cgroup to `cpus`; false where it has no `cpu.max`
    /// (`background.slice` before anything ran in it), already has a quota at
    /// or below `cpus`, or the write fails.
    fn cap(&mut self, cgroup: PathBuf, cpus: f64) -> bool {
        let old = read(&cgroup.join("cpu.max"));
        if old.is_empty() {
            return false;
        }
        if let Some((quota, period)) = old.split_once(' ')
            && let (Ok(quota), Ok(period)) = (quota.parse::<f64>(), period.trim().parse::<f64>())
            && quota / period <= cpus
        {
            return false;
        }
        let quota = (cpus * 100_000.0) as u64;
        let capped = fs::write(cgroup.join("cpu.max"), format!("{quota} 100000\n")).is_ok();
        if capped {
            self.capped.push((cgroup, old));
        }
        capped
    }

    fn uncap(&mut self) {
        for (cgroup, old) in self.capped.drain(..) {
            let _ = fs::write(cgroup.join("cpu.max"), old);
        }
    }

    /// Takes over what an instance that was killed left behind: CPU caps
    /// systemd does not know of, and jobs in their scopes in `background.slice`.
    fn recover(&mut self) {
        let mut cgroups = vec![self.background.clone()];
        cgroups.extend(units(&self.app).into_keys().map(|unit| self.app.join(unit)));
        for cgroup in cgroups {
            let Some(unit) = cgroup.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let max = read(&cgroup.join("cpu.max"));
            if !max.is_empty()
                && !max.starts_with("max ")
                && unit_property(unit, "CPUQuotaPerSecUSec").as_deref() == Some("infinity")
                && fs::write(cgroup.join("cpu.max"), "max 100000\n").is_ok()
            {
                println!("{unit}: CPU cap lifted");
            }
        }
        for scope in units(&self.background).into_keys() {
            if !scope.starts_with("deskbalance-") {
                continue;
            }
            match unit_property(&scope, "Description") {
                Some(origin) if movable(&origin) => {
                    println!("{scope}: taken over, from {origin}");
                    let moved = Moved::new(&origin, &self.background.join(&scope));
                    self.moved.insert(scope, moved);
                }
                _ => println!("{scope}: left alone, its origin is unknown"),
            }
        }
    }

    fn demote(&mut self, unit: &str, why: &str) {
        let path = self.app.join(unit);
        let Some(pgid) = job(&path) else {
            return;
        };
        self.scopes_started += 1;
        // The process id keeps the name free of scopes left by a killed instance.
        let scope = format!(
            "deskbalance-{}-{}.scope",
            std::process::id(),
            self.scopes_started
        );
        // Charges stay with the old cgroup; the new scope holds what the job
        // allocates from now on, and check_moved keeps its limit at that
        // plus the cache.
        let pick = |pid| group(pid) == Some(pgid);
        match move_job(
            &path,
            pick,
            &self.background,
            &scope,
            unit,
            Some(CACHE_ROOM),
        ) {
            Ok(count) => {
                let left = ids(&path, "procs").len();
                println!(
                    "{why}: {count} of {} processes of {unit} -> background.slice/{scope}",
                    count + left
                );
                let moved = Moved::new(unit, &self.background.join(&scope));
                self.moved.insert(scope, moved);
            }
            Err(e) => println!("{why}: {unit} not moved: {e}"),
        }
    }

    /// Returns a moved job to `app.slice`. When that fails, it is tried again
    /// after another [`QUIET_TIME`].
    fn restore(&mut self, scope: &str) {
        let Some(unit) = self.moved.get(scope).map(|m| m.unit.clone()) else {
            return;
        };
        let cgroup = self.background.join(scope);
        if ids(&cgroup, "procs").is_empty() {
            self.moved.remove(scope);
            return;
        }
        // Program scopes are not delegated: the job gets a scope of its own,
        // which an earlier, partial return may have started already.
        let back = return_scope(&unit, scope);
        let result = if self.app.join(&back).is_dir() {
            attach(&back, &ids(&cgroup, "procs"))
        } else {
            self.own.insert(back.clone());
            move_job(&cgroup, |_| true, &self.app, &back, &unit, None).map(|_| ())
        };
        if let Err(e) = &result {
            println!("quiet: {scope} not returned: {e}");
            self.own.remove(&back);
        }
        if result.is_ok() && ids(&cgroup, "procs").is_empty() {
            println!("quiet: {scope} -> app.slice/{back}");
            self.moved.remove(scope);
        } else if let Some(moved) = self.moved.get_mut(scope) {
            moved.quiet_since = None;
        }
    }

    /// Moves the program scope doing most of `app.slice`'s disk I/O; true if one moved.
    fn move_heavy(&mut self, rates: &HashMap<String, f64>, why: &str) -> bool {
        let total: f64 = rates.values().sum();
        let heavy = rates
            .iter()
            .filter(|&(unit, &rate)| {
                rate > HEAVY_IO
                    && rate >= SHARE * total
                    && movable(unit)
                    && age(&self.app.join(unit)) >= MIN_AGE.as_secs_f64()
            })
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(unit, _)| unit.clone());
        match heavy {
            Some(unit) => {
                self.demote(&unit, why);
                true
            }
            None => false,
        }
    }

    /// Caps CPU for a start and moves a heavy disk user if one shows already.
    /// When none does, the snapshot to look at again later and when it was taken.
    fn boost(&mut self, started: &[String]) -> Option<(Instant, Snapshot)> {
        println!("boost on: {}", started.join(" "));
        let taken = Instant::now();
        let before = units(&self.app);
        sleep(Duration::from_millis(200));
        let after = units(&self.app);
        let heavy: Vec<(String, f64)> = after
            .iter()
            .filter(|(unit, _)| !started.contains(unit))
            .filter_map(|(unit, &(cpu, _))| {
                let (old, _) = before.get(unit)?;
                let cpus = cpu.saturating_sub(*old) as f64 / 0.2e6;
                (cpus > heavy_cpus()).then(|| (unit.clone(), cpus))
            })
            .collect();
        let share = boost_cpus() / (heavy.len() + 1) as f64;
        self.cap(self.background.clone(), share);
        for (unit, cpus) in &heavy {
            if self.cap(self.app.join(unit), share) {
                println!("cap: {unit} ({cpus:.1} CPUs -> {share:.1})");
            } else {
                println!("cap: {unit} ({cpus:.1} CPUs) left as it is");
            }
        }
        if self.move_heavy(&io_rates(&before, &after, 0.2), "start") {
            None
        } else {
            Some((taken, before))
        }
    }

    /// Returns each moved job that has been quiet for [`QUIET_TIME`].
    fn check_moved(&mut self) {
        let mut quiet = Vec::new();
        self.moved.retain(|scope, moved| {
            let cgroup = self.background.join(scope);
            if !cgroup.is_dir() {
                return !moved.seen && moved.sampled.elapsed() < Duration::from_secs(60);
            }
            moved.seen = true;
            // Wakeups for starts or pressure come closer: a rate over a few
            // milliseconds of terminal output would read as busy.
            if moved.sampled.elapsed() < Duration::from_secs(1) {
                return true;
            }
            // What the job allocated: anonymous memory, and shmem, which holds
            // its tmpfs files and shared mappings.
            let allocated: Option<u64> = read(&cgroup.join("memory.stat"))
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("anon ")
                        .or_else(|| line.strip_prefix("shmem "))
                })
                .map(|n| n.parse::<u64>().ok())
                .sum();
            if let Some(allocated) = allocated {
                let high = (allocated + CACHE_ROOM).to_string();
                if read(&cgroup.join("memory.high")).trim() != high {
                    let _ = fs::write(cgroup.join("memory.high"), high);
                }
            }
            let (chars, blocked) = activity(&cgroup);
            let now = Instant::now();
            let secs = now.duration_since(moved.sampled).as_secs_f64();
            let bytes: u64 = chars
                .iter()
                .map(|(pid, &now)| now.saturating_sub(moved.chars.get(pid).copied().unwrap_or(0)))
                .sum();
            let rate = bytes as f64 / secs.max(f64::EPSILON);
            moved.chars = chars;
            moved.sampled = now;
            moved.quiet_since = if !blocked && rate < QUIET_RATE {
                Some(moved.quiet_since.unwrap_or(now))
            } else {
                None
            };
            if moved
                .quiet_since
                .is_some_and(|since| now.duration_since(since) >= QUIET_TIME)
            {
                quiet.push(scope.clone());
            }
            true
        });
        for scope in quiet {
            self.restore(&scope);
        }
    }

    /// Under pressure, moves the program scope with [`SHARE`] of `app.slice`'s
    /// bytes over [`WINDOW`], when another unit uses the disk too.
    fn relieve(&mut self, history: &mut Vec<(Instant, Snapshot)>, now: Instant) {
        history.retain(|(t, _)| now.duration_since(*t) <= WINDOW);
        history.push((now, units(&self.app)));
        let (Some((t0, oldest)), Some((_, current))) = (history.first(), history.last()) else {
            return;
        };
        let span = now.duration_since(*t0);
        if span < WINDOW / 2 {
            return;
        }
        let rates = io_rates(oldest, current, span.as_secs_f64());
        let total: f64 = rates.values().sum();
        let Some((unit, &rate)) = rates
            .iter()
            .filter(|(unit, _)| movable(unit))
            .max_by(|a, b| a.1.total_cmp(b.1))
        else {
            return;
        };
        let busy_units = rates.values().filter(|&&r| r > 0.0).count();
        if rate > HEAVY_IO
            && rate >= SHARE * total
            && busy_units > 1
            && age(&self.app.join(unit)) >= MIN_AGE.as_secs_f64()
        {
            let unit = unit.clone();
            self.demote(&unit, "pressure");
            history.clear();
        }
    }
}

/// The user manager this process runs under, from its own cgroup.
fn manager_dir() -> Result<PathBuf, String> {
    let cgroup = read(Path::new("/proc/self/cgroup"));
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("no cgroup v2 path in /proc/self/cgroup")?;
    let end = path
        .find(".service/")
        .filter(|&i| {
            path[..i]
                .rsplit('/')
                .next()
                .is_some_and(|u| u.starts_with("user@"))
        })
        .ok_or_else(|| format!("{path} is not inside a user manager"))?;
    Ok(Path::new("/sys/fs/cgroup").join(&path[1..end + ".service".len()]))
}

fn run() -> Result<(), String> {
    let user_manager = manager_dir()?;
    let mut balancer = Balancer {
        app: user_manager.join("app.slice"),
        background: user_manager.join("background.slice"),
        moved: HashMap::new(),
        scopes_started: 0,
        own: HashSet::new(),
        capped: Vec::new(),
    };
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.add(Signal::SIGINT);
    signals
        .thread_block()
        .map_err(|e| format!("signals: {e}"))?;
    let signal_fd = SignalFd::with_flags(&signals, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)
        .map_err(|e| format!("signalfd: {e}"))?;
    balancer.recover();
    let controllers = read(&balancer.app.join("cgroup.subtree_control"));
    if !controllers.split_whitespace().any(|c| c == "io") {
        println!("app.slice has no io controller: no job moves until a unit there asks for it");
    }
    if !controllers.split_whitespace().any(|c| c == "cpu") {
        println!("app.slice has no cpu controller: of the CPU caps, only background.slice's hold");
    }
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
        .map_err(|e| format!("inotify: {e}"))?;
    inotify
        .add_watch(&balancer.app, AddWatchFlags::IN_CREATE)
        .map_err(|e| format!("inotify on {}: {e}", balancer.app.display()))?;
    let psi_path = balancer.app.join("io.pressure");
    let psi = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&psi_path)
        .and_then(|file| std::io::Write::write_all(&mut &file, PSI_TRIGGER).map(|()| file));
    let psi = match psi {
        Ok(file) => Some(file),
        Err(e) => {
            println!(
                "no PSI trigger on {}: {e}; jobs move only when a program starts",
                psi_path.display()
            );
            None
        }
    };

    // The first start of a boost, and when the boost ends.
    let mut boost: Option<(Instant, Instant)> = None;
    // Starts are ignored until then after a boost that reached BOOST_LIMIT.
    let mut no_boost_until: Option<Instant> = None;
    // Snapshots of app.slice under pressure, the oldest first.
    let mut history: Vec<(Instant, Snapshot)> = Vec::new();
    // A disk that writes back in bursts (HDD) shows nothing in 200 ms: the
    // bytes are looked at again over a longer span, without blocking.
    let mut io_check: Option<(Instant, Instant, Snapshot)> = None;

    let result = loop {
        let now = Instant::now();
        let mut timeouts = Vec::new();
        if let Some((_, until)) = boost {
            timeouts.push(until.saturating_duration_since(now));
        }
        if !balancer.moved.is_empty() || !history.is_empty() {
            timeouts.push(Duration::from_secs(1));
        }
        if let Some((due, _, _)) = &io_check {
            timeouts.push(due.saturating_duration_since(now));
        }
        // Rounded up: a timeout cut to 0 ms would spin until the deadline.
        let timeout = timeouts.iter().min().map_or(PollTimeout::NONE, |t| {
            PollTimeout::try_from(t.as_micros().div_ceil(1000)).unwrap_or(PollTimeout::MAX)
        });
        let mut fds = vec![
            PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(inotify.as_fd(), PollFlags::POLLIN),
        ];
        if let Some(psi) = &psi {
            fds.push(PollFd::new(psi.as_fd(), PollFlags::POLLPRI));
        }
        match poll(&mut fds, timeout) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(e) => break Err(format!("poll: {e}")),
        }
        let revents = |i: usize| {
            fds.get(i)
                .and_then(PollFd::revents)
                .unwrap_or(PollFlags::empty())
        };
        let failed = PollFlags::POLLERR | PollFlags::POLLNVAL;
        if let Some(i) = (0..fds.len()).find(|&i| revents(i).intersects(failed)) {
            let name = ["signalfd", "inotify", "PSI trigger"][i];
            break Err(format!("{name} failed: {:?}", revents(i)));
        }
        let (stop, started_ready, pressure) = (
            revents(0).contains(PollFlags::POLLIN),
            revents(1).contains(PollFlags::POLLIN),
            revents(2).contains(PollFlags::POLLPRI),
        );
        if stop {
            break Ok(());
        }
        let now = Instant::now();

        if started_ready {
            let events = match inotify.read_events() {
                Ok(events) => events,
                Err(nix::errno::Errno::EAGAIN) => Vec::new(),
                Err(e) => break Err(format!("inotify: {e}")),
            };
            if events
                .iter()
                .any(|e| e.mask.contains(AddWatchFlags::IN_IGNORED))
            {
                break Err(format!("{} is gone", balancer.app.display()));
            }
            // A scope this daemon created shows up once; the rest are starts.
            let started: Vec<String> = events
                .into_iter()
                .filter(|e| e.mask.contains(AddWatchFlags::IN_ISDIR))
                .filter_map(|e| e.name.map(|n| n.to_string_lossy().into_owned()))
                .filter(|n| !balancer.own.remove(n))
                .collect();
            if !started.is_empty() && no_boost_until.is_none_or(|t| now >= t) {
                let first = match boost {
                    Some((first, _)) => first,
                    None => {
                        io_check = balancer.boost(&started).map(|(taken, snapshot)| {
                            (Instant::now() + Duration::from_secs(1), taken, snapshot)
                        });
                        now
                    }
                };
                boost = Some((
                    first,
                    (Instant::now() + BOOST_TIME).min(first + BOOST_LIMIT),
                ));
            }
        }
        if let Some((due, taken, snapshot)) = io_check.take() {
            if Instant::now() >= due {
                let rates = io_rates(
                    &snapshot,
                    &units(&balancer.app),
                    taken.elapsed().as_secs_f64(),
                );
                balancer.move_heavy(&rates, "start, 1 s later");
            } else {
                io_check = Some((due, taken, snapshot));
            }
        }
        if let Some((first, until)) = boost
            && Instant::now() >= until
        {
            println!("boost off");
            balancer.uncap();
            boost = None;
            if until == first + BOOST_LIMIT {
                no_boost_until = Some(until + BOOST_TIME);
            }
        }

        balancer.check_moved();

        if pressure {
            balancer.relieve(&mut history, now);
        } else if history
            .last()
            .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
        {
            history.clear();
        }
    };

    balancer.uncap();
    let scopes: Vec<String> = balancer.moved.keys().cloned().collect();
    for scope in scopes {
        balancer.restore(&scope);
    }
    result
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("deskbalance: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_survive_odd_names() {
        let stat = "4242 (a) b (c)) S 1 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 98765 0";
        let fields = stat_fields(stat);
        assert_eq!(fields[0], "S");
        assert_eq!(fields[2], "4242");
        assert_eq!(fields[19], "98765");
    }

    #[test]
    fn io_stat_sums_reads_and_writes() {
        let stat = "8:0 rbytes=1000 wbytes=24 rios=3 wios=1 dbytes=5\n259:0 rbytes=6 wbytes=0";
        assert_eq!(io_stat_bytes(stat), 1030);
    }

    #[test]
    fn rates_skip_new_units() {
        let before = Snapshot::from([("a".into(), (0, 100))]);
        let after = Snapshot::from([("a".into(), (0, 300)), ("b".into(), (0, 50))]);
        let rates = io_rates(&before, &after, 2.0);
        assert_eq!(rates.len(), 1);
        assert_eq!(rates["a"], 100.0);
    }

    #[test]
    fn busiest_group_needs_half_heavy_io() {
        let groups = HashMap::from([(10, 7), (11, 9)]);
        let small = HashMap::from([(10, 1 << 20), (11, 0)]);
        assert_eq!(busiest_group(&small, &groups), None);
        let big = HashMap::from([(10, 1 << 20), (11, 4 << 20)]);
        assert_eq!(busiest_group(&big, &groups), Some(9));
    }

    #[test]
    fn busiest_group_sums_its_processes() {
        let groups = HashMap::from([(10, 7), (11, 7), (12, 9)]);
        let delta = HashMap::from([(10, 2 << 20), (11, 2 << 20), (12, 3 << 20)]);
        assert_eq!(busiest_group(&delta, &groups), Some(7));
    }

    #[test]
    fn return_scope_keeps_one_suffix() {
        assert_eq!(
            return_scope("app-term-1.scope", "deskbalance-700-3.scope"),
            "app-term-1-deskbalance-700-3.scope"
        );
        assert_eq!(
            return_scope(
                "app-term-1-deskbalance-700-3.scope",
                "deskbalance-700-9.scope"
            ),
            "app-term-1-deskbalance-700-9.scope"
        );
    }
}
