//! Turns raw processes into app groups, findings and reports.

pub mod act;
pub mod facts;
pub mod report;
pub mod rules;

use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::history::{self, Activity, Key, Sample};
use crate::probe::{self, Mem, Proc};
use crate::util::{base_name, fmt_minutes, now_unix};

/// Memory attributed to one app or tool (all its helper processes).
#[derive(Clone, Debug, Serialize)]
pub struct Group {
    pub name: String,
    pub mb: u64,
    pub procs: usize,
    /// Percent of one core; -1 if not measured.
    pub cpu_percent: f64,
    /// A GUI .app bundle (quittable).
    pub is_app: bool,
    #[serde(skip)]
    pub bytes: u64,
    #[serde(skip)]
    pub pids: Vec<i32>,
}

pub struct Snapshot {
    pub time: i64,
    pub mem: Mem,
    pub groups: Vec<Group>,
    pub procs: Vec<Proc>,
    pub idx: HashMap<i32, usize>,
    pub children: HashMap<i32, Vec<i32>>,
    pub group_of: HashMap<i32, String>,
    /// Percent of one core; absent = unknown.
    pub cpu: HashMap<i32, f64>,
    hist_procs: HashMap<Key, Activity>,
    hist_groups: HashMap<String, Activity>,
    hist_ok: bool,
}

/// Sample the system. With a window, sample twice to measure CPU.
pub fn take(window: Duration) -> Result<Snapshot, String> {
    let mut procs = probe::procs()?;
    let mut cpu = HashMap::new();
    if !window.is_zero() {
        let t0 = Instant::now();
        std::thread::sleep(window);
        let second = probe::procs()?;
        cpu = cpu_between(&procs, &second, t0.elapsed());
        procs = second;
    }
    Ok(Snapshot::build(procs, cpu, probe::memory()?))
}

/// Percent-of-one-core per PID between two listings (recycled PIDs skipped).
pub fn cpu_between(a: &[Proc], b: &[Proc], dt: Duration) -> HashMap<i32, f64> {
    let prev: HashMap<i32, &Proc> = a.iter().map(|p| (p.pid, p)).collect();
    let ns = dt.as_nanos().max(1) as f64;
    b.iter()
        .filter_map(|p| {
            let q = prev.get(&p.pid)?;
            (q.start == p.start && p.readable && q.readable && p.cpu_ns >= q.cpu_ns)
                .then(|| (p.pid, (p.cpu_ns - q.cpu_ns) as f64 / ns * 100.0))
        })
        .collect()
}

/// How sure we are that something is idle.
#[derive(Clone, Copy, Debug, Default)]
pub struct Idleness {
    pub idle: bool,
    pub known: bool,
    /// > 0 when backed by history.
    pub minutes: f64,
}

impl std::fmt::Display for Idleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.known, self.idle, self.minutes > 0.0) {
            (false, _, _) => write!(f, "activity unknown"),
            (true, true, true) => write!(f, "idle {}+", fmt_minutes(self.minutes)),
            (true, true, false) => write!(f, "idle right now"),
            (true, false, _) => write!(f, "busy"),
        }
    }
}

