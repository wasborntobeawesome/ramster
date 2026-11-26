use std::collections::HashMap;
use std::time::Duration;

use super::facts::{DockerInfo, Facts};
use super::rules::{self, mcp_key, Action, Finding, CONFIRM, SAFE};
use super::*;
use crate::config::Config;
use crate::history::{Group as HGroup, ProcRec, Sample, Sys};
use crate::probe::{Mem, Proc};
use crate::util::{now_unix, uid};

const MB: u64 = 1 << 20;

/// Facts as if launchctl/lsof found nothing special (tests never shell out).
fn known_facts() -> Facts {
    Facts {
        procs_known: true,
        ..Default::default()
    }
}

fn run_rules(s: &Snapshot, cfg: &Config) -> Vec<Finding> {
    rules::suggest_with(s, cfg, &known_facts())
}

fn snap(procs: Vec<Proc>, cpu: &[(i32, f64)]) -> Snapshot {
    Snapshot::build(
        procs,
        cpu.iter().copied().collect(),
        Mem {
            total: 48 << 30,
            pressure: 2,
            ..Default::default()
        },
    )
}

fn old() -> i64 {
    now_unix() - 3 * 3600
}

fn p(pid: i32, ppid: i32, name: &str, args: &str, mb: u64) -> Proc {
    Proc {
        pid,
        ppid,
        uid: uid(),
        name: name.into(),
        args: args.into(),
        start: old(),
        footprint: mb * MB,
        readable: true,
        resp_pid: pid,
        ..Default::default()
    }
}

fn gradle(pid: i32) -> Proc {
    p(pid, 1, "java", "java -Xmx4096m -cp gradle-daemon-main-9.8.2.jar org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.8.2", 2000)
}

#[test]
fn app_names() {
    assert_eq!(
        app_name("/Applications/Google Chrome.app/Contents/Frameworks/X.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/x"),
        Some("Google Chrome")
    );
    assert_eq!(
        app_name("/Applications/Docker.app/Contents/MacOS/com.docker.backend"),
        Some("Docker")
    );
    assert_eq!(app_name("/Users/x/Library/Application Support/com.conductor.app/agent-binaries/claude/2.1/claude"), None);
}

#[test]
fn classify_groups() {
    let mut docker = p(10, 1, "Docker", "", 100);
    docker.path = "/Applications/Docker.app/Contents/MacOS/com.docker.backend".into();
    let mut vm = p(11, 1, "com.apple.Virtua", "", 8000);
    vm.path =
        "/System/Library/Frameworks/Virtualization.framework/XPCServices/vm.xpc/Contents/MacOS/vm"
            .into();
    vm.resp_pid = 10;
    let s = snap(
        vec![
            docker,
            vm,
            p(20, 1, "claude", "claude --mcp-config x", 300),
            p(21, 20, "npm", "npm exec firebase-tools mcp --dir .", 90),
            p(
                22,
                21,
                "node",
                "node /opt/homebrew/bin/firebase mcp --dir .",
                130,
            ),
            p(
                30,
                1,
                "java",
                "java -Xmx8g org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.7.1",
                2400,
            ),
            p(31, 30, "java", "java worker GradleWorkerMain", 300),
        ],
        &[],
    );
    let want = [
        (10, "Docker"),
        (11, "Docker"),
        (20, "AI agents"),
        (21, "AI agents"),
        (22, "AI agents"),
        (30, "Gradle daemons"),
        (31, "Gradle daemons"),
    ];
    for (pid, g) in want {
        assert_eq!(s.group_of[&pid], g, "pid {pid}");
    }
    let g = s.group("Gradle daemons").unwrap();
    assert_eq!((g.mb, g.procs), (2700, 2));
    assert_eq!(s.groups[0].name, "Docker");
}

#[test]
fn gradle_idle_suggested_busy_never() {
    let s = snap(vec![gradle(100), gradle(200)], &[(100, 0.1), (200, 85.0)]);
    let fs = run_rules(&s, &Config::default());
    let f = fs
        .iter()
        .find(|f| f.id == "gradle-daemon:100")
        .expect("idle daemon suggested");
    assert_eq!(f.safety, SAFE);
    assert_eq!(f.frees_mb, 2000);
    assert!(f.detail.contains("4096m"), "{}", f.detail);
    assert!(
        fs.iter().all(|f| f.id != "gradle-daemon:200"),
        "busy daemon must never be suggested"
    );
}

