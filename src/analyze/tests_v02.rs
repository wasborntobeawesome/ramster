//! Tests for the v0.2 rules: local LLMs, Xcode, IDEs, dev servers, test
//! runners, Electron apps and rules.d.

use std::path::PathBuf;

use super::custom::{self, CustomRule};
use super::facts::Facts;
use super::rules::{self, Action, Finding, CONFIRM, INFO, SAFE};
use super::tests::{history_idle, p, run_rules, snap};
use super::*;
use crate::config::Config;
use crate::probe::Proc;
use crate::util::{now_unix, uid};

fn find<'a>(fs: &'a [Finding], rule: &str) -> Option<&'a Finding> {
    fs.iter().find(|f| f.rule == rule)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ramster-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn with_path(mut q: Proc, path: &str) -> Proc {
    q.path = path.into();
    q
}

#[test]
fn dev_kinds() {
    let k = |args: &str| dev_kind(&p(1, 1, "node", args, 10));
    assert_eq!(
        k("node /x/node_modules/.bin/next dev --port 3000"),
        Some(DEV_SERVERS[0])
    );
    // `next start` (production) also titles itself next-server: not a dev server.
    assert_eq!(k("next-server (v15.1.0)"), None);
    assert_eq!(k("node /x/node_modules/.bin/next start"), None);
    assert_eq!(
        k("node /x/node_modules/vite/bin/vite.js --host"),
        Some(DEV_SERVERS[1])
    );
    assert_eq!(k("node /x/node_modules/.bin/vite build"), None);
    assert_eq!(
        k("node /x/node_modules/.bin/webpack serve"),
        Some(DEV_SERVERS[2])
    );
    assert_eq!(
        k("node /x/node_modules/.bin/react-native start"),
        Some(DEV_SERVERS[6])
    );
    assert_eq!(
        k("node /x/node_modules/jest-worker/build/workers/processChild.js"),
        Some(JEST)
    );
    assert_eq!(
        k("node /x/node_modules/vitest/dist/workers/forks.js"),
        Some(VITEST)
    );
    assert_eq!(k("node /x/node_modules/.bin/vitest"), Some(VITEST));
    assert_eq!(k("node server.js"), None);
    // Only for JS runtimes.
    assert_eq!(dev_kind(&p(1, 1, "python3", "python3 vite", 10)), None);

    let runner = with_path(
        p(
            2,
            1,
            "ollama",
            "/Applications/Ollama.app/Contents/Resources/ollama runner --model /m/blobs/sha256-abc",
            10,
        ),
        "/Applications/Ollama.app/Contents/Resources/ollama",
    );
    assert_eq!(dev_kind(&runner), Some(OLLAMA));
    let llama = with_path(
        p(3, 1, "llama-server", "llama-server -m /m/q.gguf", 10),
        "/opt/homebrew/bin/llama-server",
    );
    assert_eq!(dev_kind(&llama), Some(LLM_SERVER));
    let mlx = p(
        4,
        1,
        "python3.12",
        "python3.12 -m mlx_lm.server --model mlx-community/Qwen3-8B-4bit",
        10,
    );
    assert_eq!(dev_kind(&mlx), Some(LLM_SERVER));
}

// ---- local LLMs -------------------------------------------------------------------

fn ollama_runner(pid: i32) -> Proc {
    with_path(
        p(pid, 40, "ollama", "/opt/homebrew/bin/ollama runner --model /Users/x/.ollama/models/blobs/sha256-0123456789abcdef --port 5000", 5200),
        "/opt/homebrew/bin/ollama",
    )
}

fn ollama_serve() -> Proc {
    with_path(
        p(40, 1, "ollama", "/opt/homebrew/bin/ollama serve", 60),
        "/opt/homebrew/bin/ollama",
    )
}

