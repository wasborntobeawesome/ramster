//! `ramster setup`: register the MCP server and the memory_check instruction
//! with the AI agents installed on this Mac, asking before every change.
//!
//! Every edit is idempotent, backs the file up once (`<file>.ramster.bak`) and
//! is written atomically. The editing functions are pure so they can be tested.

use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ui::{c, DIM, GREEN, RED, YELLOW};
use crate::util::{ask, base_name, run, tildify, which};

/// The line agents read in their global instructions file.
pub const LINE: &str = "Before starting memory-heavy work (Gradle/Xcode builds, emulators, Docker, big test runs), call ramster's memory_check.";
const NAME: &str = "ramster";
const MCP: &str = "MCP server";
const RULE: &str = "instructions";

/// What a step looks for (and, for file edits, writes).
#[derive(Clone, Debug)]
pub enum Edit {
    /// `<key>.ramster = value` in a JSON file.
    Json { key: &'static str, value: Value },
    /// `[mcp_servers.ramster]` in Codex's config.toml.
    Toml { bin: String },
    /// The instruction line in a markdown file.
    Line,
}

/// How a step is carried out.
#[derive(Clone, Debug)]
pub enum Via {
    /// Edit `Step::file` directly.
    File,
    /// Run the agent's own CLI: (program, add args, remove args).
    Cli(String, Vec<String>, Vec<String>),
    /// Nothing ramster can safely write; tell the user (add, remove).
    Manual(String, String),
}

#[derive(Clone, Debug)]
pub struct Step {
    pub agent: &'static str,
    pub what: &'static str,
    /// File that shows whether the step is done; None = cannot be checked.
    pub file: Option<PathBuf>,
    pub edit: Edit,
    pub via: Via,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Nothing to do (already set up, or nothing to remove).
    Done,
    Todo,
    /// Needs the user's hands; the message says what to do.
    Manual(String),
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

fn line_step(agent: &'static str, file: PathBuf) -> Step {
    Step {
        agent,
        what: RULE,
        file: Some(file),
        edit: Edit::Line,
        via: Via::File,
    }
}

fn json_step(agent: &'static str, file: PathBuf, key: &'static str, value: Value) -> Step {
    Step {
        agent,
        what: MCP,
        file: Some(file),
        edit: Edit::Json { key, value },
        via: Via::File,
    }
}

/// Which agents are installed under `home` (name, detected) and the steps for
/// the detected ones. `which` finds a CLI on PATH.
pub fn plan(
    home: &Path,
    bin: &str,
    which: &dyn Fn(&str) -> Option<String>,
) -> (Vec<(&'static str, bool)>, Vec<Step>) {
    let p = |rel: &str| home.join(rel);
    let server = json!({"command": bin, "args": ["mcp"]});
    let mut agents = Vec::new();
    let mut steps = Vec::new();

    let claude = which("claude").or_else(|| {
        [".local/bin/claude", ".claude/local/claude"]
            .iter()
            .map(|r| p(r))
            .find(|f| f.is_file())
            .map(|f| f.to_string_lossy().into_owned())
    });
    let found = claude.is_some() || p(".claude").is_dir();
    agents.push(("Claude Code", found));
    if found {
        let add = strings(&["mcp", "add", "--scope", "user", NAME, "--", bin, "mcp"]);
        let rm = strings(&["mcp", "remove", "--scope", "user", NAME]);
        let via = match claude {
            Some(c) => Via::Cli(c, add, rm),
            None => Via::Manual(
                format!("claude CLI not found; run: claude {}", add.join(" ")),
                format!("claude CLI not found; run: claude {}", rm.join(" ")),
            ),
        };
        let mut s = json_step(
            "Claude Code",
            p(".claude.json"),
            "mcpServers",
            server.clone(),
        );
        s.via = via;
        steps.push(s);
        steps.push(line_step("Claude Code", p(".claude/CLAUDE.md")));
    }

    let codex = which("codex");
    let found = codex.is_some() || p(".codex").is_dir();
    agents.push(("Codex", found));
    if found {
        steps.push(Step {
            agent: "Codex",
            what: MCP,
            file: Some(p(".codex/config.toml")),
            edit: Edit::Toml { bin: bin.into() },
            via: match codex {
                Some(c) => Via::Cli(
                    c,
                    strings(&["mcp", "add", NAME, "--", bin, "mcp"]),
                    strings(&["mcp", "remove", NAME]),
                ),
                None => Via::File,
            },
        });
        steps.push(line_step("Codex", p(".codex/AGENTS.md")));
    }

    let found = p(".cursor").is_dir() || which("cursor-agent").is_some();
    agents.push(("Cursor", found));
    if found {
        steps.push(json_step(
            "Cursor",
            p(".cursor/mcp.json"),
            "mcpServers",
            server.clone(),
        ));
        steps.push(Step {
            file: None,
            via: Via::Manual(
                format!("Cursor keeps user rules in the app: Cursor Settings > Rules > User Rules. Paste: {LINE}"),
                "remove the memory_check line from Cursor Settings > Rules > User Rules, if you added it".into(),
            ),
            ..line_step("Cursor", PathBuf::new())
        });
    }

    let found = which("gemini").is_some() || p(".gemini").is_dir();
    agents.push(("Gemini CLI", found));
    if found {
        steps.push(json_step(
            "Gemini CLI",
            p(".gemini/settings.json"),
            "mcpServers",
            server,
        ));
        steps.push(line_step("Gemini CLI", p(".gemini/GEMINI.md")));
    }

    let dir = p(".config/opencode");
    let found = which("opencode").is_some() || dir.is_dir();
    agents.push(("opencode", found));
    if found {
        let (json, jsonc) = (dir.join("opencode.json"), dir.join("opencode.jsonc"));
        let file = if !json.exists() && jsonc.exists() {
            jsonc
        } else {
            json
        };
        let value = json!({"type": "local", "command": [bin, "mcp"], "enabled": true});
        steps.push(json_step("opencode", file, "mcp", value));
        steps.push(line_step("opencode", dir.join("AGENTS.md")));
    }
    (agents, steps)
}

// ---- pure edits: None = nothing to change ----------------------------------

/// Add (`value` is Some) or remove `ramster` under `key` in a JSON object,
/// keeping every other key in its order. Refuses anything that isn't plain JSON.
pub fn json_edit(text: &str, key: &str, value: Option<&Value>) -> Result<Option<String>, String> {
    let mut root: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text).map_err(|_| "is not plain JSON (comments?)".to_string())?
    };
    let obj = root.as_object_mut().ok_or("is not a JSON object")?;
    match value {
        Some(v) => {
            let servers = obj.entry(key).or_insert_with(|| json!({}));
            let servers = servers
                .as_object_mut()
                .ok_or(format!("has a \"{key}\" that is not an object"))?;
            if servers.contains_key(NAME) {
                return Ok(None);
            }
            servers.insert(NAME.into(), v.clone());
        }
        None => {
            let removed = obj
                .get_mut(key)
                .and_then(Value::as_object_mut)
                .and_then(|s| s.shift_remove(NAME));
            if removed.is_none() {
                return Ok(None);
            }
            // Leave no empty `"mcpServers": {}` behind.
            if obj[key].as_object().is_some_and(|s| s.is_empty()) {
                obj.shift_remove(key);
            }
        }
    }
    let mut out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(Some(out))
}

/// `[mcp_servers.ramster]` or one of its subtables (`[mcp_servers.ramster.env]`).
fn is_ramster_table(line: &str) -> bool {
    let h: String = line
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect();
    let h = h.split_inclusive(']').next().unwrap_or("");
    h == "[mcp_servers.ramster]" || h.starts_with("[mcp_servers.ramster.")
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Ensure the text ends with a blank line (unless empty) so a block can follow.
fn blank_line_after(text: &str) -> String {
    let mut s = text.to_string();
    if !s.trim().is_empty() {
        while !s.ends_with("\n\n") {
            s.push('\n');
        }
    } else {
        s.clear();
    }
    s
}

/// Text without trailing blank lines, ending in one newline (or empty).
fn tidy_end(s: &str) -> String {
    let s = s.trim_end();
    if s.is_empty() {
        String::new()
    } else {
        format!("{s}\n")
    }
}

pub fn toml_add(text: &str, bin: &str) -> Option<String> {
    if text.lines().any(is_ramster_table) {
        return None;
    }
    Some(format!(
        "{}[mcp_servers.{NAME}]\ncommand = {}\nargs = [\"mcp\"]\n",
        blank_line_after(text),
        toml_str(bin)
    ))
}

/// Drop `[mcp_servers.ramster]` and its subtables, up to the next other table.
pub fn toml_remove(text: &str) -> Option<String> {
    if !text.lines().any(is_ramster_table) {
        return None;
    }
    let mut skipping = false;
    let mut kept = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            skipping = is_ramster_table(line);
        }
        if !skipping {
            kept.push(line);
        }
    }
    Some(tidy_end(&kept.join("\n")))
}