#[test]
fn recent_gradle_daemon_needs_confirm() {
    let mut g = gradle(100);
    g.start = now_unix() - 180;
    let fs = run_rules(&snap(vec![g], &[(100, 0.0)]), &Config::default());
    assert_eq!(fs[0].safety, CONFIRM);
}

#[test]
fn orphan_and_duplicate_mcp() {
    let procs = vec![
        p(
            50,
            1,
            "node",
            "node /usr/local/bin/some-mcp-server --stdio",
            120,
        ), // parent agent died
        p(60, 1, "claude", "claude", 300),
        p(61, 60, "npm", "npm exec firebase-tools mcp --dir .", 90),
        p(
            62,
            61,
            "node",
            "node /opt/homebrew/bin/firebase mcp --dir .",
            130,
        ),
        p(70, 1, "claude", "claude", 300),
        p(71, 70, "npm", "npm exec firebase-tools mcp --dir .", 90),
        p(
            72,
            71,
            "node",
            "node /opt/homebrew/bin/firebase mcp --dir .",
            130,
        ),
    ];
    let fs = run_rules(&snap(procs, &[(50, 0.0)]), &Config::default());
    let orphans: Vec<&Finding> = fs.iter().filter(|f| f.rule == "orphan-mcp").collect();
    assert_eq!(orphans.len(), 1);
    assert_eq!((orphans[0].pids[0], orphans[0].safety), (50, SAFE));
    let dup = fs
        .iter()
        .find(|f| f.rule == "duplicate-mcp")
        .expect("duplicate MCP");
    assert!(
        dup.title.contains("firebase-tools mcp") && dup.title.contains("2×"),
        "{}",
        dup.title
    );
}

#[test]
fn agent_cli_is_not_an_mcp_server() {
    let claude = p(60, 1, "claude", "/x/claude --mcp-config /tmp/mcp.json", 300);
    assert!(!rules::is_mcp(&claude));
}

#[test]
fn never_touch_is_respected() {
    let cfg = Config {
        never_touch: vec!["gradle".into()],
        ..Default::default()
    };
    let fs = run_rules(&snap(vec![gradle(100)], &[(100, 0.0)]), &cfg);
    assert!(fs.iter().all(|f| f.rule != "gradle-daemon"));
}

#[test]
fn idle_from_history() {
    let start = old();
    let mut g = gradle(100);
    g.start = start;
    let now = now_unix();
    let samples: Vec<Sample> = (0..=30)
        .rev()
        .map(|i| Sample {
            t: now - i * 60,
            p: vec![ProcRec {
                i: 100,
                st: start,
                m: 2000,
                c: 2,
            }],
            ..Default::default()
        })
        .collect();
    let mut s = snap(vec![g], &[(100, 0.0)]);
    s.attach_history(&samples, 20 * 60);
    let idle = s.idle_procs(&[100], 20);
    assert!(idle.idle && idle.minutes >= 16.0, "{idle:?}");
    assert!(idle.to_string().starts_with("idle 2"), "{idle}");
}

#[test]
fn cpu_between_skips_recycled_pids() {
    let a = vec![
        Proc {
            pid: 1,
            start: 10,
            readable: true,
            ..Default::default()
        },
        Proc {
            pid: 2,
            start: 10,
            readable: true,
            ..Default::default()
        },
    ];
    let b = vec![
        Proc {
            pid: 1,
            start: 10,
            cpu_ns: 500_000_000,
            readable: true,
            ..Default::default()
        },
        Proc {
            pid: 2,
            start: 99,
            cpu_ns: 500_000_000,
            readable: true,
            ..Default::default()
        },
    ];
    let cpu = cpu_between(&a, &b, Duration::from_secs(1));
    assert!((cpu[&1] - 50.0).abs() < 0.5);
    assert!(!cpu.contains_key(&2));
}

#[test]
fn report_finds_forgotten_apps() {
    let start = now_unix() - 4 * 3600;
    let samples: Vec<Sample> = (0..240)
        .map(|i| Sample {
            t: start + i * 60,
            s: Sys {
                tot: 49152,
                used: 40000,
                comp: 20000,
                pr: if i % 2 == 0 { 2 } else { 1 },
                ..Default::default()
            },
            g: vec![
                HGroup {
                    n: "Android emulator".into(),
                    m: 7000,
                    c: 2,
                    k: 1,
                }, // idle all along
                HGroup {
                    n: "Safari".into(),
                    m: 4000,
                    c: 300,
                    k: 30,
                }, // in use
            ],
            ..Default::default()
        })
        .collect();
    let r = report::build(&samples);
    assert_eq!(r.forgotten.len(), 1);
    assert_eq!(r.forgotten[0].name, "Android emulator");
    assert!(
        (26.0..28.0).contains(&r.forgotten[0].idle_gb_hours),
        "{}",
        r.forgotten[0].idle_gb_hours
    );
    assert!((45.0..55.0).contains(&r.pressure_warning_percent));
    assert!(r.advice.iter().any(|a| a.contains("emulator")));
}