#[test]
fn ollama_model_tiers() {
    // Idle now, no history: ask first.
    let s = snap(vec![ollama_serve(), ollama_runner(41)], &[(41, 0.0)]);
    let fs = run_rules(&s, &Config::default());
    let f = find(&fs, "ollama-model").expect("loaded model");
    assert_eq!((f.safety, f.frees_mb), (CONFIRM, 5200));
    assert!(f.title.contains("sha256:0123456789ab"), "{}", f.title);
    assert!(
        fs.iter().all(|f| f.pids != vec![40]),
        "ollama serve is left alone"
    );

    // Idle for a while according to the sampler: safe, it reloads on demand.
    let r = ollama_runner(41);
    let mut s = snap(vec![ollama_serve(), r.clone()], &[(41, 0.0)]);
    s.attach_history(&history_idle(41, r.start, 1), 20 * 60);
    assert_eq!(
        find(&run_rules(&s, &Config::default()), "ollama-model")
            .unwrap()
            .safety,
        SAFE
    );

    // Generating right now: never.
    let s = snap(vec![ollama_serve(), ollama_runner(41)], &[(41, 350.0)]);
    assert!(find(&run_rules(&s, &Config::default()), "ollama-model").is_none());
}

#[test]
fn ollama_app_is_not_an_idle_app() {
    let mut app = with_path(
        p(30, 1, "Ollama", "", 900),
        "/Applications/Ollama.app/Contents/MacOS/Ollama",
    );
    app.resp_pid = 30;
    let mut s = snap(vec![app], &[]);
    s.attach_history(
        &(0..=30)
            .rev()
            .map(|i| crate::history::Sample {
                t: now_unix() - i * 60,
                g: vec![crate::history::Group {
                    n: "Ollama".into(),
                    m: 900,
                    c: 0,
                    k: 1,
                }],
                ..Default::default()
            })
            .collect::<Vec<_>>(),
        20 * 60,
    );
    assert!(find(&run_rules(&s, &Config::default()), "idle-app").is_none());
}

