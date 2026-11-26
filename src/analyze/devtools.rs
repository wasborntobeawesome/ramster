//! Rules for the tools that hold the most memory on a developer Mac besides
//! build daemons and VMs: local LLM runtimes, Xcode services, JetBrains IDEs,
//! editor extension hosts, JS dev servers, test runners and Electron apps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::rules::{after, dir_gone, short_cmd, Action, Ctx, Finding, CONFIRM, INFO, SAFE};
use super::{dev_kind, exe, Group, DEV_SERVERS, JEST, LLM_SERVER, LM_STUDIO, VITEST};
use crate::probe::{self, Proc};
use crate::util::{base_name, fmt_mb, home, slug, tildify};

/// Processes of `kind` whose parent is not of the same kind (one per tool instance).
fn roots<'a>(c: &'a Ctx, kinds: &'a [&str]) -> impl Iterator<Item = (&'a Proc, &'static str)> {
    c.s.procs.iter().filter_map(move |p| {
        let k = dev_kind(p).filter(|k| kinds.contains(k))?;
        let parent = c.s.proc(p.ppid).and_then(dev_kind);
        (parent != Some(k) && p.uid == c.me).then_some((p, k))
    })
}

/// Idle according to the sampler's history, not just this instant.
fn idle_for_long(i: &super::Idleness) -> bool {
    i.known && i.idle && i.minutes > 0.0
}

// ---- local LLM runtimes ------------------------------------------------------------

pub(super) fn local_llms(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in &c.s.procs {
        let runner = is_ollama_runner(p);
        if !runner && dev_kind(p) != Some(LLM_SERVER) {
            continue;
        }
        if !runner && c.s.proc(p.ppid).and_then(dev_kind) == Some(LLM_SERVER) {
            continue; // a worker of a server we already count
        }
        let pids = c.s.tree(p.pid);
        if c.s.bytes_of(&pids) >> 20 < 500 {
            continue;
        }
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue; // generating right now
        }
        let model = model_arg(&p.args);
        if runner {
            let name = model
                .as_deref()
                .map(|m| ollama_model_name(m).unwrap_or_else(|| short_model(m)))
                .unwrap_or_else(|| "model".into());
            // Ollama reloads the model on the next request, so an idle one is
            // safe to drop once history says nobody used it for a while.
            let safety = if idle_for_long(&idle) { SAFE } else { CONFIRM };
            let mut f = c.term(
                "ollama-model",
                p,
                pids,
                safety,
                format!("Ollama model {name} is loaded"),
                format!("{idle}, loaded {} ago", c.age(p)),
                "unload it (stops the model runner; Ollama loads it again on the next request)",
            );
            if p.start > 0 && c.now - p.start > 3600 {
                f.tip = "Ollama keeps a model loaded for OLLAMA_KEEP_ALIVE after the last request (5 min by default). If yours stay loaded for hours, lower OLLAMA_KEEP_ALIVE or pass keep_alive per request.".into();
            }
            out.push(f);
        } else {
            let label = llm_server_label(p);
            let title = match &model {
                Some(m) => format!("{label} serving {}", short_model(m)),
                None => format!("{label}: {}", short_cmd(p)),
            };
            out.push(c.term(
                "llm-server",
                p,
                pids,
                CONFIRM,
                title,
                format!("{idle}, up {}", c.age(p)),
                "stop the server (SIGTERM); start it again when you need it",
            ));
        }
    }
    out.extend(lm_studio(c));
    out
}

fn is_ollama_runner(p: &Proc) -> bool {
    let x = exe(p);
    x == "ollama_llama_server"
        || (x == "ollama" && p.args.split_whitespace().nth(1) == Some("runner"))
}

fn llm_server_label(p: &Proc) -> &'static str {
    let a = &p.args;
    match exe(p) {
        "llama-server" => "llama.cpp server",
        "llamafile" => "llamafile",
        "koboldcpp" => "KoboldCpp",
        _ if a.contains("mlx_vlm") => "MLX-VLM server",
        _ if a.contains("mlx") => "MLX server",
        _ if a.contains("vllm") => "vLLM server",
        _ => "Local LLM server",
    }
}

