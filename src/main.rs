//! ramster: see what is eating your RAM, free it safely, and learn your habits.

use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use ramster::analyze::rules::CONFIRM;
use ramster::analyze::{self, act, report};
use ramster::config::Config;
use ramster::util::{fmt_gb, fmt_mb, now_unix};
use ramster::{daemon, history, mcp, ui, VERSION};

const USAGE: &str = "ramster: what is eating your RAM, and how to get it back.

Usage:
  ramster                     overview: memory, top apps, what can be freed
  ramster status              memory and top apps (fast, no CPU sampling)
  ramster suggest             everything that can be freed, with ids and safety tier
  ramster free                show the free plan (dry run)
  ramster free --safe         free every \"safe\" item (idle build daemons, orphaned MCP/LSP servers)
  ramster free <id|rule>...   free specific items (asks before \"confirm\" items unless --yes)
  ramster report [--hours N]  your usage behaviour over time (needs the daemon)
  ramster rules               custom rules from ~/.ramster/rules.d and any errors in them
  ramster daemon              run the sampler in the foreground
  ramster agent install       run the sampler in the background at login (launchd)
  ramster agent uninstall|status
  ramster mcp                 MCP server on stdio, so AI agents can check and free memory
  ramster version

Flags:
  --json      machine-readable output (for scripts and AI agents)
  --yes       don't ask before \"confirm\" items
  --force     SIGKILL processes that ignore SIGTERM

Config: ~/.ramster/config.json (created by \"ramster agent install\")
";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let json_out = take_flag(&mut args, "--json");
    let cmd = if args.is_empty() {
        String::new()
    } else {
        args.remove(0)
    };
    let res = match cmd.as_str() {
        "" => overview(json_out),
        "status" | "s" => status(json_out),
        "suggest" | "sg" => suggest(json_out),
        "free" | "f" => free(args, json_out),
        "report" | "r" => report_cmd(args, json_out),
        "rules" => rules_cmd(json_out),
        "daemon" => daemon::run_daemon(Config::load()),
        "agent" => daemon::cmd_agent(args.first().map(String::as_str).unwrap_or("status"), None),
        "mcp" => mcp::serve(VERSION).map_err(|e| e.to_string()),
        "version" | "--version" | "-v" | "-V" => {
            println!("ramster {VERSION}");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprint!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("ramster: {e}");
        std::process::exit(1);
    }
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != name);
    args.len() != before
}

fn print_json(v: &serde_json::Value) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, v).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

fn status(json_out: bool) -> Result<(), String> {
    let s = analyze::take(Duration::ZERO)?;
    if json_out {
        return print_json(
            &json!({"memory": mcp::mem_view(&s), "groups": &s.groups[..20.min(s.groups.len())]}),
        );
    }
    ui::mem(&s);
    ui::groups(&s, 12);
    Ok(())
}

fn overview(json_out: bool) -> Result<(), String> {
    let (s, fs) = mcp::findings(&Config::load())?;
    if json_out {
        return print_json(
            &json!({"memory": mcp::mem_view(&s), "groups": &s.groups[..15.min(s.groups.len())], "findings": fs}),
        );
    }
    ui::mem(&s);
    ui::groups(&s, 10);
    ui::findings(&fs, true);
    Ok(())
}

fn suggest(json_out: bool) -> Result<(), String> {
    let (s, fs) = mcp::findings(&Config::load())?;
    if json_out {
        return print_json(&json!({"memory": mcp::mem_view(&s), "findings": fs}));
    }
    ui::mem_line(&s);
    ui::findings(&fs, false);
    Ok(())
}