#[test]
fn ollama_manifest_names_models() {
    let d = tmp("ollama");
    let m = d.join("registry.ollama.ai/library/llama3.1");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(
        m.join("8b"),
        r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:feedbeef"}]}"#,
    )
    .unwrap();
    let u = d.join("hf.co/someone/tiny");
    std::fs::create_dir_all(&u).unwrap();
    std::fs::write(
        u.join("latest"),
        r#"{"layers":[{"digest":"sha256:c0ffee"}]}"#,
    )
    .unwrap();
    assert_eq!(
        devtools::ollama_manifest_lookup(&d, "feedbeef").as_deref(),
        Some("llama3.1:8b")
    );
    assert_eq!(
        devtools::ollama_manifest_lookup(&d, "c0ffee").as_deref(),
        Some("someone/tiny:latest")
    );
    assert_eq!(devtools::ollama_manifest_lookup(&d, "nope"), None);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn llm_server_needs_confirmation() {
    let llama = with_path(
        p(
            50,
            1,
            "llama-server",
            "/opt/homebrew/bin/llama-server -m /Users/x/models/qwen2.5-7b-q4_k_m.gguf --port 8080",
            4800,
        ),
        "/opt/homebrew/bin/llama-server",
    );
    let fs = run_rules(&snap(vec![llama], &[(50, 0.1)]), &Config::default());
    let f = find(&fs, "llm-server").expect("llama.cpp server");
    assert_eq!(f.safety, CONFIRM);
    assert_eq!(f.title, "llama.cpp server serving qwen2.5-7b-q4_k_m.gguf");
    assert!(
        fs.iter().all(|f| f.rule != "detached-process"),
        "not double counted"
    );

    let mlx = p(
        51,
        1,
        "python3.12",
        "python3.12 -m mlx_lm.server --model mlx-community/Qwen3-8B-4bit",
        6000,
    );
    let fs = run_rules(&snap(vec![mlx], &[(51, 0.0)]), &Config::default());
    assert_eq!(
        find(&fs, "llm-server").unwrap().title,
        "MLX server serving mlx-community/Qwen3-8B-4bit"
    );
}

// ---- Xcode ------------------------------------------------------------------------

fn sourcekit(mb: u64) -> Proc {
    with_path(
        p(60, 1, "SourceKitServic", "", mb),
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/sourcekitd.framework/Versions/A/XPCServices/SourceKitService.xpc/Contents/MacOS/SourceKitService",
    )
}

fn xcode_app() -> Proc {
    with_path(
        p(59, 1, "Xcode", "", 900),
        "/Applications/Xcode.app/Contents/MacOS/Xcode",
    )
}

#[test]
fn xcode_services() {
    // Xcode quit but SourceKit stayed: safe.
    let fs = run_rules(
        &snap(vec![sourcekit(900)], &[(60, 0.0)]),
        &Config::default(),
    );
    let f = find(&fs, "sourcekitservice").expect("leftover SourceKit");
    assert_eq!(f.safety, SAFE);
    assert!(f.detail.contains("left over"), "{}", f.detail);

    // Xcode open: only when it is big, and only with confirmation.
    let fs = run_rules(
        &snap(vec![xcode_app(), sourcekit(900)], &[(60, 0.0)]),
        &Config::default(),
    );
    assert!(find(&fs, "sourcekitservice").is_none());
    let fs = run_rules(
        &snap(vec![xcode_app(), sourcekit(3000)], &[(60, 0.0)]),
        &Config::default(),
    );
    assert_eq!(find(&fs, "sourcekitservice").unwrap().safety, CONFIRM);

    // Indexing right now: never.
    let fs = run_rules(
        &snap(vec![sourcekit(3000)], &[(60, 90.0)]),
        &Config::default(),
    );
    assert!(find(&fs, "sourcekitservice").is_none());
}

// ---- JS dev servers and test runners ------------------------------------------------

fn next_dev(pid: i32, ppid: i32) -> Proc {
    p(
        pid,
        ppid,
        "node",
        "node /x/node_modules/.bin/next dev",
        1800,
    )
}

#[test]
fn dev_servers_need_evidence() {
    let shell = p(9, 1, "zsh", "-zsh", 5);
    // Attached to a terminal and idle this instant: you may be using it.
    let fs = run_rules(
        &snap(vec![shell.clone(), next_dev(10, 9)], &[(10, 0.0)]),
        &Config::default(),
    );
    assert!(find(&fs, "dev-server").is_none());

    // Idle for 30 minutes according to history: ask.
    let d = next_dev(10, 9);
    let mut s = snap(vec![shell, d.clone()], &[(10, 0.0)]);
    s.attach_history(&history_idle(10, d.start, 1), 20 * 60);
    let fs = run_rules(&s, &Config::default());
    let f = find(&fs, "dev-server").expect("idle dev server");
    assert_eq!(f.safety, CONFIRM);
    assert!(f.title.starts_with("Next.js dev server"), "{}", f.title);

    // Its terminal is gone: ask, and say why.
    let fs = run_rules(
        &snap(vec![next_dev(10, 1)], &[(10, 0.0)]),
        &Config::default(),
    );
    let f = find(&fs, "dev-server").expect("detached dev server");
    assert!(f.detail.contains("terminal is gone"), "{}", f.detail);
    // and it is not also reported as a generic detached process
    assert!(fs.iter().all(|f| f.rule != "detached-process"));
}

#[test]
fn test_runner_leftovers() {
    let worker = |pid, ppid| {
        p(
            pid,
            ppid,
            "node",
            "node /x/node_modules/vitest/dist/workers/forks.js",
            400,
        )
    };
    // One quiet reading is not proof: ask.
    let fs = run_rules(&snap(vec![worker(20, 1)], &[(20, 0.0)]), &Config::default());
    let f = find(&fs, "test-workers").expect("orphaned workers");
    assert_eq!(f.safety, CONFIRM);
    // Quiet for 30 minutes per history, facts gathered, no clients: safe.
    let w = worker(20, 1);
    let mut s = snap(vec![w.clone()], &[(20, 0.0)]);
    s.attach_history(&history_idle(20, w.start, 1), 20 * 60);
    let f = find(&run_rules(&s, &Config::default()), "test-workers")
        .cloned()
        .unwrap();
    assert_eq!(f.safety, SAFE);
    // Facts not gathered yet: never SAFE.
    let f = rules::suggest_with(&s, &Config::default(), &Facts::default())
        .into_iter()
        .find(|f| f.rule == "test-workers")
        .unwrap();
    assert_eq!(f.safety, CONFIRM);

    // Watch mode with a live terminal: only once history shows it idle.
    let shell = p(9, 1, "zsh", "-zsh", 5);
    let runner = p(21, 9, "node", "node /x/node_modules/.bin/vitest", 300);
    let s = snap(
        vec![shell.clone(), runner.clone(), worker(22, 21)],
        &[(21, 0.0), (22, 0.0)],
    );
    assert!(run_rules(&s, &Config::default())
        .iter()
        .all(|f| !f.rule.starts_with("test-")));
    let mut s = snap(
        vec![shell, runner.clone(), worker(22, 21)],
        &[(21, 0.0), (22, 0.0)],
    );
    s.attach_history(&history_idle(21, runner.start, 1), 20 * 60);
    let fs = run_rules(&s, &Config::default());
    let f = find(&fs, "test-watch").expect("idle watch mode");
    assert_eq!((f.safety, f.frees_mb, f.pids.len()), (CONFIRM, 700, 2));
}

// ---- IDEs, editors, Electron -----------------------------------------------------------

#[test]
fn extension_hosts_name_the_heavy_children() {
    let app = "/Applications/Cursor.app/Contents/MacOS/Cursor";
    let host = "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin).app/Contents/MacOS/Cursor Helper (Plugin)";
    let main = with_path(p(70, 1, "Cursor", "", 400), app);
    let h = with_path(
        p(
            71,
            70,
            "Cursor Helper (",
            "Cursor Helper (Plugin) --type=utility",
            600,
        ),
        host,
    );
    let ts = with_path(
        p(
            72,
            71,
            "node",
            "node /x/typescript/lib/tsserver.js --serverMode",
            1200,
        ),
        "/opt/homebrew/bin/node",
    );
    let s = snap(vec![main, h, ts], &[]);
    let fs = run_rules(&s, &Config::default());
    let f = find(&fs, "extension-host").expect("heavy extension hosts");
    assert_eq!((f.safety, f.frees_mb), (INFO, 1800));
    assert!(f.title.starts_with("Cursor extensions"), "{}", f.title);
    assert!(f.detail.contains("tsserver.js 1.2 GB"), "{}", f.detail);
}

#[test]
fn several_jetbrains_ides() {
    let ide = |pid, name: &str, mb| {
        with_path(
            p(pid, 1, name, "-Xmx4096m -Didea.paths.selector=X", mb),
            &format!("/Applications/{name}.app/Contents/MacOS/{name}"),
        )
    };
    let s = snap(
        vec![
            ide(80, "Android Studio", 3500),
            ide(81, "IntelliJ IDEA", 2500),
        ],
        &[],
    );
    let fs = run_rules(&s, &Config::default());
    let js: Vec<&Finding> = fs.iter().filter(|f| f.rule == "jetbrains").collect();
    assert_eq!(js.len(), 2);
    assert!(js[0].tip.contains("2 JetBrains IDEs"), "{}", js[0].tip);
    assert!(js[0].detail.contains("4096m"), "{}", js[0].detail);
}

#[test]
fn electron_apps_are_summed() {
    let d = tmp("electron");
    let mut procs = Vec::new();
    for (i, name) in ["Slack", "Discord", "Notes"].iter().enumerate() {
        let bundle = d.join(format!("{name}.app"));
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        if *name != "Notes" {
            std::fs::create_dir_all(
                bundle.join("Contents/Frameworks/Electron Framework.framework"),
            )
            .unwrap();
        }
        let path = bundle.join(format!("Contents/MacOS/{name}"));
        procs.push(with_path(
            p(90 + i as i32, 1, name, "", 1000),
            &path.to_string_lossy(),
        ));
    }
    let fs = run_rules(&snap(procs, &[]), &Config::default());
    std::fs::remove_dir_all(&d).unwrap();
    let f = find(&fs, "electron").expect("electron summary");
    assert_eq!(f.frees_mb, 2000);
    assert!(
        f.detail.contains("Slack") && !f.detail.contains("Notes"),
        "{}",
        f.detail
    );
}

// ---- rules.d ----------------------------------------------------------------------------

#[test]
fn rule_packs_load_and_report_errors() {
    let d = tmp("rules");
    std::fs::write(
        d.join("a.json"),
        r#"{"rules": [
            {"id": "es", "title": "Elasticsearch", "match": {"args": "org.elasticsearch.bootstrap"}, "min_mb": 500},
            {"id": "bad", "match": {}},
            {"id": "typo", "match": {"name": "x"}, "teir": "safe"}
        ]}"#,
    )
    .unwrap();
    std::fs::write(
        d.join("b.json"),
        r#"[{"id": "slack", "match": {"app": "Slack"}, "action": "quit", "tier": "confirm"}]"#,
    )
    .unwrap();
    std::fs::write(d.join("broken.json"), "{ nope").unwrap();
    std::fs::write(d.join("README.md"), "ignored").unwrap();
    let (rules, errors) = custom::load(&d);
    std::fs::remove_dir_all(&d).unwrap();
    let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["es", "slack"]);
    assert_eq!(rules[0].source, "a.json");
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(
        errors.iter().any(|e| e.contains("a.json: rule 2")),
        "{errors:?}"
    );
    assert!(errors.iter().any(|e| e.contains("teir")), "{errors:?}");
    assert!(
        errors.iter().any(|e| e.starts_with("broken.json")),
        "{errors:?}"
    );
}