#[test]
fn mcp_key_strips_launchers() {
    let p = p(
        1,
        2,
        "npm",
        "npm exec -y @modelcontextprotocol/server-github --stdio",
        10,
    );
    assert_eq!(mcp_key(&p), "server-github");
}

#[test]
fn terminate_verifies_identity() {
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let start = crate::probe::start_time(pid);
    let finding = |want: i64| Finding {
        id: "t".into(),
        safety: SAFE,
        pids: vec![pid],
        act: Action::Term,
        starts: HashMap::from([(pid, want)]),
        ..Default::default()
    };
    // Wrong start time simulates a recycled PID: must not be signalled.
    act::execute(&finding(start - 1000), false);
    assert!(
        child.try_wait().unwrap().is_none(),
        "killed a process whose identity did not match"
    );
    // Reap in the background so the PID disappears once killed.
    let reaper = std::thread::spawn(move || child.wait());
    let r = act::execute(&finding(start), false);
    assert!(r.ok, "{r:?}");
    assert!(reaper.join().unwrap().is_ok());
}

#[test]
fn info_findings_do_nothing() {
    let r = act::execute(
        &Finding {
            id: "tip".into(),
            safety: rules::INFO,
            ..Default::default()
        },
        true,
    );
    assert!(!r.ok);
}

#[test]
fn process_managers_are_left_alone() {
    let pm2 = p(80, 1, "node", "PM2 v5.4.2: God Daemon (/Users/x/.pm2)", 400);
    let fs = run_rules(&snap(vec![pm2], &[(80, 0.0)]), &Config::default());
    assert!(fs.iter().all(|f| !f.pids.contains(&80)), "{fs:?}");
}

#[test]
fn disclaimed_helpers_fall_back_to_app_ancestor() {
    let mut app = p(10, 1, "Code", "", 300);
    app.path = "/Applications/Visual Studio Code.app/Contents/MacOS/Code".into();
    let mut helper = p(11, 10, "node", "node server.js", 200);
    helper.path = "/opt/homebrew/bin/node".into(); // resp_pid == self (disclaimed)
    let s = snap(vec![app, helper], &[]);
    assert_eq!(s.group_of[&11], "Visual Studio Code");
}

#[test]
fn terminal_children_keep_their_own_name() {
    let mut term = p(10, 1, "ghostty", "", 300);
    term.path = "/Applications/Ghostty.app/Contents/MacOS/ghostty".into();
    let shell = p(11, 10, "zsh", "-zsh", 5);
    let mut vite = p(12, 11, "node", "node vite", 900);
    vite.resp_pid = 10;
    let s = snap(vec![term, shell, vite], &[]);
    assert_eq!(s.group_of[&12], "node");
    assert_eq!(s.group_of[&10], "Ghostty");
}

// ---- regression tests for review findings ------------------------------------------

fn history_idle(pid: i32, start: i64, cpu_permille: u32) -> Vec<Sample> {
    let now = now_unix();
    (0..=30)
        .rev()
        .map(|i| Sample {
            t: now - i * 60,
            p: vec![ProcRec {
                i: pid,
                st: start,
                m: 2000,
                c: cpu_permille,
            }],
            ..Default::default()
        })
        .collect()
}

#[test]
fn busy_live_cpu_beats_idle_history() {
    let g = gradle(100);
    let mut s = snap(vec![g.clone()], &[(100, 300.0)]); // a build just started
    s.attach_history(&history_idle(100, g.start, 1), 20 * 60);
    assert!(!s.idle_procs(&[100], 20).idle);
    assert!(run_rules(&s, &Config::default())
        .iter()
        .all(|f| f.rule != "gradle-daemon"));
}

#[test]
fn busy_child_in_history_keeps_tree_busy() {
    let root = p(10, 1, "npm", "npm run dev", 60);
    let child = p(11, 10, "node", "node server.js", 400);
    let mut s = snap(vec![root.clone(), child.clone()], &[]);
    let mut samples = history_idle(10, root.start, 1);
    for smp in &mut samples {
        smp.p.push(ProcRec {
            i: 11,
            st: child.start,
            m: 400,
            c: 300,
        });
    }
    s.attach_history(&samples, 20 * 60);
    assert!(!s.idle_procs(&s.tree(10), 20).idle);
}

