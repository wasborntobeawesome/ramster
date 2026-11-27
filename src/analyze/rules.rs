//! Rules: each looks at a snapshot and proposes findings.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use super::facts::{self, Facts, Need};
use super::{custom, dev_kind, devtools, is_agent, is_system_path, Idleness, Snapshot};
use crate::config::Config;
use crate::probe::{self, Proc};
use crate::util::{base_name, fmt_mb, fmt_minutes, home, now_unix, slug, tildify, uid};

/// Rule names of the built-in rules (custom rules may not reuse them).
pub const BUILTIN_RULES: &[&str] = &[
    "gradle-daemon",
    "kotlin-daemon",
    "maven-daemon",
    "gradle-idle-timeout",
    "android-emulator",
    "ios-simulator",
    "docker",
    "vm",
    "orphan-mcp",
    "orphan-lsp",
    "detached-process",
    "nx-daemon",
    "turbo-daemon",
    "idle-app",
    "duplicate-mcp",
    "agents",
    "browser",
    "ollama-model",
    "llm-server",
    "lm-studio",
    "sourcekitservice",
    "xcbbuildservice",
    "swbbuildservice",
    "jetbrains",
    "extension-host",
    "dev-server",
    "test-workers",
    "test-watch",
    "electron",
];

/// Restarts on demand / nobody is using it; no data loss.
pub const SAFE: &str = "safe";
/// Stops something a human might still want.
pub const CONFIRM: &str = "confirm";
/// Advice only.
pub const INFO: &str = "info";

#[derive(Clone, Debug, Default)]
pub enum Action {
    #[default]
    None,
    /// SIGTERM the PIDs (verified by start time).
    Term,
    /// Run a command.
    Cmd(Vec<String>),
    /// Ask a GUI app to quit.
    Quit(String),
}

/// One recommendation. IDs are stable for the life of the process (rule:pid).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Finding {
    pub id: String,
    pub rule: String,
    pub safety: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    pub frees_mb: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tip: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<i32>,
    #[serde(skip)]
    pub act: Action,
    #[serde(skip)]
    pub starts: HashMap<i32, i64>,
}

impl Finding {
    pub fn actionable(&self) -> bool {
        self.safety != INFO && !matches!(self.act, Action::None)
    }
}

pub(super) struct Ctx<'a> {
    pub s: &'a Snapshot,
    pub cfg: &'a Config,
    pub facts: &'a Facts,
    pub me: u32,
    pub now: i64,
}

type Rule = fn(&Ctx) -> Vec<Finding>;

/// Run every rule; safe first, then confirm, then info, biggest wins first.
pub fn suggest(s: &Snapshot, cfg: &Config) -> Vec<Finding> {
    suggest_with(s, cfg, &facts::blocking(needs(s)))
}

/// Same as `suggest`, but never waits on external commands: uses cached facts
/// and refreshes them in the background. For interactive UIs.
pub fn suggest_nonblocking(s: &Snapshot, cfg: &Config) -> Vec<Finding> {
    suggest_with(s, cfg, &facts::nonblocking(needs(s)))
}

/// Which external facts the rules will need for this snapshot.
pub fn needs(s: &Snapshot) -> Need {
    let me = uid();
    let now = now_unix();
    Need {
        procs: s.procs.iter().any(|p| detached_user_proc(p, me, now)),
        docker: s.group("Docker").is_some_and(|g| g.mb >= 1024),
    }
}

/// Run the rules against given facts (tests inject their own).
pub fn suggest_with(s: &Snapshot, cfg: &Config, facts: &Facts) -> Vec<Finding> {
    let c = Ctx {
        s,
        cfg,
        facts,
        me: uid(),
        now: now_unix(),
    };
    // Order matters: when two rules would act on the same process, the first
    // one wins (the user's own rules, then specific rules, then generic ones).
    let rules: &[Rule] = &[
        custom::run,
        build_daemons,
        emulator,
        simulator,
        docker,
        devtools::local_llms,
        devtools::xcode,
        devtools::dev_servers,
        devtools::test_runners,
        js_daemons,
        orphans,
        idle_apps,
        other_vms,
        duplicate_mcp,
        agents,
        browsers,
        devtools::jetbrains,
        devtools::extension_hosts,
        devtools::electron,
    ];
    let mut out: Vec<Finding> = Vec::new();
    // Processes an earlier actionable finding already covers.
    let mut claimed: HashSet<i32> = HashSet::new();
    for rule in rules {
        for f in rule(&c) {
            let mut fields: Vec<&str> = vec![&f.title, &f.detail];
            for pid in &f.pids {
                if let Some(p) = s.proc(*pid) {
                    fields.push(&p.name);
                    fields.push(&p.args);
                    if let Some(g) = s.group_of.get(pid) {
                        fields.push(g);
                    }
                }
            }
            if cfg.protected(&fields) {
                continue;
            }
            if f.actionable() {
                if f.pids.iter().any(|p| claimed.contains(p)) {
                    continue; // never count or signal the same process twice
                }
                claimed.extend(f.pids.iter().copied());
            }
            out.push(f);
        }
    }
    let rank = |f: &Finding| match f.safety {
        SAFE => 0,
        CONFIRM => 1,
        _ => 2,
    };
    out.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b.frees_mb.cmp(&a.frees_mb)));
    out
}

