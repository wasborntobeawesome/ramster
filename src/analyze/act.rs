//! Executing findings. Processes are re-verified by start time so a recycled PID is never signalled.

use serde::Serialize;
use std::time::{Duration, Instant};

use super::rules::{Action, Finding};
use crate::probe;
use crate::util::run;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub id: String,
    pub ok: bool,
    pub estimated_freed_mb: u64,
    pub message: String,
}

pub fn execute(f: &Finding, force: bool) -> Outcome {
    let mut r = Outcome {
        id: f.id.clone(),
        ..Default::default()
    };
    if !f.actionable() {
        r.message = "advice only, nothing to run".into();
        return r;
    }
    match &f.act {
        Action::Term => return terminate(f, force),
        Action::Cmd(cmd) => {
            let args: Vec<&str> = cmd[1..].iter().map(String::as_str).collect();
            let (ok, out) = run(Duration::from_secs(90), &cmd[0], &args);
            if !ok {
                r.message = format!("{} failed: {out}", cmd.join(" "));
                return r;
            }
            r.message = format!("ran: {}", cmd.join(" "));
        }
        #[cfg(not(target_os = "macos"))]
        Action::Quit(app) => {
            // No AppleScript: ask the app's top-level processes to exit, as closing
            // its windows would. Helpers follow their parent.
            let set: std::collections::HashSet<i32> = f.pids.iter().copied().collect();
            let mut top = f.clone();
            top.pids.retain(|&p| !set.contains(&probe::parent_of(p)));
            let mut o = terminate(&top, force);
            if o.ok {
                o.message = format!("asked {app} to quit (SIGTERM)");
            }
            return o;
        }
        #[cfg(target_os = "macos")]
        Action::Quit(app) => {
            let script = format!("tell application {} to quit", applescript_str(app));
            let (ok, out) = run(Duration::from_secs(30), "osascript", &["-e", &script]);
            if !ok {
                r.message = format!("could not quit {app}: {out}");
                return r;
            }
            r.message = format!("asked {app} to quit");
        }
        Action::None => unreachable!(),
    }
    r.ok = true;
    r.estimated_freed_mb = f.frees_mb;
    r
}

#[cfg(target_os = "macos")]
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn terminate(f: &Finding, force: bool) -> Outcome {
    let mut r = Outcome {
        id: f.id.clone(),
        ..Default::default()
    };
    let targets: Vec<i32> = f
        .pids
        .iter()
        .copied()
        .filter(|&pid| {
            let got = probe::start_time(pid);
            if got < 0 {
                return false; // already gone
            }
            match f.starts.get(&pid) {
                Some(&want) if want > 0 && got > 0 => got == want, // else: PID recycled
                _ => true,
            }
        })
        .collect();
    if targets.is_empty() {
        r.ok = true;
        r.message = "already gone".into();
        return r;
    }
    for &pid in &targets {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let mut alive = wait_gone(&targets, Duration::from_secs(8));
    if !alive.is_empty() && force {
        for &pid in &alive {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        alive = wait_gone(&alive, Duration::from_secs(2));
    }
    if !alive.is_empty() {
        r.ok = alive.len() < targets.len();
        r.message = format!(
            "{} of {} processes still running (retry with --force to SIGKILL)",
            alive.len(),
            targets.len()
        );
        return r;
    }
    r.ok = true;
    r.estimated_freed_mb = f.frees_mb;
    r.message = format!("stopped {} process(es)", targets.len());
    r
}

fn wait_gone(pids: &[i32], d: Duration) -> Vec<i32> {
    let deadline = Instant::now() + d;
    loop {
        let alive: Vec<i32> = pids
            .iter()
            .copied()
            .filter(|&p| probe::start_time(p) >= 0)
            .collect();
        if alive.is_empty() || Instant::now() > deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