pub fn line_add(text: &str) -> Option<String> {
    if text.contains("memory_check") {
        return None;
    }
    Some(format!("{}{LINE}\n", blank_line_after(text)))
}

pub fn line_remove(text: &str) -> Option<String> {
    if !text.lines().any(|l| l.trim() == LINE) {
        return None;
    }
    let kept: Vec<&str> = text.lines().filter(|l| l.trim() != LINE).collect();
    Some(tidy_end(&kept.join("\n")))
}

// ---- files ------------------------------------------------------------------

fn read(file: &Path) -> Result<String, String> {
    match std::fs::read_to_string(file) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", tildify(&file.to_string_lossy()))),
    }
}

/// One-time copy to `<file>.ramster.bak` before the first change.
pub fn backup(file: &Path) -> Result<(), String> {
    let bak = PathBuf::from(format!("{}.ramster.bak", file.display()));
    if file.exists() && !bak.exists() {
        std::fs::copy(file, &bak).map_err(|e| format!("backup {}: {e}", bak.display()))?;
    }
    Ok(())
}

/// Write via a temp file and rename, through symlinks, keeping permissions.
pub fn write_atomic(file: &Path, text: &str) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", file.display());
    let real = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if let Some(dir) = real.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let name = real.file_name().unwrap_or_default().to_string_lossy();
    let tmp = real.with_file_name(format!(".{name}.ramster.tmp"));
    std::fs::write(&tmp, text).map_err(err)?;
    if let Ok(m) = std::fs::metadata(&real) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, &real).map_err(err)
}