impl Ctx<'_> {
    /// Start times that identify these PIDs when the finding is carried out.
    pub fn starts_of(&self, pids: &[i32]) -> HashMap<i32, i64> {
        pids.iter()
            .filter_map(|&p| self.s.proc(p).map(|q| (p, q.start)))
            .collect()
    }

    pub fn age(&self, p: &Proc) -> String {
        if p.start <= 0 {
            "?".into()
        } else {
            fmt_minutes((self.now - p.start) as f64 / 60.0)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn term(
        &self,
        rule: &str,
        root: &Proc,
        pids: Vec<i32>,
        safety: &'static str,
        title: String,
        detail: String,
        action: &str,
    ) -> Finding {
        let starts = self.starts_of(&pids);
        Finding {
            id: format!("{rule}:{}", root.pid),
            rule: rule.into(),
            safety,
            title,
            detail,
            frees_mb: self.s.bytes_of(&pids) >> 20,
            action: action.into(),
            pids,
            act: Action::Term,
            starts,
            ..Default::default()
        }
    }
}

// ---- tiny parsers (instead of a regex dependency) ------------------------------

/// Text right after `marker` up to the first whitespace.
pub(super) fn after<'a>(s: &'a str, marker: &str) -> Option<&'a str> {
    let i = s.find(marker)? + marker.len();
    let rest = s[i..].trim_start();
    let tok = rest.split_whitespace().next()?;
    (!tok.is_empty()).then_some(tok)
}

fn gradle_version(args: &str) -> Option<String> {
    if let Some(i) = args.find("gradle-daemon-main-") {
        let rest = &args[i + "gradle-daemon-main-".len()..];
        if let Some(j) = rest.find(".jar") {
            return Some(rest[..j].to_string());
        }
    }
    after(args, "GradleDaemon")
        .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        .map(String::from)
}

// ---- build daemons ---------------------------------------------------------------

fn build_daemons(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut versions: Vec<String> = Vec::new();
    for p in &c.s.procs {
        let Some(kind) = dev_kind(p) else { continue };
        if !matches!(kind, "Gradle daemons" | "Kotlin daemon" | "Maven daemon") {
            continue;
        }
        if p.ppid != 1 && kind != "Gradle daemons" {
            continue; // only detached daemons
        }
        let pids = c.s.tree(p.pid);
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue; // a build is running; never interrupt it
        }
        let (rule, title) = match kind {
            "Gradle daemons" => {
                let v = gradle_version(&p.args).unwrap_or_default();
                if !v.is_empty() {
                    versions.push(v.clone());
                }
                (
                    "gradle-daemon",
                    format!("Gradle daemon {v}").trim().to_string(),
                )
            }
            "Kotlin daemon" => ("kotlin-daemon", "Kotlin compile daemon".to_string()),
            _ => ("maven-daemon", "Maven daemon (mvnd)".to_string()),
        };
        let mut detail = format!("{idle}, up {}", c.age(p));
        if let Some(x) = after(&p.args, "-Xmx") {
            detail += &format!(", heap limit {x}");
        }
        let mut safety = SAFE;
        if idle.minutes == 0.0 && p.start > 0 && c.now - p.start < c.cfg.idle_minutes as i64 * 60 {
            // Started recently and no history: probably still warm for the next build.
            safety = CONFIRM;
            detail += ", started recently (the next build would be cold)";
        }
        out.push(c.term(
            rule,
            p,
            pids,
            safety,
            title,
            detail,
            "stop it (SIGTERM); the next build starts a fresh one",
        ));
    }
    if versions.len() > 1 {
        let mut uniq = versions.clone();
        uniq.dedup();
        uniq.sort();
        uniq.dedup();
        if let Some(f) = out.iter_mut().find(|f| f.rule == "gradle-daemon") {
            f.tip = format!(
                "{} Gradle daemons are running (versions {}): every Gradle version and JVM-arg combination gets its own.",
                versions.len(),
                uniq.join(", ")
            );
        }
    }
    if !versions.is_empty() && !gradle_has_idle_timeout() {
        out.push(Finding {
            id: "gradle-idle-timeout".into(),
            rule: "gradle-idle-timeout".into(),
            safety: INFO,
            title: "Gradle daemons stay resident for 3 h after the last build".into(),
            detail: "No org.gradle.daemon.idletimeout in ~/.gradle/gradle.properties.".into(),
            tip: "Add `org.gradle.daemon.idletimeout=1800000` (30 min) to ~/.gradle/gradle.properties so idle daemons exit by themselves.".into(),
            ..Default::default()
        });
    }
    out
}