fn rule(json: &str) -> CustomRule {
    let r: CustomRule = serde_json::from_str(json).unwrap();
    r.validate().unwrap();
    r
}

#[test]
fn custom_rules_find_and_respect_safety() {
    let cfg = Config {
        custom_rules: vec![rule(
            r#"{"id": "es", "title": "Elasticsearch dev node", "match": {"name": "java", "args": ["org.elasticsearch.bootstrap", "opensearch"]}, "tier": "safe", "min_mb": 500, "tip": "lower -Xmx"}"#,
        )],
        ..Default::default()
    };
    let es = p(
        100,
        1,
        "java",
        "java -Xmx2g org.elasticsearch.bootstrap.Elasticsearch",
        2100,
    );
    let child = p(101, 100, "java", "java org.elasticsearch.controller", 50); // counted with its parent
    let fs = run_rules(
        &snap(vec![es.clone(), child.clone()], &[(100, 0.0), (101, 0.0)]),
        &cfg,
    );
    let f = find(&fs, "es").expect("custom finding");
    assert_eq!(
        (f.id.as_str(), f.safety, f.frees_mb),
        ("es:100", SAFE, 2150)
    );
    assert!(matches!(f.act, Action::Term) && f.tip == "lower -Xmx");
    assert_eq!(fs.iter().filter(|f| f.rule == "es").count(), 1);

    // Busy: never, whatever the rule says.
    let fs = run_rules(&snap(vec![es.clone(), child.clone()], &[(100, 80.0)]), &cfg);
    assert!(find(&fs, "es").is_none());
    // Below min_mb.
    let small = p(
        100,
        1,
        "java",
        "java org.elasticsearch.bootstrap.Elasticsearch",
        100,
    );
    assert!(find(&run_rules(&snap(vec![small], &[(100, 0.0)]), &cfg), "es").is_none());
    // never_touch still wins.
    let cfg2 = Config {
        never_touch: vec!["elasticsearch".into()],
        ..cfg.clone()
    };
    assert!(find(
        &run_rules(&snap(vec![es, child], &[(100, 0.0)]), &cfg2),
        "es"
    )
    .is_none());
}