fn model_arg(args: &str) -> Option<String> {
    ["--model ", "--model=", "-m "]
        .iter()
        .find_map(|m| after(args, m))
        .filter(|m| !m.starts_with("mlx_") && !m.starts_with("vllm"))
        .map(String::from)
}

/// "qwen2.5-7b-instruct-q4_k_m.gguf" from a path; HF repo ids stay as they are.
fn short_model(m: &str) -> String {
    if m.starts_with('/') || m.starts_with('~') {
        let b = base_name(m);
        if let Some(d) = b.strip_prefix("sha256-") {
            return format!("sha256:{}", &d[..d.len().min(12)]);
        }
        return b.to_string();
    }
    m.to_string()
}

/// Map an Ollama blob path back to "llama3.1:8b" by reading the manifests.
pub fn ollama_model_name(blob: &str) -> Option<String> {
    let digest = base_name(blob).strip_prefix("sha256-")?;
    let root = match std::env::var("OLLAMA_MODELS") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => Path::new(&home()).join(".ollama/models"),
    };
    ollama_manifest_lookup(&root.join("manifests"), digest)
}

/// manifests/<registry>/<namespace>/<model>/<tag> are small JSON files listing layer digests.
pub fn ollama_manifest_lookup(manifests: &Path, digest: &str) -> Option<String> {
    let needle = format!("sha256:{digest}");
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    for registry in dirs(manifests) {
        for ns in dirs(&registry) {
            for model in dirs(&ns) {
                for tag in dirs(&model) {
                    let Ok(text) = std::fs::read_to_string(&tag) else {
                        continue;
                    };
                    if !text.contains(&needle) {
                        continue;
                    }
                    let name = |p: &Path| base_name(&p.to_string_lossy()).to_string();
                    let mut m = name(&model);
                    if name(&ns) != "library" {
                        m = format!("{}/{m}", name(&ns));
                    }
                    return Some(format!("{m}:{}", name(&tag)));
                }
            }
        }
    }
    None
}

fn lm_studio(c: &Ctx) -> Vec<Finding> {
    let Some(g) = c.s.group(LM_STUDIO).filter(|g| g.mb >= 1500) else {
        return vec![];
    };
    let mut idle = c.s.idle_group(&g.name, c.cfg.idle_minutes);
    if !idle.known {
        idle = c.s.idle_procs(&g.pids, c.cfg.idle_minutes);
    }
    if idle.known && !idle.idle {
        return vec![];
    }
    let lms = format!("{}/.lmstudio/bin/lms", home());
    let (act, action, frees) = if Path::new(&lms).is_file() {
        (
            Action::Cmd(vec![lms, "unload".into(), "--all".into()]),
            "unload all models (lms unload --all); LM Studio stays open".to_string(),
            g.mb.saturating_sub(500),
        )
    } else {
        (
            Action::Quit(g.name.clone()),
            "quit LM Studio".to_string(),
            g.mb,
        )
    };
    vec![Finding {
        id: "lm-studio".into(),
        rule: "lm-studio".into(),
        safety: CONFIRM,
        title: format!("LM Studio holds {} (loaded models)", fmt_mb(g.mb)),
        detail: format!("{idle}, {} processes", g.procs),
        frees_mb: frees,
        action,
        tip: "LM Studio can unload models on its own: turn on the idle TTL (Developer → Server settings → Auto unload unused JIT loaded models).".into(),
        pids: g.pids.clone(),
        act,
        ..Default::default()
    }]
}

// ---- Xcode ----------------------------------------------------------------------------

