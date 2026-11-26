//! Background sampler and its launchd integration.
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

fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn notify(title: &str, body: &str) {
    let script = format!(
        "display notification {} with title {}",
        applescript_str(body),
        applescript_str(&format!("ramster: {title}"))
    );
    let _ = run(Duration::from_secs(10), "osascript", &["-e", &script]);
}

// ---- launchd ----------------------------------------------------------------

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

/// Is the launchd sampler installed?
pub fn agent_installed() -> bool {
    std::path::Path::new(&plist_path()).exists()
}

/// install | uninstall | status. `exe` is the ramster CLI binary to run as the
/// daemon (defaults to the current executable).
pub fn cmd_agent(sub: &str, exe: Option<std::path::PathBuf>) -> Result<(), String> {
    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
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
                exe = xml_escape(&exe),
                log = xml_escape(&log)
            );
            let path = plist_path();
            if let Some(dir) = std::path::Path::new(&path).parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            bootout_and_wait(&service);
            std::fs::write(&path, plist).map_err(|e| e.to_string())?;
            bootstrap_with_retry(&domain, &path)?;
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
            bootout_and_wait(&service);
            match std::fs::remove_file(plist_path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            println!(
                "Uninstalled. History is kept in {}",
                history::dir().display()
            );
        }
        "status" => {
            let (ok, out) = run(Duration::from_secs(5), "launchctl", &["print", &service]);
            if !ok {
                println!("Not running. Install with: ramster agent install");
                return Ok(());
            }
            let pid: i32 = out
                .lines()
                .find_map(|l| l.trim().strip_prefix("pid = "))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
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
