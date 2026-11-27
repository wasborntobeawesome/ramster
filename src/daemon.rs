//! Background sampler and how it runs at login (launchd on macOS, a systemd
//! user service on Linux).
//!
//! Built to stay tiny: one thread, no subprocesses on the hot path, and a slim
//! copy of the previous sample (no command lines) kept between ticks.

use std::time::{Duration, Instant};

use crate::analyze::rules::{self, SAFE};
use crate::analyze::{act, cpu_between, dev_kind, is_system_path, Snapshot};
use crate::config::{self, Config};
use crate::history::{self, Group, ProcRec, Sample, Sys};
use crate::probe::{self, Proc};
use crate::util::{fmt_mb, now_unix, run, uid};

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.ramster.daemon";

fn logf(msg: &str) {
    let (y, mo, d, h, mi) = crate::util::local(now_unix());
    eprintln!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} {msg}");
}

pub fn run_daemon(cfg: Config) -> Result<(), String> {
    let interval = Duration::from_secs(cfg.interval_sec);
    let mut prev: Option<(Vec<Proc>, Instant)> = None;
    let (mut last_notify, mut last_auto, mut last_prune) = (0i64, 0i64, 0i64);
    let mut warn_streak = 0;
    logf(&format!(
        "daemon started: interval {}s, history in {}",
        cfg.interval_sec,
        history::dir().display()
    ));
    loop {
        let procs = probe::procs()?;
        let mem = probe::memory()?;
        let now = Instant::now();
        let slim_copy = slim(&procs);
        if let Some((prev_procs, prev_t)) = &prev {
            let cpu = cpu_between(prev_procs, &procs, now - *prev_t);
            let mut snap = Snapshot::build(procs, cpu, mem);
            if let Err(e) = history::append(&to_sample(&snap)) {
                logf(&format!("write history: {e}"));
            }
            warn_streak = if snap.mem.pressure >= 2 {
                warn_streak + 1
            } else {
                0
            };
            if warn_streak >= 2 {
                react(&cfg, &mut snap, &mut last_notify, &mut last_auto);
            }
        }
        if now_unix() - last_prune > 6 * 3600 {
            history::prune(cfg.retention_days);
            last_prune = now_unix();
        }
        prev = Some((slim_copy, now));
        std::thread::sleep(interval);
    }
}

/// Only what CPU deltas need.
pub fn slim(ps: &[Proc]) -> Vec<Proc> {
    ps.iter()
        .map(|p| Proc {
            pid: p.pid,
            start: p.start,
            cpu_ns: p.cpu_ns,
            readable: p.readable,
            ..Default::default()
        })
        .collect()
}

pub fn to_sample(s: &Snapshot) -> Sample {
    let mb = |b: u64| (b >> 20) as u32;
    let mut smp = Sample {
        t: s.time,
        s: Sys {
            tot: mb(s.mem.total),
            used: mb(s.mem.used),
            comp: mb(s.mem.compressed),
            wired: mb(s.mem.wired),
            swap: mb(s.mem.swap_used),
            pr: s.mem.pressure,
        },
        ..Default::default()
    };
    for g in s.groups.iter().take_while(|g| g.mb >= 50).take(60) {
        smp.g.push(Group {
            n: g.name.clone(),
            m: g.mb as u32,
            c: (g.cpu_percent.max(0.0) * 10.0) as u32,
            k: g.procs.min(u16::MAX as usize) as u16,
        });
    }
    // Only what idle detection needs: big processes, dev daemons, and detached
    // non-app processes (orphan candidates). Apps are covered by groups.
    let me = uid();
    for p in &s.procs {
        let g = s.group_of.get(&p.pid).map(String::as_str).unwrap_or("");
        let keep = p.footprint >= 512 << 20
            || (p.ppid == 1
                && matches!(
                    g,
                    "Gradle daemons"
                        | "Kotlin daemon"
                        | "Maven daemon"
                        | "nx daemon"
                        | "turbo daemon"
                ))
            // Dev servers, test runners and model runners: idle rules need their history.
            || (p.uid == me && p.footprint >= 20 << 20 && dev_kind(p).is_some())
            || (p.uid == me
                && p.ppid == 1
                && p.footprint >= 20 << 20
                && !p.path.contains(".app/Contents/")
                && p.app.is_empty()
                && !is_system_path(&p.path));
        if let (true, Some(c)) = (keep, s.cpu.get(&p.pid)) {
            smp.p.push(ProcRec {
                i: p.pid,
                st: p.start,
                m: mb(p.footprint),
                c: (c * 10.0) as u32,
            });
        }
    }
    smp
}