/// (executable, what it is, MB worth mentioning while Xcode is open, what stopping it costs)
const XCODE_SERVICES: &[(&str, &str, u64, &str)] = &[
    (
        "SourceKitService",
        "SourceKit (code completion and indexing)",
        1536,
        "stop it; Xcode restarts it on demand (code completion pauses for a few seconds)",
    ),
    (
        "XCBBuildService",
        "Xcode build service",
        1024,
        "stop it; the next build starts a fresh one (a little slower)",
    ),
    (
        "SWBBuildService",
        "Xcode build service",
        1024,
        "stop it; the next build starts a fresh one (a little slower)",
    ),
];

pub(super) fn xcode(c: &Ctx) -> Vec<Finding> {
    let xcode_open =
        c.s.procs
            .iter()
            .any(|p| exe(p) == "Xcode" && p.path.contains(".app/Contents/MacOS/"));
    let mut out = Vec::new();
    for p in c.s.procs.iter().filter(|p| p.uid == c.me) {
        let x = exe(p);
        let Some(&(_, what, big, action)) = XCODE_SERVICES.iter().find(|s| s.0 == x) else {
            continue;
        };
        let pids = c.s.tree(p.pid);
        let mb = c.s.bytes_of(&pids) >> 20;
        if mb < if xcode_open { big } else { 200 } {
            continue;
        }
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue; // indexing or building right now
        }
        let (safety, why) = if xcode_open {
            (CONFIRM, "Xcode is open")
        } else if idle.known {
            (SAFE, "left over after Xcode quit")
        } else {
            (CONFIRM, "left over after Xcode quit")
        };
        let mut f = c.term(
            &slug(x),
            p,
            pids,
            safety,
            format!("{what} holds {}", fmt_mb(mb)),
            format!("{why}, {idle}, up {}", c.age(p)),
            action,
        );
        if x == "SourceKitService" && xcode_open {
            f.tip = "SourceKit grows with the number of open projects and indexed packages. Close workspaces you aren't using.".into();
        }
        out.push(f);
    }
    out
}

// ---- JetBrains IDEs --------------------------------------------------------------------

const JETBRAINS: &[&str] = &[
    "IntelliJ IDEA",
    "Android Studio",
    "PyCharm",
    "WebStorm",
    "GoLand",
    "CLion",
    "Rider",
    "RubyMine",
    "PhpStorm",
    "DataGrip",
    "RustRover",
    "DataSpell",
    "Aqua",
    "Writerside",
];

pub fn is_jetbrains(app: &str) -> bool {
    JETBRAINS.iter().any(|j| app.starts_with(j))
}

pub(super) fn jetbrains(c: &Ctx) -> Vec<Finding> {
    let ides: Vec<&Group> =
        c.s.groups
            .iter()
            .filter(|g| g.is_app && is_jetbrains(&g.name))
            .collect();
    let names: Vec<&str> = ides.iter().map(|g| g.name.as_str()).collect();
    ides.iter()
        .filter(|g| g.mb >= 1500)
        // Idle ones get a "quit it" finding from idle_apps instead.
        .filter(|g| {
            let i = c.s.idle_group(&g.name, c.cfg.idle_minutes);
            !(i.known && i.idle)
        })
        .map(|g| {
            let heap = g
                .pids
                .iter()
                .filter_map(|&p| c.s.proc(p))
                .find_map(|p| after(&p.args, "-Xmx"));
            let mut detail = format!("{} processes", g.procs);
            if let Some(h) = heap {
                detail += &format!(", heap limit {h}");
            }
            let tip = if ides.len() > 1 {
                format!("{} JetBrains IDEs are open ({}). Each is its own JVM with its own heap and indexes; close the ones you aren't using.", ides.len(), names.join(", "))
            } else {
                "The JVM rarely gives heap back to macOS once it has grown. Help → Change Memory Settings sets the limit; close projects you aren't using (each keeps its index in memory).".into()
            };
            Finding {
                id: format!("jetbrains:{}", slug(&g.name)),
                rule: "jetbrains".into(),
                safety: INFO,
                title: format!("{} uses {}", g.name, fmt_mb(g.mb)),
                detail,
                frees_mb: g.mb,
                tip,
                pids: g.pids.clone(),
                ..Default::default()
            }
        })
        .collect()
}

