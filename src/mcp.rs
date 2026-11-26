//! MCP server over stdio (newline-delimited JSON-RPC 2.0), hand-rolled and
//! synchronous to stay tiny. Supports the `initialize` handshake and the
//! stateless `server/discover` method.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::Duration;

use crate::analyze::rules::{Finding, CONFIRM, SAFE};
use crate::analyze::{self, act, report, Snapshot};
use crate::config::Config;
use crate::history;
use crate::probe;
use crate::util::now_unix;

const LATEST_PROTOCOL: &str = "2025-11-25";
/// Versions whose message shapes this server handles.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "ramster reports and frees RAM on this Mac. Several agents and dev tools share one machine, so:
- Call memory_check before heavy work (Gradle/Xcode builds, emulators/simulators, Docker, big test suites, large installs).
- If it says there's not enough headroom, free the \"safe\" items it lists with memory_free (idle build daemons, orphaned MCP/LSP servers). They restart on demand.
- \"confirm\" items (emulators, Docker, GUI apps, detached processes) stop things a human may be using. Ask the user first, then call memory_free with confirm=true.
- Never free anything just to tidy up when pressure is normal and headroom is fine.";

fn tools() -> Value {
    json!([
        {
            "name": "memory_status",
            "description": "System memory (used/compressed/swap/pressure) and the apps and tools using the most RAM, grouped by app (helpers included). Fast and read-only.",
            "inputSchema": {"type": "object", "properties": {"top": {"type": "integer", "description": "how many groups to return (default 15)"}}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "memory_check",
            "description": "Ask before starting memory-heavy work: is there enough headroom for need_mb more MB? If not, returns which findings to free and how much each gives back.",
            "inputSchema": {"type": "object", "properties": {
                "need_mb": {"type": "integer", "description": "extra RAM the task needs, e.g. 4000 for a Gradle build, 3000 for an emulator"},
                "purpose": {"type": "string", "description": "what you are about to run"}
            }, "required": ["need_mb"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "memory_suggest",
            "description": "Things that can be freed right now, each with an id, safety tier (safe | confirm | info), estimated MB and reason. Takes about a second because it measures CPU so busy processes are never touched.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "memory_free",
            "description": "Free memory by executing findings from memory_suggest or memory_check. ids may be finding ids (gradle-daemon:1234), rule names (gradle-daemon, orphan-mcp), or [\"safe\"] for every safe finding. 'confirm' findings require confirm=true, which you may only pass after the user agreed.",
            "inputSchema": {"type": "object", "properties": {
                "ids": {"type": "array", "items": {"type": "string"}},
                "confirm": {"type": "boolean", "description": "the user explicitly approved stopping confirm-tier items"},
                "force": {"type": "boolean", "description": "SIGKILL processes that ignore SIGTERM"}
            }, "required": ["ids"]},
            "annotations": {"destructiveHint": true}
        },
        {
            "name": "memory_report",
            "description": "Usage behaviour over the last N hours from the background daemon: pressure over time, heaviest apps, and apps that held memory while idle (forgotten), with advice.",
            "inputSchema": {"type": "object", "properties": {"hours": {"type": "number", "description": "window, default 24"}}},
            "annotations": {"readOnlyHint": true}
        }
    ])
}

/// Serve until stdin closes.
pub fn serve(version: &str) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Err(_) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
            ),
            Ok(req) => handle(&req, version),
        };
        if let Some(r) = reply {
            serde_json::to_writer(&mut out, &r)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Handle one message; None for notifications.
pub fn handle(req: &Value, version: &str) -> Option<Value> {
    let id = req.get("id")?.clone();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" | "server/discover" => {
            // Echo the client's version only if we support it; otherwise offer our latest.
            let proto = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
                .unwrap_or(LATEST_PROTOCOL);
            Ok(json!({
                "protocolVersion": proto,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ramster", "version": version},
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match call(name, &args) {
                Ok(data) => json!({
                    "content": [{"type": "text", "text": data.to_string()}],
                    "structuredContent": data
                }),
                Err(e) => json!({"isError": true, "content": [{"type": "text", "text": e}]}),
            })
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

fn call(name: &str, args: &Value) -> Result<Value, String> {
    let cfg = Config::load();
    match name {
        "memory_status" => {
            let s = analyze::take(Duration::ZERO)?;
            let top = args.get("top").and_then(Value::as_u64).unwrap_or(15) as usize;
            Ok(json!({"memory": mem_view(&s), "groups": &s.groups[..top.min(s.groups.len())]}))
        }
        "memory_suggest" => {
            let (s, fs) = findings(&cfg)?;
            Ok(
                json!({"memory": mem_view(&s), "findings": fs, "reclaimable_safe_mb": sum_mb(&fs, SAFE)}),
            )
        }
        "memory_check" => check(
            &cfg,
            args.get("need_mb").and_then(Value::as_i64).unwrap_or(0),
        ),
        "memory_free" => {
            let ids: Vec<String> = args
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let confirm = args
                .get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            free(&cfg, &ids, confirm, force)
        }
        "memory_report" => {
            let hours = args
                .get("hours")
                .and_then(Value::as_f64)
                .filter(|h| *h > 0.0)
                .unwrap_or(24.0);
            let samples = history::load(now_unix() - (hours * 3600.0) as i64);
            if samples.is_empty() {
                return Ok(
                    json!({"error": "no history yet: the background daemon isn't running. The user can start it with `ramster agent install`."}),
                );
            }
            Ok(json!({"report": report::build(&samples)}))
        }
        _ => Err(format!("unknown tool {name:?}")),
    }
}

/// CPU-measured snapshot with history attached, plus findings.
pub fn findings(cfg: &Config) -> Result<(Snapshot, Vec<Finding>), String> {
    let mut s = analyze::take(Duration::from_secs(1))?;
    let window = cfg.idle_minutes as i64 * 60;
    s.attach_history(&history::load(now_unix() - window - 120), window);
    let fs = analyze::rules::suggest(&s, cfg);
    Ok((s, fs))
}

/// Findings measured against an earlier process listing (the CPU window is the
/// time since then), so interactive callers don't have to sleep. Falls back to a
/// short live window when there is no usable baseline.
pub fn findings_since(
    cfg: &Config,
    baseline: Option<(&[probe::Proc], std::time::Instant)>,
) -> Result<(Snapshot, Vec<Finding>), String> {
    let mut s = match baseline {
        Some((prev, t)) if t.elapsed() >= Duration::from_millis(300) => {
            let procs = probe::procs()?;
            let cpu = analyze::cpu_between(prev, &procs, t.elapsed());
            Snapshot::build(procs, cpu, probe::memory()?)
        }
        _ => analyze::take(Duration::from_millis(250))?,
    };
    let window = cfg.idle_minutes as i64 * 60;
    s.attach_history(&history::load(now_unix() - window - 120), window);
    // Interactive path: never wait on docker/lsof; cached facts refresh in the background.
    let fs = analyze::rules::suggest_nonblocking(&s, cfg);
    Ok((s, fs))
}

/// Memory obtainable without pressure: free + file cache, minus a 2 GB margin.
pub fn headroom_mb(s: &Snapshot) -> i64 {
    let mut h = ((s.mem.free + s.mem.cached) >> 20) as i64 - 2048;
    if s.mem.pressure >= 2 {
        h -= (s.mem.compressed >> 20) as i64 / 4; // the compressor is already working hard
    }
    h.max(0)
}

/// Compact, unit-labelled memory summary for JSON consumers.
pub fn mem_view(s: &Snapshot) -> Value {
    let m = &s.mem;
    json!({
        "total_mb": m.total >> 20, "used_mb": m.used >> 20, "app_mb": m.app >> 20, "wired_mb": m.wired >> 20,
        "compressed_mb": m.compressed >> 20, "cached_mb": m.cached >> 20, "free_mb": m.free >> 20,
        "swap_used_mb": m.swap_used >> 20, "pressure": probe::pressure_name(m.pressure), "headroom_mb": headroom_mb(s)
    })
}

fn sum_mb(fs: &[Finding], safety: &str) -> u64 {
    fs.iter()
        .filter(|f| f.safety == safety && f.actionable())
        .map(|f| f.frees_mb)
        .sum()
}

fn check(cfg: &Config, need: i64) -> Result<Value, String> {
    let (s, fs) = findings(cfg)?;
    let head = headroom_mb(&s);
    let ok = need <= head && s.mem.pressure < 4;
    if ok {
        return Ok(
            json!({"ok": true, "need_mb": need, "headroom_mb": head, "memory": mem_view(&s), "advice": "Enough headroom. Go ahead."}),
        );
    }
    let gap = need - head;
    let mut plan: Vec<&Finding> = Vec::new();
    let mut planned = 0i64;
    for tier in [SAFE, CONFIRM] {
        for f in fs.iter().filter(|f| f.safety == tier && f.actionable()) {
            if planned < gap {
                plan.push(f);
                planned += f.frees_mb as i64;
            }
        }
    }
    Ok(json!({
        "ok": false, "need_mb": need, "headroom_mb": head, "gap_mb": gap, "memory": mem_view(&s),
        "plan": plan, "plan_frees_mb": planned,
        "advice": "Not enough headroom. Free the safe items in plan with memory_free; ask the user before any confirm item."
    }))
}

fn free(cfg: &Config, ids: &[String], confirm: bool, force: bool) -> Result<Value, String> {
    if ids.is_empty() {
        return Err("ids is required (finding ids, rule names, or \"safe\")".into());
    }
    let (s, fs) = findings(cfg)?;
    let before = s.mem.used;
    let mut results = Vec::new();
    let mut need_confirm: Vec<&Finding> = Vec::new();
    for f in select(&fs, ids) {
        if f.safety == CONFIRM && !confirm {
            need_confirm.push(f);
        } else {
            results.push(act::execute(f, force));
        }
    }
    let mut out = json!({"results": results});
    if !need_confirm.is_empty() {
        out["needs_confirmation"] = json!(need_confirm);
        out["note"] = json!("These stop things a human may be using. Ask the user, then call again with confirm=true.");
    }
    if !results.is_empty() {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(after) = analyze::take(Duration::ZERO) {
            out["used_dropped_mb"] = json!(before.saturating_sub(after.mem.used) >> 20);
            out["memory_after"] = mem_view(&after);
        }
    }
    Ok(out)
}

/// Pick findings by id, rule name, tier ("safe", "confirm") or "all".
pub fn select<'a>(fs: &'a [Finding], ids: &[String]) -> Vec<&'a Finding> {
    fs.iter()
        .filter(|f| f.actionable())
        .filter(|f| {
            ids.iter()
                .any(|id| id == &f.id || id == &f.rule || id == f.safety || id == "all")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_and_errors() {
        let init = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}), "t").unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("memory_check"));
        let future = handle(
            &json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}),
            "t",
        )
        .unwrap();
        assert_eq!(future["result"]["protocolVersion"], LATEST_PROTOCOL);
        let disc = handle(
            &json!({"jsonrpc":"2.0","id":2,"method":"server/discover"}),
            "t",
        )
        .unwrap();
        assert_eq!(disc["result"]["serverInfo"]["name"], "ramster");
        assert!(handle(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            "t"
        )
        .is_none());
        let list = handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}), "t").unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "memory_status",
                "memory_check",
                "memory_suggest",
                "memory_free",
                "memory_report"
            ]
        );
        let bad = handle(&json!({"jsonrpc":"2.0","id":4,"method":"nope"}), "t").unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }
}