fn react(cfg: &Config, snap: &mut Snapshot, last_notify: &mut i64, last_auto: &mut i64) {
    let window = cfg.idle_minutes as i64 * 60;
    snap.attach_history(&history::load(now_unix() - window - 120), window);
    // Pick up rules.d edits without restarting the sampler.
    let mut cfg = cfg.clone();
    (cfg.custom_rules, cfg.rule_errors) = crate::analyze::custom::load(&config::rules_dir());
    let cfg = &cfg;
    let fs = rules::suggest(snap, cfg);
    let now = now_unix();

    if cfg.auto_free_safe && now - *last_auto > 600 {
        *last_auto = now;
        let mut freed = 0;
        let mut names = Vec::new();
        for f in fs.iter().filter(|f| f.safety == SAFE && f.actionable()) {
            let r = act::execute(f, false);
            if r.ok {
                freed += r.estimated_freed_mb;
                names.push(f.title.clone());
                logf(&format!("auto-freed {} ({})", f.id, fmt_mb(f.frees_mb)));
            }
        }
        if freed > 0 && cfg.notify {
            notify(&format!("Freed {}", fmt_mb(freed)), &names.join(", "));
            *last_notify = now;
            return;
        }
    }

    if !cfg.notify || now - *last_notify < 1800 {
        return;
    }
    let actionable: Vec<_> = fs.iter().filter(|f| f.actionable()).collect();
    let total: u64 = actionable.iter().map(|f| f.frees_mb).sum();
    if total < 512 {
        return;
    }
    let top: Vec<String> = actionable
        .iter()
        .take(3)
        .map(|f| format!("{} ({})", f.title, fmt_mb(f.frees_mb)))
        .collect();
    *last_notify = now;
    notify(
        &format!(
            "Memory pressure {} · {} reclaimable",
            probe::pressure_name(snap.mem.pressure),
            fmt_mb(total)
        ),
        &format!("{}. Run: ramster free", top.join(", ")),
    );
}

// ---- notifications ------------------------------------------------------------

#[cfg(target_os = "macos")]
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn notify(title: &str, body: &str) {
    let script = format!(
        "display notification {} with title {}",
        applescript_str(body),
        applescript_str(&format!("ramster: {title}"))
    );
    let _ = run(Duration::from_secs(10), "osascript", &["-e", &script]);
}

/// Desktop notification through the freedesktop `notify-send`, when installed.
#[cfg(not(target_os = "macos"))]
fn notify(title: &str, body: &str) {
    if let Some(bin) = crate::util::which("notify-send", &[]) {
        let title = format!("ramster: {title}");
        let _ = run(
            Duration::from_secs(10),
            &bin,
            &["--app-name=ramster", &title, body],
        );
    }
}

// ---- running at login: launchd (macOS) or a systemd user service (Linux) ------

/// Is the background sampler installed?
pub fn agent_installed() -> bool {
    service::installed()
}