impl Snapshot {
    pub fn build(procs: Vec<Proc>, cpu: HashMap<i32, f64>, mem: Mem) -> Snapshot {
        let mut s = Snapshot {
            time: now_unix(),
            mem,
            groups: Vec::new(),
            idx: HashMap::with_capacity(procs.len()),
            children: HashMap::new(),
            group_of: HashMap::with_capacity(procs.len()),
            cpu,
            procs,
            hist_procs: HashMap::new(),
            hist_groups: HashMap::new(),
            hist_ok: false,
        };
        for (i, p) in s.procs.iter().enumerate() {
            s.idx.insert(p.pid, i);
            s.children.entry(p.ppid).or_default().push(p.pid);
        }
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Group> = HashMap::new();
        for p in &s.procs {
            let (name, is_app) = s.classify(p);
            s.group_of.insert(p.pid, name.clone());
            let g = groups.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                Group {
                    name: name.clone(),
                    mb: 0,
                    procs: 0,
                    cpu_percent: -1.0,
                    is_app,
                    bytes: 0,
                    pids: Vec::new(),
                }
            });
            g.bytes += p.footprint;
            g.procs += 1;
            g.pids.push(p.pid);
            if let Some(c) = s.cpu.get(&p.pid) {
                g.cpu_percent = g.cpu_percent.max(0.0) + c;
            }
        }
        s.groups = order
            .into_iter()
            .filter_map(|n| groups.remove(&n))
            .map(|mut g| {
                g.mb = g.bytes >> 20;
                g
            })
            .collect();
        s.groups.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        s
    }

    pub fn proc(&self, pid: i32) -> Option<&Proc> {
        self.idx.get(&pid).map(|&i| &self.procs[i])
    }

    pub fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Use the daemon's samples for idle detection.
    pub fn attach_history(&mut self, samples: &[Sample], window_secs: i64) {
        if samples.is_empty() {
            return;
        }
        let (p, g) = history::recent(samples, window_secs);
        self.hist_procs = p;
        self.hist_groups = g;
        self.hist_ok = true;
    }

    /// pid plus all descendants.
    pub fn tree(&self, pid: i32) -> Vec<i32> {
        let mut out = vec![pid];
        let mut i = 0;
        while i < out.len() && i < 4096 {
            let cur = out[i];
            if let Some(kids) = self.children.get(&cur) {
                out.extend(kids.iter().copied().filter(|&k| k != cur));
            }
            i += 1;
        }
        out
    }

    pub fn bytes_of(&self, pids: &[i32]) -> u64 {
        pids.iter()
            .filter_map(|&p| self.proc(p))
            .map(|p| p.footprint)
            .sum()
    }

    fn cpu_of(&self, pids: &[i32]) -> Option<f64> {
        let vals: Vec<f64> = pids
            .iter()
            .filter_map(|p| self.cpu.get(p).copied())
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum())
    }

    /// Judge a process tree.
    ///
    /// - A busy live reading always wins: history can be a whole sampler
    ///   interval old, and a build may have started since.
    /// - History must cover the root for long enough, and *every* process of the
    ///   tree it knows about must have stayed quiet (a quiet parent can have a
    ///   busy child).
    /// - Otherwise fall back to the live reading.
    pub fn idle_procs(&self, pids: &[i32], idle_min: u32) -> Idleness {
        let live = self.cpu_of(pids);
        if live.is_some_and(|c| c >= 2.0) {
            return Idleness {
                idle: false,
                known: true,
                minutes: 0.0,
            };
        }
        if self.hist_ok {
            let act = |pid: i32| {
                let p = self.proc(pid)?;
                self.hist_procs.get(&(p.pid, p.start))
            };
            if let Some(root) = pids.first().and_then(|&p| act(p)) {
                if root.minutes >= idle_min as f64 * 0.8 {
                    let tree_cpu: u32 =
                        pids.iter().filter_map(|&p| act(p)).map(|a| a.max_cpu).sum();
                    return Idleness {
                        idle: tree_cpu < 10,
                        known: true,
                        minutes: root.minutes,
                    };
                }
            }
        }
        match live {
            Some(c) => Idleness {
                idle: c < 2.0,
                known: true,
                minutes: 0.0,
            },
            None => Idleness::default(),
        }
    }

    /// Judge a whole app from history only.
    pub fn idle_group(&self, name: &str, idle_min: u32) -> Idleness {
        if !self.hist_ok {
            return Idleness::default();
        }
        match self.hist_groups.get(name) {
            Some(a) if a.minutes >= idle_min as f64 * 0.8 => Idleness {
                idle: a.max_cpu < 10,
                known: true,
                minutes: a.minutes,
            },
            _ => Idleness::default(),
        }
    }

    fn classify(&self, p: &Proc) -> (String, bool) {
        let mut cur = Some(p);
        let mut depth = 0;
        while let Some(c) = cur {
            if depth >= 24 {
                break;
            }
            if let Some(k) = dev_kind(c) {
                return (k.to_string(), false);
            }
            if is_agent(c) {
                return ("AI agents".into(), false);
            }
            if c.ppid <= 1 || c.ppid == c.pid {
                break;
            }
            cur = self.proc(c.ppid);
            depth += 1;
        }
        if p.resp_pid != p.pid {
            if let Some(n) = self
                .proc(p.resp_pid)
                .and_then(|r| app_name(&r.path))
                .filter(|n| !is_terminal(n))
            {
                return (n.to_string(), true);
            }
        }
        if let Some(n) = app_name(&p.path) {
            return (n.to_string(), true);
        }
        // Helpers launched with responsibility disclaimed (common for IDE and
        // agent spawns) report themselves; fall back to the nearest app ancestor.
        let mut cur = self.proc(p.ppid);
        for _ in 0..24 {
            let Some(c) = cur else { break };
            if let Some(n) = app_name(&c.path) {
                if is_terminal(n) {
                    break; // a dev server started from a terminal is its own thing
                }
                return (n.to_string(), true);
            }
            if c.ppid <= 1 || c.ppid == c.pid {
                break;
            }
            cur = self.proc(c.ppid);
        }
        (p.name.clone(), false)
    }
}