fn free(mut args: Vec<String>, json_out: bool) -> Result<(), String> {
    let safe = take_flag(&mut args, "--safe");
    let all = take_flag(&mut args, "--all");
    let yes = take_flag(&mut args, "--yes") | take_flag(&mut args, "-y");
    let force = take_flag(&mut args, "--force");
    if let Some(bad) = args.iter().find(|a| a.starts_with('-')) {
        return Err(format!("unknown flag {bad}"));
    }
    let mut ids = args;
    if safe {
        ids.push("safe".into());
    }
    if all {
        ids.push("all".into());
    }
    let (s, fs) = mcp::findings(&Config::load())?;
    if ids.is_empty() {
        if json_out {
            return print_json(&json!({"dry_run": true, "findings": fs}));
        }
        ui::mem_line(&s);
        ui::findings(&fs, false);
        return Ok(());
    }
    let chosen = mcp::select(&fs, &ids);
    if chosen.is_empty() {
        if json_out {
            return print_json(&json!({"results": [], "note": "nothing matched"}));
        }
        println!(
            "Nothing to free for {} (run `ramster suggest` for current ids).",
            ids.join(", ")
        );
        return Ok(());
    }
    let tty = std::io::stdin().is_terminal() && !json_out;
    let before = s.mem.used;
    let mut results = Vec::new();
    for f in chosen {
        if f.safety == CONFIRM && !yes {
            let skip = if !tty {
                Some("skipped: needs confirmation (pass --yes)")
            } else if !ask(&format!(
                "{} {}: {}. {}?",
                ui::c(ui::YELLOW, "?"),
                f.title,
                fmt_mb(f.frees_mb),
                f.action
            )) {
                Some("skipped")
            } else {
                None
            };
            if let Some(msg) = skip {
                results.push(act::Outcome {
                    id: f.id.clone(),
                    message: msg.into(),
                    ..Default::default()
                });
                continue;
            }
        }
        let r = act::execute(f, force);
        if !json_out {
            let mark = if r.ok {
                ui::c(ui::GREEN, "✓")
            } else {
                ui::c(ui::RED, "✗")
            };
            println!(
                "{mark} {:<34} {:>8}  {}",
                f.title,
                fmt_mb(f.frees_mb),
                ui::c(ui::DIM, &r.message)
            );
        }
        results.push(r);
    }
    std::thread::sleep(Duration::from_secs(2));
    let after = analyze::take(Duration::ZERO).ok();
    if json_out {
        let mut out = json!({"results": results});
        if let Some(a) = &after {
            out["memory_after"] = mcp::mem_view(a);
        }
        return print_json(&out);
    }
    if let Some(a) = after {
        let drop = (before >> 20) as i64 - (a.mem.used >> 20) as i64;
        println!(
            "\nMemory used: {} → {} ({:+.1} GB), pressure {}",
            fmt_gb(before),
            fmt_gb(a.mem.used),
            -(drop as f64) / 1024.0,
            ui::pressure_word(a.mem.pressure)
        );
    }
    Ok(())
}

fn report_cmd(args: Vec<String>, json_out: bool) -> Result<(), String> {
    let mut hours = 24.0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = if let Some(v) = a.strip_prefix("--hours=") {
            v.to_string()
        } else if a == "--hours" {
            it.next().cloned().ok_or("--hours needs a value")?
        } else {
            return Err(format!("unknown argument {a}"));
        };
        hours = v.parse().map_err(|_| format!("bad --hours value {v:?}"))?;
    }
    let samples = history::load(now_unix() - (hours * 3600.0) as i64);
    if samples.is_empty() {
        if json_out {
            return print_json(&json!({"error": "no history yet"}));
        }
        println!("No history yet. Start the background sampler with:\n\n  ramster agent install\n\nIt samples once a minute and uses a few MB of RAM. Come back in a few hours (a full working day is best).");
        return Ok(());
    }
    let r = report::build(&samples);
    if json_out {
        return print_json(&serde_json::to_value(&r).map_err(|e| e.to_string())?);
    }
    ui::report(&r);
    Ok(())
}

fn rules_cmd(json_out: bool) -> Result<(), String> {
    let cfg = Config::load();
    let dir = ramster::config::rules_dir();
    if json_out {
        return print_json(&json!({
            "dir": dir.display().to_string(),
            "rules": cfg.custom_rules,
            "errors": cfg.rule_errors,
        }));
    }
    println!("Custom rules in {}", dir.display());
    if cfg.custom_rules.is_empty() && cfg.rule_errors.is_empty() {
        println!("  none yet. Drop rule packs (*.json) there; see README \"Custom rules\".");
    }
    for r in &cfg.custom_rules {
        println!(
            "  {:<24} {:<8} {}",
            r.id,
            r.tier,
            ui::c(ui::DIM, &format!("{} · {}", r.title, r.source))
        );
    }
    for e in &cfg.rule_errors {
        println!("  {} {e}", ui::c(ui::RED, "✗"));
    }
    if !cfg.rule_errors.is_empty() {
        return Err(format!("{} problem(s) in rules.d", cfg.rule_errors.len()));
    }
    Ok(())
}

fn ask(q: &str) -> bool {
    print!("{q} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}