#[test]
fn permission_denied_is_not_deleted() {
    assert!(rules::dir_gone("/definitely/not/here/ramster"));
    assert!(!rules::dir_gone(&std::env::temp_dir().to_string_lossy()));
    assert!(!rules::dir_gone(""));
    // A directory we can't stat into (like TCC-protected ~/Documents for launchd).
    let locked = std::env::temp_dir().join(format!("ramster-locked-{}", std::process::id()));
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let gone = rules::dir_gone(&locked.join("inner").to_string_lossy());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&locked).unwrap();
    assert!(!gone, "EACCES must not count as deleted");
}

fn orphan_mcp() -> Proc {
    p(
        50,
        1,
        "node",
        "node /usr/local/bin/some-mcp-server --stdio",
        120,
    )
}

fn orphan_finding(facts: &Facts, cpu: f64) -> Option<Finding> {
    let s = snap(vec![orphan_mcp()], &[(50, cpu)]);
    rules::suggest_with(&s, &Config::default(), facts)
        .into_iter()
        .find(|f| f.rule == "orphan-mcp")
}

#[test]
fn orphan_mcp_safety_needs_proof() {
    assert_eq!(orphan_finding(&known_facts(), 0.0).unwrap().safety, SAFE);
    // A LaunchAgent-managed server is deliberate, not an orphan.
    let mut f = known_facts();
    f.launchd_pids.insert(50);
    assert!(orphan_finding(&f, 0.0).is_none());
    // Listening on a port: may be a shared server with clients.
    let mut f = known_facts();
    f.listening_pids.insert(50);
    let found = orphan_finding(&f, 0.0).unwrap();
    assert_eq!(found.safety, CONFIRM);
    assert!(found.detail.contains("listens"));
    // Facts not gathered yet (menu bar first open): never SAFE.
    assert_eq!(
        orphan_finding(&Facts::default(), 0.0).unwrap().safety,
        CONFIRM
    );
    // Busy right now: not suggested at all.
    assert!(orphan_finding(&known_facts(), 25.0).is_none());
}

#[test]
fn docker_rule_reads_facts_without_running_commands() {
    let mut vm = p(11, 1, "com.apple.Virtua", "", 8000);
    vm.path = "/Applications/Docker.app/Contents/MacOS/vm".into();
    let s = snap(vec![vm], &[]);
    let with = |docker: Option<DockerInfo>| {
        let facts = Facts {
            docker,
            ..known_facts()
        };
        rules::suggest_with(&s, &Config::default(), &facts)
            .into_iter()
            .find(|f| f.rule == "docker")
            .unwrap()
    };
    assert!(with(None).detail.contains("checking"));
    let empty = with(Some(DockerInfo {
        running: Some(0),
        ..Default::default()
    }));
    assert!(empty.title.contains("no containers"));
    assert!(matches!(&empty.act, Action::Cmd(c) if c[0] == "osascript"));
    let busy = with(Some(DockerInfo {
        running: Some(2),
        used_mb: 300,
        stop: vec!["docker".into(), "desktop".into(), "stop".into()],
    }));
    assert!(busy.title.contains("2 container") && busy.detail.contains("300 MB"));
}

#[test]
fn agent_breakdown_puts_count_first() {
    let mut procs = Vec::new();
    for i in 0..3 {
        procs.push(p(100 + i, 1, "claude", "claude", 400));
    }
    let f = run_rules(&snap(procs, &[]), &Config::default())
        .into_iter()
        .find(|f| f.rule == "agents")
        .unwrap();
    assert!(f.detail.starts_with("3×claude"), "{}", f.detail);
}

#[test]
fn commented_gradle_property_does_not_count() {
    let key = "org.gradle.daemon.idletimeout";
    assert!(rules::properties_set(
        "org.gradle.daemon.idletimeout=1800000",
        key
    ));
    assert!(rules::properties_set(
        "  org.gradle.daemon.idletimeout : 1800000",
        key
    ));
    assert!(!rules::properties_set(
        "#org.gradle.daemon.idletimeout=1800000",
        key
    ));
    assert!(!rules::properties_set(
        "! org.gradle.daemon.idletimeout=1",
        key
    ));
    assert!(!rules::properties_set(
        "org.gradle.daemon.idletimeoutx=1",
        key
    ));
}