fn gradle_has_idle_timeout() -> bool {
    let dir = std::env::var("GRADLE_USER_HOME").unwrap_or_else(|_| format!("{}/.gradle", home()));
    std::fs::read_to_string(format!("{dir}/gradle.properties"))
        .map(|s| properties_set(&s, "org.gradle.daemon.idletimeout"))
        .unwrap_or(false)
}

/// Is `key` set by a non-comment line of a .properties file?
pub fn properties_set(text: &str, key: &str) -> bool {
    text.lines().map(str::trim).any(|l| {
        !l.starts_with('#')
            && !l.starts_with('!')
            && l.strip_prefix(key).is_some_and(|rest| {
                let rest = rest.trim_start();
                rest.starts_with('=') || rest.starts_with(':')
            })
    })
}

// ---- emulators / simulators ------------------------------------------------------

fn emulator(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in &c.s.procs {
        if dev_kind(p) != Some("Android emulator") {
            continue;
        }
        let avd = after(&p.args, "-avd").unwrap_or("emulator").to_string();
        let pids = c.s.tree(p.pid);
        let mut detail = format!("up {}", c.age(p));
        if let Some(ram) = avd_ram(&avd) {
            detail += &format!(
                ", guest RAM configured {ram} but the host process holds {}",
                fmt_mb(c.s.bytes_of(&pids) >> 20)
            );
        }
        let mut f = c.term(
            "android-emulator",
            p,
            pids,
            CONFIRM,
            format!("Android emulator {avd}"),
            detail,
            "shut the emulator down (SIGTERM, same as closing its window)",
        );
        if p.start > 0 && c.now - p.start > 6 * 3600 {
            f.tip = "Long-running emulators keep growing (graphics buffers, page cache). Restart it daily, or close it when you're not testing.".into();
        }
        out.push(f);
    }
    out
}

fn avd_ram(avd: &str) -> Option<String> {
    let cfg =
        std::fs::read_to_string(format!("{}/.android/avd/{avd}.avd/config.ini", home())).ok()?;
    let v = cfg
        .lines()
        .find_map(|l| l.strip_prefix("hw.ramSize="))?
        .trim();
    Some(if v.chars().all(|c| c.is_ascii_digit()) {
        format!("{v} MB")
    } else {
        v.to_string()
    })
}

fn simulator(c: &Ctx) -> Vec<Finding> {
    let Some(g) = c.s.group("iOS Simulator") else {
        return vec![];
    };
    if g.mb < 300 {
        return vec![];
    }
    let n = g
        .pids
        .iter()
        .filter(|&&p| c.s.proc(p).is_some_and(|q| q.name == "launchd_sim"))
        .count();
    vec![Finding {
        id: "ios-simulator".into(),
        rule: "ios-simulator".into(),
        safety: CONFIRM,
        title: format!("{n} booted iOS Simulator(s)"),
        detail: format!("{} processes", g.procs),
        frees_mb: g.mb,
        action: "xcrun simctl shutdown all".into(),
        pids: g.pids.clone(),
        act: Action::Cmd(vec![
            "xcrun".into(),
            "simctl".into(),
            "shutdown".into(),
            "all".into(),
        ]),
        ..Default::default()
    }]
}

// ---- Docker & other VMs ----------------------------------------------------------