// ---- VS Code-style editors: extension hosts -------------------------------------------

/// The extension host runs in the "<App> Helper (Plugin)" process of VS Code and its forks.
fn is_extension_host(p: &Proc) -> bool {
    p.path.contains(" Helper (Plugin).app/Contents/MacOS/")
        && !p.path.contains("Google Chrome")
        && !p.path.contains("Brave Browser")
        && !p.path.contains("Microsoft Edge")
}

pub(super) fn extension_hosts(c: &Ctx) -> Vec<Finding> {
    // app → (host roots, all pids)
    let mut apps: HashMap<&str, (usize, Vec<i32>)> = HashMap::new();
    for p in c.s.procs.iter().filter(|p| is_extension_host(p)) {
        if c.s.proc(p.ppid).is_some_and(is_extension_host) {
            continue;
        }
        let Some(app) = c.s.group_of.get(&p.pid) else {
            continue;
        };
        let e = apps.entry(app.as_str()).or_default();
        e.0 += 1;
        e.1.extend(c.s.tree(p.pid));
    }
    let mut out: Vec<Finding> = apps
        .into_iter()
        .filter_map(|(app, (hosts, pids))| {
            let mb = c.s.bytes_of(&pids) >> 20;
            if mb < 1500 {
                return None;
            }
            // The heaviest things extensions started (language servers, tsserver, linters).
            let mut heavy: Vec<(String, u64)> = pids
                .iter()
                .filter_map(|&p| c.s.proc(p))
                .filter(|p| !is_extension_host(p) && p.footprint >= 100 << 20)
                .map(|p| (child_label(p), p.footprint >> 20))
                .collect();
            heavy.sort_by(|a, b| b.1.cmp(&a.1));
            let mut detail = format!("{hosts} window(s)");
            if !heavy.is_empty() {
                let top: Vec<String> = heavy
                    .iter()
                    .take(3)
                    .map(|(n, m)| format!("{n} {}", fmt_mb(*m)))
                    .collect();
                detail += &format!("; heaviest: {}", top.join(", "));
            }
            Some(Finding {
                id: format!("extension-host:{}", slug(app)),
                rule: "extension-host".into(),
                safety: INFO,
                title: format!("{app} extensions use {}", fmt_mb(mb)),
                detail,
                frees_mb: mb,
                tip: "Command palette → \"Developer: Show Running Extensions\" shows which extension is heavy. Disable extensions per workspace where you don't need them; \"Developer: Restart Extension Host\" (or \"TypeScript: Restart TS Server\") gives leaked memory back.".into(),
                pids,
                ..Default::default()
            })
        })
        .collect();
    out.sort_by(|a, b| b.frees_mb.cmp(&a.frees_mb));
    out
}

/// "tsserver.js" for `node .../tsserver.js`, else the executable.
fn child_label(p: &Proc) -> String {
    let x = exe(p);
    if matches!(x, "node" | "bun") || x.contains("Helper") {
        if let Some(script) = p
            .args
            .split_whitespace()
            .skip(1)
            .find(|t| !t.starts_with('-'))
        {
            return base_name(script).to_string();
        }
    }
    x.to_string()
}

// ---- JS dev servers and test runners ----------------------------------------------------

fn project(cwd: &str) -> String {
    if cwd.is_empty() {
        String::new()
    } else {
        base_name(cwd).to_string()
    }
}