impl Step {
    /// The file's new text, or None if there is nothing to change.
    fn edited(&self, file: &Path, remove: bool) -> Result<Option<String>, String> {
        let text = read(file)?;
        Ok(match &self.edit {
            Edit::Json { key, value } => {
                return json_edit(&text, key, (!remove).then_some(value)).map_err(|e| {
                    let f = tildify(&file.to_string_lossy());
                    if remove {
                        format!("{f} {e}, left untouched; remove \"{NAME}\" from \"{key}\" by hand")
                    } else {
                        format!("{f} {e}, left untouched; add by hand: \"{key}\": {{\"{NAME}\": {value}}}")
                    }
                })
            }
            Edit::Toml { .. } if remove => toml_remove(&text),
            Edit::Toml { bin } => toml_add(&text, bin),
            Edit::Line if remove => line_remove(&text),
            Edit::Line => line_add(&text),
        })
    }

    pub fn status(&self, remove: bool) -> Status {
        let Some(file) = &self.file else {
            return Status::Manual(self.action(remove));
        };
        match self.edited(file, remove) {
            Err(e) => Status::Manual(e),
            Ok(None) => Status::Done,
            Ok(Some(_)) if matches!(self.via, Via::Manual(..)) => {
                Status::Manual(self.action(remove))
            }
            Ok(Some(_)) => Status::Todo,
        }
    }

    /// What the step does, in words.
    pub fn action(&self, remove: bool) -> String {
        let f = self
            .file
            .as_ref()
            .map(|f| tildify(&f.to_string_lossy()))
            .unwrap_or_default();
        let (verb, to) = if remove {
            ("remove", "from")
        } else {
            ("add", "to")
        };
        match (&self.via, &self.edit) {
            (Via::Manual(add, rm), _) => (if remove { rm } else { add }).clone(),
            (Via::Cli(prog, add, rm), _) => {
                format!(
                    "{} {}",
                    base_name(prog),
                    (if remove { rm } else { add }).join(" ")
                )
            }
            (Via::File, Edit::Json { key, .. }) => format!("{verb} {key}.{NAME} {to} {f}"),
            (Via::File, Edit::Toml { .. }) => format!("{verb} [mcp_servers.{NAME}] {to} {f}"),
            (Via::File, Edit::Line) => format!("{verb} the memory_check line {to} {f}"),
        }
    }