fn docker(c: &Ctx) -> Vec<Finding> {
    let Some(g) = c.s.group("Docker") else {
        return vec![];
    };
    if g.mb < 1024 {
        return vec![];
    }
    let info = c.facts.docker.clone().unwrap_or_default();
    let stop = if info.stop.is_empty() {
        vec![
            "osascript".into(),
            "-e".into(),
            r#"tell application "Docker" to quit"#.into(),
        ]
    } else {
        info.stop.clone()
    };
    let stop_label = if stop[0] == "osascript" {
        "quit Docker Desktop".to_string()
    } else {
        "docker desktop stop".to_string()
    };
    let mut f = Finding {
        id: "docker".into(),
        rule: "docker".into(),
        safety: CONFIRM, // docker commands fail until it's started again
        frees_mb: g.mb,
        pids: g.pids.clone(),
        tip: "Docker Desktop reserves its VM memory up front. Settings → Resources: turn on Resource Saver and set the memory limit to what your containers need (often 3–4 GB). OrbStack or Colima give memory back dynamically.".into(),
        act: Action::Cmd(stop),
        ..Default::default()
    };
    match info.running {
        Some(0) => {
            f.title = "Docker VM is running with no containers".into();
            f.detail = format!("the VM holds {} for nothing", fmt_mb(g.mb));
            f.action = stop_label;
        }
        Some(n) => {
            f.title = format!("Docker VM holds {} for {n} container(s)", fmt_mb(g.mb));
            if info.used_mb > 0 {
                f.detail = format!(
                    "containers actually use {}; the rest is VM overhead and cache",
                    fmt_mb(info.used_mb)
                );
            }
            f.action = format!("{stop_label} (stops all containers)");
        }
        None => {
            f.title = format!("Docker VM holds {}", fmt_mb(g.mb));
            f.detail = if c.facts.docker.is_none() {
                "checking containers…".into()
            } else {
                "could not ask Docker how many containers are running".into()
            };
            f.action = format!("{stop_label} (stops any running containers)");
        }
    }
    vec![f]
}

fn other_vms(c: &Ctx) -> Vec<Finding> {
    const VMS: &[&str] = &[
        "OrbStack",
        "UTM",
        "Parallels Desktop",
        "VMware Fusion",
        "Podman Desktop",
        "Rancher Desktop",
        "limactl",
        "colima",
        "qemu-system-aarch64",
        "qemu-system-x86_64",
    ];
    VMS.iter()
        .filter_map(|&name| c.s.group(name).filter(|g| g.mb >= 2048))
        .map(|g| Finding {
            id: format!("vm:{}", slug(&g.name)),
            rule: "vm".into(),
            safety: INFO,
            title: format!("{} VM holds {}", g.name, fmt_mb(g.mb)),
            frees_mb: g.mb,
            pids: g.pids.clone(),
            tip: "Stop VMs you aren't using, or lower their memory allocation.".into(),
            ..Default::default()
        })
        .collect()
}

// ---- orphans (parent agent/terminal died) ---------------------------------------------

/// `mcp` as a whole word, or common MCP package names.
pub fn is_mcp(p: &Proc) -> bool {
    if p.args.is_empty()
        || is_agent(p)
        || p.path.contains(".app/Contents/")
        || p.path.contains("ramster")
    {
        return false;
    }
    let a = p.args.to_lowercase();
    a.contains("modelcontextprotocol")
        || a.contains("mcp-server")
        || a.contains("server-mcp")
        || a.split(|ch: char| ch.is_whitespace() || "/_@-.".contains(ch))
            .any(|t| t == "mcp")
}

fn is_lsp(p: &Proc) -> bool {
    let a = format!("{} {}", p.name, p.args).to_lowercase();
    const SUBSTR: &[&str] = &[
        "language-server",
        "languageserver",
        "langserver",
        "rust-analyzer",
        "sourcekit-lsp",
        "tsserver",
        "org.eclipse.jdt.ls",
        "solargraph",
        "intelephense",
    ];
    SUBSTR.iter().any(|s| a.contains(s))
        || a.split_whitespace().any(|t| {
            let b = base_name(t);
            matches!(b, "gopls" | "clangd" | "zls") || b.ends_with("-lsp")
        })
}

const INTERPRETERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "ruby", "java", "dotnet", "php", "esbuild", "uv",
    "npm", "pnpm", "yarn",
];

fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.contains(&name) || (name.starts_with("python3.") && name.len() <= 10)
}

/// dev_kind buckets that `detached_user_proc` leaves to their own rules.
const DAEMON_KINDS: &[&str] = &[
    "Gradle daemons",
    "Kotlin daemon",
    "Maven daemon",
    "Android emulator",
    "iOS Simulator",
    "nx daemon",
    "turbo daemon",
];

/// A process of ours that was reparented to launchd (its terminal or agent is gone),
/// excluding apps, system services, dev daemons, agents and process managers.
fn detached_user_proc(p: &Proc, me: u32, now: i64) -> bool {
    p.ppid == 1
        && p.uid == me
        && p.readable
        && !p.path.contains(".app/Contents/")
        && p.app.is_empty() // a desktop app (Linux), not a forgotten tool
        && !is_system_path(&p.path)
        // Build daemons, emulators and nx/turbo have their own rules. Dev servers,
        // test runners and LLM servers do too, but only above their thresholds,
        // so they stay orphan candidates (duplicates are dropped in suggest_with).
        && !dev_kind(p).is_some_and(|k| DAEMON_KINDS.contains(&k))
        && p.start > 0
        && now - p.start > 120
        && !is_agent(p)
        && !is_process_manager(p)
}

/// What we know about a process that lost its parent.
pub(super) struct OrphanEvidence {
    /// Idle, launchd/listening facts gathered, and not serving clients.
    pub proven: bool,
    /// ", listens on a TCP port ..." / ", not verified yet" / "".
    pub note: &'static str,
}

impl Ctx<'_> {
    fn detached_user_proc(&self, p: &Proc) -> bool {
        detached_user_proc(p, self.me, self.now)
    }

    /// None when a LaunchAgent runs it on purpose (managed, not orphaned).
    pub(super) fn orphan_evidence(
        &self,
        p: &Proc,
        pids: &[i32],
        idle: &Idleness,
    ) -> Option<OrphanEvidence> {
        if self.facts.launchd_pids.contains(&p.pid) {
            return None;
        }
        let listening = pids.iter().any(|q| self.facts.listening_pids.contains(q));
        let note = if listening {
            ", listens on a TCP port (may be a shared server)"
        } else if !self.facts.procs_known {
            ", not verified yet"
        } else {
            ""
        };
        Some(OrphanEvidence {
            proven: idle.known && idle.idle && self.facts.procs_known && !listening,
            note,
        })
    }
}

/// Has this directory really been deleted? Permission errors (e.g. TCC-protected
/// ~/Documents as seen from the launchd sampler) do not count as deleted.
pub fn dir_gone(path: &str) -> bool {
    !path.is_empty() && path != "/" && matches!(Path::new(path).try_exists(), Ok(false))
}

/// Supervisors whose job is to keep things running: never treat them as forgotten.
fn is_process_manager(p: &Proc) -> bool {
    const NAMES: &[&str] = &[
        "tmux",
        "screen",
        "zellij",
        "supervisord",
        "pm2",
        "forever",
        "overmind",
        "foreman",
    ];
    NAMES.contains(&p.name.as_str())
        || p.args.contains("PM2 v")
        || p.args.contains("God Daemon")
        || p.args
            .split_whitespace()
            .take(3)
            .any(|t| NAMES.contains(&base_name(t)))
}

pub(super) fn short_cmd(p: &Proc) -> String {
    let parts: Vec<&str> = p
        .args
        .split_whitespace()
        .take(5)
        .map(|x| if x.starts_with('/') { base_name(x) } else { x })
        .collect();
    if parts.is_empty() {
        return p.name.clone();
    }
    let s = parts.join(" ");
    if s.chars().count() > 70 {
        format!("{}…", s.chars().take(70).collect::<String>())
    } else {
        s
    }
}

