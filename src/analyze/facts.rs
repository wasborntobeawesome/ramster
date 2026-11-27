//! Facts that need external commands (launchctl, lsof, docker). Rules never
//! run subprocesses themselves; they read a `Facts` value. Interactive callers
//! (the menu bar) use the cache and refresh it in the background, so the UI
//! never blocks on a slow `docker stats`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::util::{run, which};

/// What the docker CLI told us.
#[derive(Clone, Debug, Default)]
pub struct DockerInfo {
    /// Running containers; None if docker could not be asked.
    pub running: Option<usize>,
    /// Memory the containers actually use.
    pub used_mb: u64,
    /// Command that stops Docker Desktop.
    pub stop: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Whether launchd/listening info was gathered (false = unknown).
    pub procs_known: bool,
    /// PIDs of running launchd jobs in the user's domain (deliberately managed).
    pub launchd_pids: HashSet<i32>,
    /// PIDs with a listening TCP socket (servers that may have clients).
    pub listening_pids: HashSet<i32>,
    /// None = not gathered.
    pub docker: Option<DockerInfo>,
}

/// What a caller needs gathered.
#[derive(Clone, Copy, Debug, Default)]
pub struct Need {
    pub procs: bool,
    pub docker: bool,
}

const TTL: Duration = Duration::from_secs(30);

/// (when gathered, what) — None until the first gather.
static CACHE: Mutex<Option<(Instant, Facts)>> = Mutex::new(None);
static REFRESHING: AtomicBool = AtomicBool::new(false);

fn cached(need: Need) -> (Facts, bool) {
    match CACHE.lock().ok().and_then(|c| c.clone()) {
        Some((at, f)) => {
            let ok = at.elapsed() < TTL
                && (!need.procs || f.procs_known)
                && (!need.docker || f.docker.is_some());
            (f, ok)
        }
        None => (Facts::default(), false),
    }
}

/// Gather now (blocking), reusing a fresh cache entry.
pub fn blocking(need: Need) -> Facts {
    let (f, ok) = cached(need);
    if ok {
        return f;
    }
    let f = gather(need);
    store(&f);
    f
}

/// Never blocks: return whatever is cached (possibly empty) and refresh in the
/// background if it is stale or missing something.
pub fn nonblocking(need: Need) -> Facts {
    let (f, ok) = cached(need);
    if !ok && !REFRESHING.swap(true, Ordering::SeqCst) {
        std::thread::spawn(move || {
            let g = gather(need);
            store(&g);
            REFRESHING.store(false, Ordering::SeqCst);
        });
    }
    f
}

/// Warm the cache in the background (used at menu bar startup).
pub fn prefetch(need: Need) {
    let _ = nonblocking(need);
}

fn store(f: &Facts) {
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((Instant::now(), f.clone()));
    }
}

fn gather(need: Need) -> Facts {
    let mut f = Facts::default();
    if need.procs {
        f.launchd_pids = launchd_pids();
        f.listening_pids = listening_pids();
        f.procs_known = true;
    }
    if need.docker {
        f.docker = Some(docker_info());
    }
    f
}

/// Linux: systemd services and D-Bus activated processes, read from /proc.
#[cfg(target_os = "linux")]
fn launchd_pids() -> HashSet<i32> {
    crate::probe::linux::managed_pids()
}

/// Linux: /proc/net/tcp listeners matched to our processes' sockets (no lsof needed).
#[cfg(target_os = "linux")]
fn listening_pids() -> HashSet<i32> {
    crate::probe::linux::listening_pids()
}

/// `launchctl list`: "PID\tStatus\tLabel" per job; PID is "-" when not running.
#[cfg(not(target_os = "linux"))]
fn launchd_pids() -> HashSet<i32> {
    let (ok, out) = run(Duration::from_secs(5), "launchctl", &["list"]);
    if !ok {
        return HashSet::new();
    }
    parse_launchctl_list(&out)
}

pub fn parse_launchctl_list(out: &str) -> HashSet<i32> {
    out.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

/// One lsof call for every listening TCP socket we can see (our own processes).
#[cfg(not(target_os = "linux"))]
fn listening_pids() -> HashSet<i32> {
    let lsof = which("lsof", &["/usr/sbin/lsof"]).unwrap_or_else(|| "lsof".into());
    let (_, out) = run(
        Duration::from_secs(5),
        &lsof,
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fp"],
    );
    parse_lsof_pids(&out)
}

pub fn parse_lsof_pids(out: &str) -> HashSet<i32> {
    out.lines()
        .filter_map(|l| l.strip_prefix('p')?.parse().ok())
        .collect()
}

fn docker_bin() -> Option<String> {
    which(
        "docker",
        &[
            "/usr/local/bin/docker",
            "/opt/homebrew/bin/docker",
            "/Applications/Docker.app/Contents/Resources/bin/docker",
        ],
    )
}

fn docker_info() -> DockerInfo {
    let mut d = DockerInfo {
        stop: vec![
            "osascript".into(),
            "-e".into(),
            r#"tell application "Docker" to quit"#.into(),
        ],
        ..Default::default()
    };
    let Some(bin) = docker_bin() else { return d };
    let (ok, out) = run(Duration::from_secs(4), &bin, &["ps", "-q"]);
    if ok {
        d.running = Some(out.split_whitespace().count());
    }
    if run(Duration::from_secs(3), &bin, &["desktop", "version"]).0 {
        d.stop = vec![bin.clone(), "desktop".into(), "stop".into()];
    }
    if d.running.unwrap_or(0) > 0 {
        let (ok, out) = run(
            Duration::from_secs(8),
            &bin,
            &["stats", "--no-stream", "--format", "{{.MemUsage}}"],
        );
        if ok {
            d.used_mb = parse_docker_mem(&out);
        }
    }
    d
}

/// Sum of `docker stats` MemUsage lines ("12.25MiB / 7.747GiB" → 12).
pub fn parse_docker_mem(out: &str) -> u64 {
    let mut total = 0.0;
    for line in out.lines() {
        let first = line.split('/').next().unwrap_or("").trim();
        let end = first
            .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
            .unwrap_or(first.len());
        let Ok(v) = first[..end].parse::<f64>() else {
            continue;
        };
        total += match first[end..].trim().chars().next() {
            Some('K') | Some('k') => v / 1024.0,
            Some('G') => v * 1024.0,
            Some('T') => v * 1024.0 * 1024.0,
            Some('B') => v / 1024.0 / 1024.0,
            _ => v,
        };
    }
    total as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let l = parse_launchctl_list(
            "PID\tStatus\tLabel\n123\t0\tdev.x\n-\t0\tdev.y\n456\t-9\tcom.z\n",
        );
        assert_eq!(l, HashSet::from([123, 456]));
        assert_eq!(
            parse_lsof_pids("p100\nf5\np200\nf7\n"),
            HashSet::from([100, 200])
        );
        assert_eq!(
            parse_docker_mem("12.25MiB / 7.747GiB\n1.5GiB / 7.7GiB\n512KiB / 1GiB\n"),
            12 + 1536
        );
    }
}