    /// Carry the step out; Ok(message) on success.
    pub fn apply(&self, remove: bool) -> Result<String, String> {
        let file = self.file.as_ref().ok_or("nothing to apply")?;
        match &self.via {
            Via::Manual(..) => Err(self.action(remove)),
            Via::Cli(prog, add, rm) => {
                backup(file)?;
                let args: Vec<&str> = (if remove { rm } else { add })
                    .iter()
                    .map(String::as_str)
                    .collect();
                match run(Duration::from_secs(60), prog, &args) {
                    (true, _) => Ok(format!("ran {}", self.action(remove))),
                    (false, out) => Err(format!("{} failed: {out}", self.action(remove))),
                }
            }
            Via::File => {
                let Some(text) = self.edited(file, remove)? else {
                    return Ok("already done".into());
                };
                backup(file)?;
                write_atomic(file, &text)?;
                Ok(format!("updated {}", tildify(&file.to_string_lossy())))
            }
        }
    }
}

// ---- command ----------------------------------------------------------------

fn take(args: &mut Vec<String>, names: &[&str]) -> bool {
    let before = args.len();
    args.retain(|a| !names.contains(&a.as_str()));
    args.len() != before
}

/// `ramster setup [--yes] [--dry-run] [--remove] [--json]`
pub fn cmd(mut args: Vec<String>, json_out: bool) -> Result<(), String> {
    let yes = take(&mut args, &["--yes", "-y"]);
    let dry_run = take(&mut args, &["--dry-run", "-n"]);
    let remove = take(&mut args, &["--remove", "--uninstall"]);
    if let Some(bad) = args.first() {
        return Err(format!(
            "unknown argument {bad}\nusage: ramster setup [--yes] [--dry-run] [--remove] [--json]"
        ));
    }
    let bin = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let home = PathBuf::from(crate::util::home());
    let (agents, steps) = plan(&home, &bin, &|n| which(n, &[]));
    let status: Vec<Status> = steps.iter().map(|s| s.status(remove)).collect();
    let todo = status.iter().filter(|s| **s == Status::Todo).count();
    let interactive = !yes && !dry_run && !json_out && std::io::stdin().is_terminal();
    let changing = !dry_run && (yes || interactive);
    let apply = changing && todo > 0;
    let warning = (bin.contains("/target/") && !remove).then(|| {
        format!("{} is a cargo build dir; agents will break after `cargo clean`. Run `make install` first and set up from ~/.local/bin/ramster.", tildify(&bin))
    });

    if !json_out {
        print_plan(&agents, &steps, &status, &bin, remove, warning.as_deref());
    }
    let mut results: Vec<Option<(&str, String)>> = vec![None; steps.len()];
    if apply {
        for (i, s) in steps.iter().enumerate() {
            if status[i] != Status::Todo {
                continue;
            }
            if interactive && !ask(&format!("{}: {}?", s.agent, s.action(remove))) {
                results[i] = Some(("skipped", "skipped".into()));
                continue;
            }
            let r = s.apply(remove);
            if !json_out {
                let (mark, msg) = match &r {
                    Ok(m) => (c(GREEN, "✓"), m),
                    Err(e) => (c(RED, "✗"), e),
                };
                println!("  {mark} {:<12} {msg}", s.agent);
            }
            results[i] = Some(match r {
                Ok(m) => ("changed", m),
                Err(e) => ("failed", e),
            });
        }
    }

    if json_out {
        let steps: Vec<Value> = steps
            .iter()
            .zip(&status)
            .zip(&results)
            .map(|((s, st), r)| {
                let (state, note) = match st {
                    Status::Done => ("done", None),
                    Status::Todo => ("todo", None),
                    Status::Manual(m) => ("manual", Some(m.clone())),
                };
                json!({
                    "agent": s.agent, "step": if s.what == MCP { "mcp" } else { "instructions" },
                    "file": s.file, "action": s.action(remove), "state": state, "note": note,
                    "result": r.as_ref().map(|r| r.0), "message": r.as_ref().map(|r| &r.1),
                })
            })
            .collect();
        let agents: Vec<Value> = agents
            .iter()
            .map(|(n, d)| json!({"name": n, "detected": d}))
            .collect();
        let out = json!({"binary": bin, "remove": remove, "dry_run": !changing, "warning": warning, "agents": agents, "steps": steps});
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if todo == 0 {
        println!("\nNothing to change.");
    } else if !changing {
        println!(
            "\nDry run: nothing changed. Run `ramster setup{} --yes` to apply all, or run it in a terminal to choose per step.",
            if remove { " --remove" } else { "" }
        );
    } else if !remove {
        println!("\nRestart running agent sessions so they pick up the ramster MCP server.");
    }
    Ok(())
}

fn print_plan(
    agents: &[(&str, bool)],
    steps: &[Step],
    status: &[Status],
    bin: &str,
    remove: bool,
    warning: Option<&str>,
) {
    println!(
        "ramster setup{} · binary {}",
        if remove { " --remove" } else { "" },
        tildify(bin)
    );
    if let Some(w) = warning {
        println!("{} {w}", c(YELLOW, "warning:"));
    }
    let names =
        |want: bool| -> Vec<&str> { agents.iter().filter(|a| a.1 == want).map(|a| a.0).collect() };
    let (found, missing) = (names(true), names(false));
    if found.is_empty() {
        println!("No supported agents found (Claude Code, Codex, Cursor, Gemini CLI, opencode).");
        return;
    }
    print!("Agents: {}", found.join(", "));
    if !missing.is_empty() {
        print!(
            " {}",
            c(DIM, &format!("· not found: {}", missing.join(", ")))
        );
    }
    println!("\n");
    for (s, st) in steps.iter().zip(status) {
        let (mark, text) = match st {
            Status::Done if remove => (c(GREEN, "✓"), c(DIM, "nothing to remove")),
            Status::Done => (c(GREEN, "✓"), c(DIM, "already done")),
            Status::Todo => (c(YELLOW, "•"), s.action(remove)),
            Status::Manual(m) => (c(YELLOW, "!"), format!("by hand: {m}")),
        };
        println!("  {mark} {:<12} {:<13} {text}", s.agent, s.what);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ramster-setup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_keeps_other_keys_in_order() {
        let text = r#"{"z": 1, "mcpServers": {"b": {"command": "b"}, "a": {}}, "a": [true]}"#;
        let v = json!({"command": "/bin/ramster", "args": ["mcp"]});
        let out = json_edit(text, "mcpServers", Some(&v)).unwrap().unwrap();
        assert!(out.ends_with("}\n") && out.contains("\n  \"z\": 1,"));
        let keys = |s: &str| -> Vec<String> {
            let p: Value = serde_json::from_str(s).unwrap();
            let mut k: Vec<String> = p.as_object().unwrap().keys().cloned().collect();
            k.extend(p["mcpServers"].as_object().unwrap().keys().cloned());
            k
        };
        assert_eq!(keys(&out), ["z", "mcpServers", "a", "b", "a", "ramster"]);
        assert_eq!(json_edit(&out, "mcpServers", Some(&v)).unwrap(), None);
        let back = json_edit(&out, "mcpServers", None).unwrap().unwrap();
        assert_eq!(keys(&back), ["z", "mcpServers", "a", "b", "a"]);
        assert_eq!(json_edit(&back, "mcpServers", None).unwrap(), None);
        // An object only ramster was in goes away with it.
        let only = json_edit("{\"x\": 1}", "mcp", Some(&v)).unwrap().unwrap();
        assert_eq!(
            json_edit(&only, "mcp", None).unwrap().unwrap(),
            "{\n  \"x\": 1\n}\n"
        );
        // Missing or empty file: created from scratch.
        let new = json_edit("", "mcp", Some(&v)).unwrap().unwrap();
        assert!(new.starts_with("{\n  \"mcp\": {\n    \"ramster\""));
    }

    #[test]
    fn json_refuses_jsonc() {
        let text = "{\n  // my servers\n  \"mcp\": {}\n}\n";
        assert!(json_edit(text, "mcp", Some(&json!({}))).is_err());
        assert!(json_edit(text, "mcp", None).is_err());
        assert!(json_edit("[1]", "mcp", Some(&json!({}))).is_err());
    }

    #[test]
    fn toml_append_and_remove() {
        let orig =
            "model = \"o3\"\n\n[mcp_servers.figma]\nurl = \"https://x\"\n\n[desktop]\nfoo = true\n";
        let added = toml_add(orig, "/u/bin/ramster").unwrap();
        assert_eq!(
            added,
            format!(
                "{orig}\n[mcp_servers.ramster]\ncommand = \"/u/bin/ramster\"\nargs = [\"mcp\"]\n"
            )
        );
        assert_eq!(toml_add(&added, "/other"), None);
        assert_eq!(toml_remove(&added).unwrap(), orig);
        assert_eq!(toml_remove(orig), None);
        // In the middle, with a subtable, as `codex mcp add` might write it.
        let mid = "[mcp_servers.figma]\nurl = \"u\"\n\n[mcp_servers.\"ramster\"]  # ours\ncommand = \"r\"\n\n[mcp_servers.ramster.env]\nA = \"1\"\n\n[desktop]\nfoo = true\n";
        assert_eq!(
            toml_remove(mid).unwrap(),
            "[mcp_servers.figma]\nurl = \"u\"\n\n[desktop]\nfoo = true\n"
        );
        assert!(!is_ramster_table("[mcp_servers.ramster_two]"));
        assert_eq!(
            toml_add("", "/b").unwrap(),
            "[mcp_servers.ramster]\ncommand = \"/b\"\nargs = [\"mcp\"]\n"
        );
    }

    #[test]
    fn instruction_line() {
        assert_eq!(line_add("").unwrap(), format!("{LINE}\n"));
        let orig = "# Rules\nBe brief.";
        let added = line_add(orig).unwrap();
        assert_eq!(added, format!("# Rules\nBe brief.\n\n{LINE}\n"));
        assert_eq!(line_add(&added), None);
        assert_eq!(line_add("Use ramster memory_check first."), None);
        assert_eq!(line_remove(&added).unwrap(), "# Rules\nBe brief.\n");
        assert_eq!(line_remove(orig), None);
        assert_eq!(line_remove(&line_add("").unwrap()).unwrap(), "");
    }

    #[test]
    fn plan_apply_idempotent_with_backup() {
        let home = temp_home("plan");
        for d in [
            ".gemini",
            ".cursor",
            ".codex",
            ".claude",
            ".config/opencode",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let settings = home.join(".gemini/settings.json");
        let orig = "{\"ide\": {\"enabled\": true}, \"security\": {}}";
        std::fs::write(&settings, orig).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"o3\"\n").unwrap();
        std::fs::write(
            home.join(".config/opencode/opencode.json"),
            "{\n // jsonc\n}",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (agents, steps) = plan(&home, "/u/bin/ramster", &|_| None);
        assert!(agents.iter().all(|a| a.1));
        let todo = |remove: bool| -> Vec<String> {
            steps
                .iter()
                .filter(|s| s.status(remove) == Status::Todo)
                .map(|s| format!("{}/{}", s.agent, s.what))
                .collect()
        };
        // Claude without its CLI, Cursor rules and the JSONC file are manual.
        assert_eq!(todo(false).len(), 7);
        assert!(matches!(steps[0].status(false), Status::Manual(_)));
        assert!(
            matches!(steps.iter().find(|s| s.agent == "opencode").unwrap().status(false), Status::Manual(m) if m.contains("not plain JSON"))
        );
        for s in steps.iter().filter(|s| s.status(false) == Status::Todo) {
            s.apply(false).unwrap();
        }
        assert!(todo(false).is_empty(), "second run changes nothing");
        let bak = home.join(".gemini/settings.json.ramster.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig);
        let now = std::fs::read_to_string(&settings).unwrap();
        assert!(now.find("\"ide\"").unwrap() < now.find("\"mcpServers\"").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&settings).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(
            home.join(".claude/CLAUDE.md").exists()
                && !home.join(".claude/CLAUDE.md.ramster.bak").exists()
        );

        assert_eq!(todo(true).len(), 7);
        for s in steps.iter().filter(|s| s.status(true) == Status::Todo) {
            s.apply(true).unwrap();
        }
        assert!(todo(true).is_empty());
        assert_eq!(
            std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
            "model = \"o3\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            orig,
            "backup is one-time"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
