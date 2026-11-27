//! Linux backend: /proc and /sys, no subprocesses.
//!
//! - Per process: PSS + SwapPss from `smaps_rollup` (your own processes; the
//!   kernel refuses others without root), CPU and start time from `stat`.
//! - Apps: the systemd scope or service a process runs in (what desktop
//!   launchers create), so browser helpers group under their browser.
//! - Orphans: processes re-parented to the user's service manager (a subreaper)
//!   are reported with ppid 1, like processes re-parented to launchd on macOS.
//! - System: /proc/meminfo, PSI (/proc/pressure/memory), zswap and zram.

use std::collections::HashSet;
use std::sync::OnceLock;

use super::procfs::{self, Stat, PF_KTHREAD};
use super::{clip, Mem, Proc};

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn clk_tck() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        t if t > 0 => t as u64,
        _ => 100,
    })
}

fn boot_time() -> i64 {
    static B: OnceLock<i64> = OnceLock::new();
    *B.get_or_init(|| {
        read("/proc/stat")
            .and_then(|s| procfs::parse_btime(&s))
            .unwrap_or(0)
    })
}

fn stat(pid: i32) -> Option<Stat> {
    procfs::parse_stat(&read(&format!("/proc/{pid}/stat"))?)
}

fn start_of(st: &Stat) -> i64 {
    boot_time() + (st.starttime / clk_tck()) as i64
}

fn link(path: &str) -> String {
    std::fs::read_link(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn cmdline(pid: i32) -> String {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let s: Vec<u8> = raw.iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
    clip(String::from_utf8_lossy(&s).trim_end())
}

/// The desktop app of a process, if it was started from a launcher.
fn app_of(pid: i32, name: &str) -> String {
    let Some(cg) = read(&format!("/proc/{pid}/cgroup")) else {
        return String::new();
    };
    let Some(app) = procfs::unit_of_cgroup(&cg).and_then(procfs::app_of_unit) else {
        return String::new();
    };
    // Shells and tools started inside a terminal share its scope (Konsole,
    // kitty...); they keep their own names, only the terminal itself is the app.
    if procfs::is_terminal_app(&app) {
        let short = app.rsplit('.').next().unwrap_or(&app).to_lowercase();
        let n = name.to_lowercase();
        if !(n.contains(&short) || short.contains(&n) || n.contains("terminal")) {
            return String::new();
        }
    }
    app
}

pub fn procs() -> Result<Vec<Proc>, String> {
    let dir = std::fs::read_dir("/proc").map_err(|e| format!("/proc: {e}"))?;
    let me = unsafe { libc::getuid() };
    let tck = clk_tck();
    let mut out = Vec::new();
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(st) = stat(pid) else { continue }; // exited meanwhile
        if st.flags & PF_KTHREAD != 0 {
            continue;
        }
        let uid = read(&format!("/proc/{pid}/status"))
            .and_then(|s| procfs::parse_status_uid(&s))
            .unwrap_or(u32::MAX);
        let mut p = Proc {
            pid,
            ppid: st.ppid,
            uid,
            name: st.comm.clone(),
            start: start_of(&st),
            cpu_ns: (st.utime + st.stime) * (1_000_000_000 / tck),
            resp_pid: pid,
            ..Default::default()
        };
        if uid == me {
            if let Some(s) = read(&format!("/proc/{pid}/smaps_rollup")) {
                p.footprint = procfs::parse_smaps_rollup(&s);
                p.readable = true;
            }
            p.path = link(&format!("/proc/{pid}/exe"));
            p.args = cmdline(pid);
            p.app = app_of(pid, &p.name);
        }
        out.push(p);
    }
    if out.is_empty() {
        return Err("no processes readable in /proc".into());
    }
    reparent_to_init(&mut out);
    Ok(out)
}

/// Children of a user service manager (`systemd --user`, a subreaper that
/// adopts orphans) count as re-parented to init, as on macOS.
fn reparent_to_init(ps: &mut [Proc]) {
    let managers: HashSet<i32> = ps
        .iter()
        .filter(|p| p.pid != 1 && p.name == "systemd")
        .map(|p| p.pid)
        .collect();
    for p in ps.iter_mut() {
        if managers.contains(&p.ppid) {
            p.ppid = 1;
        }
    }
}

pub fn memory() -> Result<Mem, String> {
    let meminfo = read("/proc/meminfo").ok_or("cannot read /proc/meminfo")?;
    let psi = read("/proc/pressure/memory");
    let zram: u64 = std::fs::read_dir("/sys/block")
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("zram"))
                .filter_map(|e| read(&format!("{}/mm_stat", e.path().display())))
                .map(|s| procfs::parse_zram_mm_stat(&s))
                .sum()
        })
        .unwrap_or(0);
    Ok(procfs::mem_from(&meminfo, psi.as_deref(), zram))
}

pub fn cwd(pid: i32) -> String {
    link(&format!("/proc/{pid}/cwd"))
}

/// Unix start time, or -1 if the process is gone.
pub fn start_time(pid: i32) -> i64 {
    stat(pid).map(|s| start_of(&s)).unwrap_or(-1)
}

pub fn parent_of(pid: i32) -> i32 {
    stat(pid).map(|s| s.ppid).unwrap_or(0)
}

/// PIDs started on purpose by a service manager (systemd services, D-Bus
/// activation): managed, never orphans. The Linux counterpart of launchd jobs.
pub fn managed_pids() -> HashSet<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            read(&format!("/proc/{pid}/cgroup"))
                .is_some_and(|cg| procfs::unit_of_cgroup(&cg).is_some_and(procfs::is_managed_unit))
        })
        .collect()
}

/// PIDs of ours holding a listening TCP socket.
pub fn listening_pids() -> HashSet<i32> {
    let mut inodes = HashSet::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Some(s) = read(f) {
            inodes.extend(procfs::parse_listen_inodes(&s));
        }
    }
    if inodes.is_empty() {
        return HashSet::new();
    }
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
                fds.flatten().any(|fd| {
                    procfs::socket_inode(&link(&fd.path().to_string_lossy()))
                        .is_some_and(|i| inodes.contains(&i))
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_system() {
        let ps = procs().unwrap();
        let me = ps
            .iter()
            .find(|p| p.pid == std::process::id() as i32)
            .expect("own process listed");
        assert!(me.readable && me.footprint > 0 && me.start > 0, "{me:?}");
        assert!(!me.path.is_empty() && !me.args.is_empty());
        assert_eq!(start_time(me.pid), me.start);
        assert!(start_time(i32::MAX) < 0);
        let m = memory().unwrap();
        assert!(
            m.total > 0 && m.used <= m.total && (1..=4).contains(&m.pressure),
            "{m:?}"
        );
        assert!(!cwd(me.pid).is_empty());
    }
}