// ---- classification ---------------------------------------------------------

const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "cursor-agent",
    "gemini",
    "opencode",
    "aider",
    "amp",
    "goose",
    "droid",
    "qwen",
    "crush",
    "copilot",
    "kiro-cli",
    "auggie",
];

pub fn is_agent(p: &Proc) -> bool {
    if AGENTS.contains(&p.name.as_str()) {
        return true;
    }
    if p.name == "node" || p.name == "bun" {
        if let Some(script) = p.args.split_whitespace().nth(1) {
            return AGENTS.contains(&base_name(script));
        }
    }
    false
}

pub fn agent_label(p: &Proc) -> String {
    if AGENTS.contains(&p.name.as_str()) {
        return p.name.clone();
    }
    p.args
        .split_whitespace()
        .nth(1)
        .map(|s| base_name(s).to_string())
        .unwrap_or_else(|| p.name.clone())
}

/// Developer tools that deserve their own bucket.
pub fn dev_kind(p: &Proc) -> Option<&'static str> {
    let a = p.args.as_str();
    if a.contains("org.gradle.launcher.daemon.bootstrap.GradleDaemon") {
        Some("Gradle daemons")
    } else if a.contains("KotlinCompileDaemon") || a.contains("org.jetbrains.kotlin.daemon") {
        Some("Kotlin daemon")
    } else if a.contains("org.mvndaemon") {
        Some("Maven daemon")
    } else if p.name.starts_with("qemu-system") && a.contains("-avd") {
        Some("Android emulator")
    } else if matches!(
        p.name.as_str(),
        "launchd_sim" | "SimMetalHost" | "Simulator"
    ) {
        Some("iOS Simulator")
    } else if a.contains("/nx/src/daemon/") {
        Some("nx daemon")
    } else if p.name == "turbo" && a.contains(" daemon") {
        Some("turbo daemon")
    } else {
        None
    }
}

/// Terminal emulators: what runs inside them is reported by its own name.
fn is_terminal(app: &str) -> bool {
    matches!(
        app,
        "Terminal"
            | "iTerm"
            | "iTerm2"
            | "Ghostty"
            | "WezTerm"
            | "Warp"
            | "Alacritty"
            | "kitty"
            | "Hyper"
            | "Tabby"
            | "Rio"
    )
}

/// "Google Chrome" from ".../Google Chrome.app/Contents/...".
pub fn app_name(path: &str) -> Option<&str> {
    let i = path.find(".app/Contents/")?;
    let j = path[..i].rfind('/').map(|j| j + 1).unwrap_or(0);
    Some(&path[j..i])
}

/// OS-owned binaries (XPC services, extensions, system daemons).
pub fn is_system_path(path: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/System/",
        "/usr/libexec/",
        "/usr/sbin/",
        "/sbin/",
        "/Library/Apple/",
        "/Library/Developer/PrivateFrameworks/",
        "/Library/SystemExtensions/",
    ];
    PREFIXES.iter().any(|p| path.starts_with(p))
        || path.contains(".xpc/")
        || path.contains(".appex/")
}

#[cfg(test)]
mod tests;