#[test]
fn custom_app_rule_quits_the_app() {
    let cfg = Config {
        custom_rules: vec![rule(
            r#"{"id": "slack", "match": {"app": "slack"}, "action": "quit", "min_mb": 500}"#,
        )],
        ..Default::default()
    };
    let slack = with_path(
        p(110, 1, "Slack", "", 900),
        "/Applications/Slack.app/Contents/MacOS/Slack",
    );
    let fs = run_rules(&snap(vec![slack], &[(110, 0.0)]), &cfg);
    let f = find(&fs, "slack").expect("app rule");
    assert_eq!(f.safety, CONFIRM);
    assert!(matches!(&f.act, Action::Quit(a) if a == "Slack"));
}

#[test]
fn custom_rule_validation() {
    let bad = |j: &str| {
        serde_json::from_str::<CustomRule>(j)
            .map_err(|e| e.to_string())
            .and_then(|r| r.validate())
    };
    assert!(bad(r#"{"id": "x", "match": {"name": "y"}, "action": "quit"}"#).is_err());
    assert!(bad(r#"{"id": "x", "match": {"name": "y"}, "action": "reboot"}"#).is_err());
    assert!(bad(r#"{"id": "x", "match": {"name": "y"}, "action": {"cmd": []}}"#).is_err());
    assert!(bad(r#"{"id": "x", "match": {"app": "y"}, "when": "detached"}"#).is_err());
    assert!(bad(r#"{"id": "two words", "match": {"name": "y"}}"#).is_err());
    assert!(bad(r#"{"id": "x", "match": {"name": "y"}, "action": {"cmd": ["brew", "services", "stop", "postgresql"]}, "when": "always"}"#).is_ok());
}

#[test]
fn contrib_rule_packs_are_valid() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contrib/rules.d");
    let (rules, errors) = custom::load(&dir);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!rules.is_empty());
}

// ---- regression tests for the v0.2 review ---------------------------------------------

#[test]
fn backgrounded_watch_run_is_never_safe() {
    // `nohup vitest --watch &`, terminal closed, idle between saves.
    let runner = p(
        30,
        1,
        "node",
        "node /x/node_modules/.bin/vitest --watch",
        500,
    );
    let mut s = snap(vec![runner.clone()], &[(30, 0.0)]);
    s.attach_history(&history_idle(30, runner.start, 1), 20 * 60);
    let fs = run_rules(&s, &Config::default());
    let f = find(&fs, "test-watch").expect("detached runner offered");
    assert_eq!(f.safety, CONFIRM);
    assert!(fs.iter().all(|f| f.safety != SAFE), "{fs:?}");
    // Started by a LaunchAgent: on purpose, not reported.
    let mut facts = super::tests::known_facts();
    facts.launchd_pids.insert(30);
    let fs = rules::suggest_with(&s, &Config::default(), &facts);
    assert!(fs.iter().all(|f| !f.pids.contains(&30)), "{fs:?}");
}

#[test]
fn small_detached_dev_server_is_still_an_orphan() {
    // Below the dev-server threshold, but its terminal is gone: the generic rule reports it.
    let vite = p(40, 1, "node", "node /x/node_modules/.bin/vite", 200);
    let fs = run_rules(&snap(vec![vite], &[(40, 0.0)]), &Config::default());
    let f = find(&fs, "detached-process").expect("detached vite");
    assert_eq!(f.pids, vec![40]);
    // Above it, the specific rule reports it, once.
    let vite = p(40, 1, "node", "node /x/node_modules/.bin/vite", 600);
    let fs = run_rules(&snap(vec![vite], &[(40, 0.0)]), &Config::default());
    assert!(find(&fs, "dev-server").is_some());
    assert_eq!(
        fs.iter().filter(|f| f.pids.contains(&40)).count(),
        1,
        "{fs:?}"
    );
}

#[test]
fn custom_and_builtin_rules_never_double_count() {
    let cfg = Config {
        custom_rules: vec![rule(
            r#"{"id": "uvicorn", "match": {"args": "uvicorn"}, "min_mb": 100}"#,
        )],
        ..Default::default()
    };
    let api = p(50, 1, "python3", "python3 -m uvicorn app:main", 600);
    let fs = run_rules(&snap(vec![api], &[(50, 0.0)]), &cfg);
    let hits: Vec<&str> = fs
        .iter()
        .filter(|f| f.pids.contains(&50))
        .map(|f| f.rule.as_str())
        .collect();
    assert_eq!(hits, ["uvicorn"], "the user's rule wins, once");
}

#[test]
fn custom_rule_with_unknown_activity_is_not_safe() {
    let r = rule(r#"{"id": "pg", "match": {"name": "postgres"}, "tier": "safe", "when": "idle"}"#);
    let cfg = Config {
        custom_rules: vec![r],
        ..Default::default()
    };
    let pg = p(60, 1, "postgres", "postgres -D /x", 400);
    // Idle now: safe as written.
    let fs = run_rules(&snap(vec![pg.clone()], &[(60, 0.0)]), &cfg);
    assert_eq!(find(&fs, "pg").unwrap().safety, SAFE);
    // `always` with no CPU reading at all: offered, but only as confirm.
    let cfg = Config {
        custom_rules: vec![rule(
            r#"{"id": "pg", "match": {"name": "postgres"}, "when": "always"}"#,
        )],
        ..Default::default()
    };
    let fs = run_rules(&snap(vec![pg.clone()], &[]), &cfg);
    assert_eq!(find(&fs, "pg").unwrap().safety, CONFIRM);
    // and `safe` + `always` is refused outright.
    let bad: CustomRule = serde_json::from_str(
        r#"{"id": "pg", "match": {"name": "postgres"}, "tier": "safe", "when": "always"}"#,
    )
    .unwrap();
    assert!(bad.validate().is_err());
}

#[test]
fn custom_ids_cannot_shadow_ramster() {
    for id in [
        "safe",
        "all",
        "confirm",
        "info",
        "docker",
        "dev-server",
        "a:b",
    ] {
        let r: CustomRule =
            serde_json::from_str(&format!(r#"{{"id": "{id}", "match": {{"name": "x"}}}}"#))
                .unwrap();
        assert!(r.validate().is_err(), "{id} accepted");
    }
    // One finding per app, each with its own id.
    let cfg = Config {
        custom_rules: vec![rule(
            r#"{"id": "chat", "match": {"app": ["Slack", "Discord"]}, "action": "quit", "min_mb": 100}"#,
        )],
        ..Default::default()
    };
    let app = |pid, n: &str| {
        with_path(
            p(pid, 1, n, "", 500),
            &format!("/Applications/{n}.app/Contents/MacOS/{n}"),
        )
    };
    let fs = run_rules(
        &snap(
            vec![app(70, "Slack"), app(71, "Discord")],
            &[(70, 0.0), (71, 0.0)],
        ),
        &cfg,
    );
    let mut ids: Vec<&str> = fs
        .iter()
        .filter(|f| f.rule == "chat")
        .map(|f| f.id.as_str())
        .collect();
    ids.sort();
    assert_eq!(ids, ["chat:discord", "chat:slack"]);
}

#[test]
fn other_users_llm_runtimes_are_ignored() {
    let mut runner = ollama_runner(41);
    runner.uid = uid() + 1;
    let fs = run_rules(
        &snap(vec![ollama_serve(), runner], &[(41, 0.0)]),
        &Config::default(),
    );
    assert!(find(&fs, "ollama-model").is_none());
}