fn orphans(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in &c.s.procs {
        if !c.detached_user_proc(p) {
            continue;
        }
        let pids = c.s.tree(p.pid);
        let mb = c.s.bytes_of(&pids) >> 20;
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue; // in use right now
        }
        // SAFE only when proven: idle, not a launchd job, not serving clients.
        let Some(OrphanEvidence { proven, note }) = c.orphan_evidence(p, &pids, &idle) else {
            continue;
        };
        let tier = if proven { SAFE } else { CONFIRM };
        let short = short_cmd(p);
        if is_mcp(p) && mb >= 10 {
            out.push(c.term(
                "orphan-mcp",
                p,
                pids,
                tier,
                format!("Orphaned MCP server: {short}"),
                format!(
                    "its agent session is gone (parent exited), up {}, {idle}{note}",
                    c.age(p)
                ),
                "stop it; agents start their own MCP servers when needed",
            ));
        } else if is_lsp(p) && mb >= 30 {
            out.push(c.term(
                "orphan-lsp",
                p,
                pids,
                tier,
                format!("Orphaned language server: {short}"),
                format!("its editor/agent is gone, up {}, {idle}{note}", c.age(p)),
                "stop it; editors restart language servers on demand",
            ));
        } else if is_interpreter(base_name(&p.name)) && mb >= 150 && idle.known && idle.idle {
            let cwd = probe::cwd(p.pid);
            let (mut safety, mut why) = (CONFIRM, "detached from its terminal/agent");
            if dir_gone(&cwd) {
                why = "its working directory no longer exists";
                if proven {
                    safety = SAFE;
                }
            }
            let mut detail = format!("{why}, up {}, {idle}{note}", c.age(p));
            if !cwd.is_empty() {
                detail += &format!(", cwd {}", tildify(&cwd));
            }
            out.push(c.term(
                "detached-process",
                p,
                pids,
                safety,
                format!("Detached {} process: {short}", p.name),
                detail,
                "stop it (SIGTERM)",
            ));
        }
    }
    out
}

/// nx / turbo daemons respawn on the next command, so stopping idle ones is safe.
fn js_daemons(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in &c.s.procs {
        let Some(k) = dev_kind(p).filter(|k| matches!(*k, "nx daemon" | "turbo daemon")) else {
            continue;
        };
        if c.s.proc(p.ppid).and_then(dev_kind) == Some(k) {
            continue; // a worker of a daemon we already count
        }
        let pids = c.s.tree(p.pid);
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue;
        }
        let cwd = probe::cwd(p.pid);
        let mut detail = format!("{idle}, up {}", c.age(p));
        if !cwd.is_empty() {
            detail += &format!(", workspace {}", tildify(&cwd));
            if dir_gone(&cwd) {
                detail += " (deleted)";
            }
        }
        let tool = k.split_whitespace().next().unwrap_or(k);
        out.push(c.term(
            &slug(k),
            p,
            pids,
            SAFE,
            k.to_string(),
            detail,
            &format!("stop it; it restarts automatically on the next {tool} command"),
        ));
    }
    out
}

// ---- idle GUI apps (needs the daemon's history) --------------------------------------

const NEVER_QUIT: &[&str] = &[
    "Finder",
    "Dock",
    "Conductor",
    "Terminal",
    "iTerm",
    "iTerm2",
    "Ghostty",
    "WezTerm",
    "Warp",
    "Alacritty",
    "kitty",
    "Docker",
    "Google Chrome",
    "Safari",
    "Arc",
    "Firefox",
    "Brave Browser",
    "Microsoft Edge",
    "Activity Monitor",
    "System Settings",
    "Claude",
    "Tailscale",
    "1Password",
    "BetterDisplay",
    "Hotspot Shield",
];

/// Apps a dedicated rule already handles better than "quit it".
const OWN_RULE: &[&str] = &["Ollama", "LM Studio"];

fn idle_apps(c: &Ctx) -> Vec<Finding> {
    let allow = &c.cfg.quit_candidates;
    c.s.groups
        .iter()
        .filter(|g| g.is_app && g.mb >= 800 && !NEVER_QUIT.contains(&g.name.as_str()))
        .filter(|g| !OWN_RULE.contains(&g.name.as_str()))
        .filter(|g| allow.is_empty() || allow.contains(&g.name))
        .filter_map(|g| {
            let idle = c.s.idle_group(&g.name, c.cfg.idle_minutes);
            (idle.known && idle.idle).then(|| Finding {
                id: format!("idle-app:{}", slug(&g.name)),
                rule: "idle-app".into(),
                safety: CONFIRM,
                title: format!("{} is idle", g.name),
                detail: format!("{} across {} processes, {idle}", fmt_mb(g.mb), g.procs),
                frees_mb: g.mb,
                action: "quit it normally (it may ask to save)".into(),
                pids: g.pids.clone(),
                act: Action::Quit(g.name.clone()),
                starts: c.starts_of(&g.pids),
                ..Default::default()
            })
        })
        .collect()
}