pub(super) fn dev_servers(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (p, kind) in roots(c, DEV_SERVERS) {
        let pids = c.s.tree(p.pid);
        if c.s.bytes_of(&pids) >> 20 < 300 {
            continue;
        }
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        let detached = p.ppid == 1;
        // CPU-idle is normal for a dev server you are using (it only works when you
        // save or reload), so ask for history, or a terminal that is gone.
        if !(idle_for_long(&idle) || (detached && idle.known && idle.idle)) {
            continue;
        }
        let cwd = probe::cwd(p.pid);
        let gone = dir_gone(&cwd);
        let mut why = Vec::new();
        if detached {
            why.push("its terminal is gone");
        }
        if gone {
            why.push("its project directory was deleted");
        }
        let mut detail = format!("{idle}, up {}", c.age(p));
        if !why.is_empty() {
            detail = format!("{}, {detail}", why.join(", "));
        }
        if !cwd.is_empty() {
            detail += &format!(", {}", tildify(&cwd));
        }
        let name = project(&cwd);
        let title = if name.is_empty() {
            kind.to_string()
        } else {
            format!("{kind} for {name}")
        };
        // Nobody can be developing in a deleted directory.
        let safety = if gone && detached && idle.known && idle.idle {
            SAFE
        } else {
            CONFIRM
        };
        out.push(c.term(
            "dev-server",
            p,
            pids,
            safety,
            title,
            detail,
            "stop it (SIGTERM); run your dev command again when you need it",
        ));
    }
    out
}

pub(super) fn test_runners(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (p, kind) in roots(c, &[JEST, VITEST]) {
        let pids = c.s.tree(p.pid);
        if c.s.bytes_of(&pids) >> 20 < 300 {
            continue;
        }
        let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
        if idle.known && !idle.idle {
            continue; // tests are running
        }
        let name = project(&probe::cwd(p.pid));
        let at = if name.is_empty() {
            String::new()
        } else {
            format!(" in {name}")
        };
        if p.ppid == 1 {
            // The run that owned them is gone; test workers hold no state.
            let safety = if idle.known { SAFE } else { CONFIRM };
            out.push(c.term(
                "test-workers",
                p,
                pids,
                safety,
                format!("Leftover {kind} workers{at}"),
                format!(
                    "the test run that started them is gone, up {}, {idle}",
                    c.age(p)
                ),
                "stop them; test workers hold no state",
            ));
        } else if idle_for_long(&idle) {
            out.push(c.term(
                "test-watch",
                p,
                pids,
                CONFIRM,
                format!("{kind} watch mode{at} is idle"),
                format!("{idle}, up {}", c.age(p)),
                "stop it (SIGTERM); start it again when you test",
            ));
        }
    }
    out
}

// ---- Electron apps -------------------------------------------------------------------------

/// Editors are reported through their extension hosts; their memory is the workload.
const SKIP_ELECTRON: &[&str] = &["Visual Studio Code", "Cursor", "Windsurf", "VSCodium"];

/// The outermost .app bundle of a path.
fn bundle_of(path: &str) -> Option<&str> {
    path.find(".app/Contents/").map(|i| &path[..i + 4])
}

pub fn is_electron_bundle(bundle: &str) -> bool {
    Path::new(bundle)
        .join("Contents/Frameworks/Electron Framework.framework")
        .exists()
}

pub(super) fn electron(c: &Ctx) -> Vec<Finding> {
    let apps: Vec<&Group> =
        c.s.groups
            .iter()
            .filter(|g| g.is_app && g.mb >= 300 && !SKIP_ELECTRON.contains(&g.name.as_str()))
            .filter(|g| {
                g.pids
                    .iter()
                    .filter_map(|&p| c.s.proc(p))
                    .find_map(|p| bundle_of(&p.path))
                    .is_some_and(is_electron_bundle)
            })
            .collect();
    let total: u64 = apps.iter().map(|g| g.mb).sum();
    if apps.len() < 2 || total < 1500 {
        return vec![];
    }
    let list: Vec<String> = apps
        .iter()
        .map(|g| format!("{} {}", g.name, fmt_mb(g.mb)))
        .collect();
    vec![Finding {
        id: "electron".into(),
        rule: "electron".into(),
        safety: INFO,
        title: format!("{} Electron apps use {}", apps.len(), fmt_mb(total)),
        detail: list.join(", "),
        frees_mb: total,
        tip: "Each Electron app ships its own copy of Chromium. Where a web version exists, a browser tab shares one engine with your other tabs; quit the apps you only check now and then.".into(),
        pids: apps.iter().flat_map(|g| g.pids.iter().copied()).collect(),
        ..Default::default()
    }]
}