/// install | uninstall | status. `exe` is the ramster CLI binary to run as the
/// daemon (defaults to the current executable).
pub fn cmd_agent(sub: &str, exe: Option<std::path::PathBuf>) -> Result<(), String> {
    match sub {
        "install" => {
            let exe = match exe {
                Some(p) => p,
                None => std::env::current_exe().map_err(|e| e.to_string())?,
            };
            let exe = exe.canonicalize().map_err(|e| e.to_string())?;
            let exe = exe.to_string_lossy().into_owned();
            if exe.contains("/target/debug/") || exe.starts_with("/tmp/") || exe.contains("/T/") {
                return Err(format!("install a real binary first (make install / cargo install); {exe} looks temporary"));
            }
            config::write_default().map_err(|e| e.to_string())?;
            let log = config::home()
                .join("daemon.log")
                .to_string_lossy()
                .into_owned();
            service::install(&exe, &log)?;
            println!(
                "Installed. The sampler runs at login and checks memory every {}s.",
                Config::load().interval_sec
            );
            println!("  config:  {}", config::path().display());
            println!("  history: {}", history::dir().display());
            println!("  log:     {log}");
            println!("Run `ramster report` after a few hours to see your usage pattern.");
        }
        "uninstall" => {
            service::uninstall()?;
            println!(
                "Uninstalled. History is kept in {}",
                history::dir().display()
            );
        }
        "status" => {
            let Some(pid) = service::running_pid() else {
                println!("Not running. Install with: ramster agent install");
                return Ok(());
            };
            println!("Running (pid {pid}).");
            if pid > 0 {
                if let Some(p) = probe::procs()?.into_iter().find(|p| p.pid == pid) {
                    println!(
                        "  daemon memory: {:.1} MB",
                        p.footprint as f64 / (1u64 << 20) as f64
                    );
                }
            }
            println!(
                "  samples in the last 24h: {}",
                history::load(now_unix() - 86_400).len()
            );
            println!("  config: {}", config::path().display());
        }
        _ => return Err("usage: ramster agent install|uninstall|status".into()),
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod service {
    use std::time::{Duration, Instant};

    use super::LABEL;
    use crate::util::{run, uid};

    fn domain() -> String {
        format!("gui/{}", uid())
    }

    fn target() -> String {
        format!("{}/{LABEL}", domain())
    }

    /// `bootout` returns before the old job is torn down; wait until launchd
    /// forgets the service so an immediate `bootstrap` doesn't fail with EIO.
    fn bootout_and_wait(service: &str) {
        if !run(Duration::from_secs(5), "launchctl", &["print", service]).0 {
            return; // not loaded
        }
        let _ = run(Duration::from_secs(10), "launchctl", &["bootout", service]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && run(Duration::from_secs(5), "launchctl", &["print", service]).0
        {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn bootstrap_with_retry(domain: &str, plist: &str) -> Result<(), String> {
        let mut last = String::new();
        for attempt in 0..5 {
            let (ok, out) = run(
                Duration::from_secs(10),
                "launchctl",
                &["bootstrap", domain, plist],
            );
            if ok {
                return Ok(());
            }
            last = out;
            std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
        }
        Err(format!("launchctl bootstrap: {last}"))
    }

    fn plist_path() -> String {
        format!("{}/Library/LaunchAgents/{LABEL}.plist", crate::util::home())
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn installed() -> bool {
        std::path::Path::new(&plist_path()).exists()
    }

    pub fn install(exe: &str, log: &str) -> Result<(), String> {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>10</integer>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
</dict>
</plist>
"#,
            exe = xml_escape(exe),
            log = xml_escape(log)
        );
        let path = plist_path();
        if let Some(dir) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        bootout_and_wait(&target());
        std::fs::write(&path, plist).map_err(|e| e.to_string())?;
        bootstrap_with_retry(&domain(), &path)
    }

    pub fn uninstall() -> Result<(), String> {
        bootout_and_wait(&target());
        match std::fs::remove_file(plist_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Some(pid) when launchd has the job loaded (pid 0 if not running right now).
    pub fn running_pid() -> Option<i32> {
        let (ok, out) = run(Duration::from_secs(5), "launchctl", &["print", &target()]);
        ok.then(|| {
            out.lines()
                .find_map(|l| l.trim().strip_prefix("pid = "))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        })
    }
}

/// A systemd user unit: runs at login, restarts if it dies, low priority.
#[cfg(any(target_os = "linux", test))]
pub fn systemd_unit(exe: &str, log: &str) -> String {
    // systemd unit syntax: quote paths, double `%` (specifier) and escape `\` and `"`.
    let q = |s: &str| {
        format!(
            "\"{}\"",
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
        )
    };
    let raw = |s: &str| s.replace('%', "%%");
    format!(
        "[Unit]
Description=ramster memory sampler
Documentation=https://github.com/wasborntobeawesome/ramster

[Service]
ExecStart={} daemon
Restart=always
RestartSec=10
Nice=10
IOSchedulingClass=idle
StandardOutput=append:{}
StandardError=append:{}

[Install]
WantedBy=default.target
",
        q(exe),
        raw(log),
        raw(log)
    )
}

#[cfg(target_os = "linux")]
mod service {
    use std::time::Duration;

    use crate::util::run;

    const UNIT: &str = "ramster.service";

    fn unit_path() -> std::path::PathBuf {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => std::path::PathBuf::from(d),
            _ => std::path::Path::new(&crate::util::home()).join(".config"),
        };
        base.join("systemd/user").join(UNIT)
    }

    fn systemctl(args: &[&str]) -> Result<String, String> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        match run(Duration::from_secs(20), "systemctl", &all) {
            (true, out) => Ok(out),
            (false, out) => Err(format!("systemctl --user {}: {out}", args.join(" "))),
        }
    }

    pub fn installed() -> bool {
        unit_path().exists()
    }

    pub fn install(exe: &str, log: &str) -> Result<(), String> {
        let path = unit_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, super::systemd_unit(exe, log)).map_err(|e| e.to_string())?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT])?;
        systemctl(&["restart", UNIT]).map(|_| ())
    }

    pub fn uninstall() -> Result<(), String> {
        if installed() {
            let _ = systemctl(&["disable", "--now", UNIT]);
            std::fs::remove_file(unit_path()).map_err(|e| e.to_string())?;
            let _ = systemctl(&["daemon-reload"]);
        }
        Ok(())
    }

    pub fn running_pid() -> Option<i32> {
        let out = systemctl(&["show", "--property=MainPID", "--value", UNIT]).ok()?;
        out.trim().parse::<i32>().ok().filter(|&p| p > 0)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod service {
    const NO: &str = "the background sampler needs launchd (macOS) or systemd (Linux)";
    pub fn installed() -> bool {
        false
    }
    pub fn install(_exe: &str, _log: &str) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn uninstall() -> Result<(), String> {
        Ok(())
    }
    pub fn running_pid() -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_paths() {
        let u = systemd_unit("/home/me/My Tools/ramster", "/home/me/.ramster/daemon.log");
        assert!(
            u.contains("ExecStart=\"/home/me/My Tools/ramster\" daemon\n"),
            "{u}"
        );
        assert!(u.contains("StandardOutput=append:/home/me/.ramster/daemon.log\n"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(systemd_unit("/x/100%/ramster", "/l").contains("ExecStart=\"/x/100%%/ramster\""));
    }
}