// ---- advice --------------------------------------------------------------------------

pub fn mcp_key(p: &Proc) -> String {
    const SKIP: &[&str] = &[
        "node", "npm", "npx", "exec", "-y", "--yes", "bun", "bunx", "uv", "uvx", "run", "python",
        "python3", "-m", "deno", "pnpm", "dlx", "--",
    ];
    p.args
        .split_whitespace()
        .map(base_name)
        .filter(|b| !SKIP.contains(b) && !b.starts_with("--"))
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn duplicate_mcp(c: &Ctx) -> Vec<Finding> {
    let mut m: BTreeMap<String, (usize, u64, Vec<i32>)> = BTreeMap::new();
    for p in &c.s.procs {
        if !is_mcp(p) || p.ppid == 1 {
            continue;
        }
        if c.s.proc(p.ppid).is_some_and(is_mcp) {
            continue; // child of an MCP wrapper (npm exec → node)
        }
        let k = mcp_key(p);
        if k.is_empty() {
            continue;
        }
        let pids = c.s.tree(p.pid);
        let e = m.entry(k).or_default();
        e.0 += 1;
        e.1 += c.s.bytes_of(&pids) >> 20;
        e.2.extend(pids);
    }
    m.into_iter()
        .filter(|(_, (n, mb, _))| *n >= 2 && *mb >= 150)
        .map(|(k, (n, mb, pids))| Finding {
            id: format!("duplicate-mcp:{}", slug(&k)),
            rule: "duplicate-mcp".into(),
            safety: INFO,
            title: format!("MCP server \"{k}\" runs {n}× (one per agent session)"),
            detail: format!("{} total", fmt_mb(mb)),
            frees_mb: mb,
            pids,
            tip: "Every agent session starts its own stdio MCP servers. Enable heavy servers only in the projects that need them (project-level config or plugin), or run one shared HTTP MCP server.".into(),
            ..Default::default()
        })
        .collect()
}

fn agents(c: &Ctx) -> Vec<Finding> {
    let Some(g) = c.s.group("AI agents") else {
        return vec![];
    };
    if g.mb < 1024 {
        return vec![];
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in g.pids.iter().filter_map(|&p| c.s.proc(p)) {
        if is_agent(p) && !c.s.proc(p.ppid).is_some_and(is_agent) {
            *counts.entry(super::agent_label(p)).or_default() += 1;
        }
    }
    let total: usize = counts.values().sum();
    let parts: Vec<String> = counts
        .iter()
        .map(|(name, count)| format!("{count}×{name}"))
        .collect();
    let per = if total > 0 { g.mb / total as u64 } else { 0 };
    vec![Finding {
        id: "agents".into(),
        rule: "agents".into(),
        safety: INFO,
        title: format!("{total} AI agent sessions use {} (with their MCP servers and tools)", fmt_mb(g.mb)),
        detail: format!("{}, about {} per session", parts.join(", "), fmt_mb(per)),
        frees_mb: g.mb,
        tip: "Archive finished agent sessions: each one keeps its CLI, MCP servers and spawned tools alive.".into(),
        ..Default::default()
    }]
}

fn browsers(c: &Ctx) -> Vec<Finding> {
    const TIPS: &[(&str, &str)] = &[
        ("Google Chrome", "Turn on Memory Saver (chrome://settings/performance) and close tab groups you aren't using."),
        ("Safari", "Each Safari tab is its own WebKit process. Close the heavy ones (Activity Monitor shows them by tab name)."),
        ("Arc", "Lower Arc's auto-archive time for idle tabs."),
        ("Brave Browser", "Turn on Memory Saver (brave://settings/system)."),
        ("Microsoft Edge", "Turn on Sleeping Tabs (edge://settings/system)."),
        ("Firefox", "about:unloads lets you unload heavy tabs."),
    ];
    TIPS.iter()
        .filter_map(|(name, tip)| {
            let g = c.s.group(name).filter(|g| g.mb >= 2500)?;
            Some(Finding {
                id: format!("browser:{}", slug(name)),
                rule: "browser".into(),
                safety: INFO,
                title: format!("{name} uses {} across {} processes", fmt_mb(g.mb), g.procs),
                detail: "mostly tabs and extensions".into(),
                frees_mb: g.mb,
                tip: tip.to_string(),
                ..Default::default()
            })
        })
        .collect()
}
