//! Small helpers: time, formatting, subprocesses. Kept dependency-free on purpose.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Local broken-down time (year, month 1-12, day, hour, minute).
pub fn local(t: i64) -> (i32, u32, u32, u32, u32) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    (
        tm.tm_year + 1900,
        tm.tm_mon as u32 + 1,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

pub fn local_date(t: i64) -> String {
    let (y, m, d, _, _) = local(t);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn local_hour(t: i64) -> usize {
    local(t).3 as usize
}

pub fn fmt_stamp(t: i64) -> String {
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, m, d, h, mi) = local(t);
    format!("{} {} {:02}:{:02}", MON[(m as usize - 1) % 12], d, h, mi)
}

pub fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

pub fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
}

pub fn fmt_minutes(m: f64) -> String {
    let mi = m as u64;
    if m < 1.0 {
        "<1m".into()
    } else if mi < 60 {
        format!("{mi}m")
    } else if mi < 24 * 60 {
        format!("{}h{:02}m", mi / 60, mi % 60)
    } else {
        format!("{}d{}h", mi / 1440, mi % 1440 / 60)
    }
}

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

pub fn tildify(p: &str) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Some(rest) if !h.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

pub fn base_name(s: &str) -> &str {
    s.trim_end_matches('/').rsplit('/').next().unwrap_or(s)
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Run a command with a timeout; returns (success, combined trimmed output).
/// Output is drained while the command runs, so a chatty command can't block
/// on a full pipe.
pub fn run(timeout: Duration, cmd: &str, args: &[&str]) -> (bool, String) {
    let child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return (false, format!("cannot run {cmd}"));
    };
    let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    let drain = |r: Option<Box<dyn std::io::Read + Send>>, which: u8| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send((which, buf));
        });
    };
    drain(child.stdout.take().map(|o| Box::new(o) as _), 0);
    drain(child.stderr.take().map(|e| Box::new(e) as _), 1);
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // Collect what was written. A background grandchild may keep a pipe open,
    // so don't wait for EOF forever.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let collect_by = Instant::now() + Duration::from_secs(2);
    for _ in 0..2 {
        let left = collect_by.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((0, b)) => out = b,
            Ok((_, b)) => err = b,
            Err(_) => break,
        }
    }
    let Some(status) = status else {
        return (false, format!("{cmd} timed out"));
    };
    let mut s = String::from_utf8_lossy(&out).into_owned();
    s.push_str(&String::from_utf8_lossy(&err));
    (status.success(), s.trim().to_string())
}

pub fn which(name: &str, fallbacks: &[&str]) -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = format!("{dir}/{name}");
            if std::path::Path::new(&p).is_file() {
                return Some(p);
            }
        }
    }
    fallbacks
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .map(|s| s.to_string())
}

/// Ask a yes/no question on the terminal; anything but y/yes is no.
pub fn ask(q: &str) -> bool {
    print!("{q} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting() {
        assert_eq!(slug("Google Chrome"), "google-chrome");
        assert_eq!(slug("firebase-tools mcp"), "firebase-tools-mcp");
        assert_eq!(fmt_mb(512), "512 MB");
        assert_eq!(fmt_mb(2048), "2.0 GB");
        assert_eq!(fmt_minutes(130.0), "2h10m");
        assert_eq!(fmt_minutes(2580.0), "1d19h");
        assert_eq!(base_name("/opt/homebrew/bin/firebase"), "firebase");
    }

    #[test]
    fn run_drains_large_output() {
        // 1 MB on stdout would block on a full pipe if it were read only at exit.
        let t0 = Instant::now();
        let (ok, out) = run(
            Duration::from_secs(10),
            "/bin/sh",
            &[
                "-c",
                "head -c 1048576 /dev/zero | tr '\\0' x; echo done >&2",
            ],
        );
        assert!(ok, "{}", &out[out.len().saturating_sub(100)..]);
        assert!(out.len() >= 1 << 20 && out.ends_with("done"));
        assert!(t0.elapsed() < Duration::from_secs(5));
        let (ok, out) = run(Duration::from_millis(200), "/bin/sleep", &["5"]);
        assert!(!ok && out.contains("timed out"));
    }
}
